//! Package-declared user settings: the `[[settings]]` manifest section, value
//! validation against it, and the per-installation value store.
//!
//! A setting is a typed, non-secret user preference a package declares
//! (`switch | choice | text | number`). The host owns its storage and
//! validation; the package only receives current values through the mediated
//! invocation context. Settings are never secrets: a declaration that looks
//! like a credential slot is rejected at parse time and pointed at the
//! credential-binding path (`[[tools.credentials]]` + `[auth.<vendor>]`).
//!
//! Values live per user scope under the `/extension-settings` mount, one
//! record per extension, mutated only through the bounded CAS helper. Removing
//! the extension clears its record ([`ExtensionSettingsStore::clear`]), so a
//! reinstall starts from the declared defaults.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_filesystem::{
    CasApply, CasUpdateError, ContentType, Entry, FilesystemError, RecordKind, RootFilesystem,
    ScopedFilesystem, cas_update,
};
use ironclaw_host_api::{ids::ExtensionId, path::ScopedPath, resource::ResourceScope};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Most settings one manifest may declare.
pub const MAX_EXTENSION_SETTINGS: usize = 64;
/// Most options one `choice` setting may declare.
pub const MAX_SETTING_OPTIONS: usize = 64;
/// Longest setting id, in bytes.
pub const MAX_SETTING_ID_BYTES: usize = 64;
/// Longest label / option label, in characters.
pub const MAX_SETTING_LABEL_CHARS: usize = 120;
/// Longest description, in characters.
pub const MAX_SETTING_DESCRIPTION_CHARS: usize = 1_000;
/// Longest section name, in characters.
pub const MAX_SETTING_SECTION_CHARS: usize = 80;
/// Longest choice option value, in bytes.
pub const MAX_SETTING_OPTION_VALUE_BYTES: usize = 128;
/// Longest `text` value, in bytes.
pub const MAX_SETTING_TEXT_VALUE_BYTES: usize = 2_048;

const SETTINGS_RECORD_KIND: &str = "extension_settings";
const SETTINGS_RECORD_SCHEMA: &str = "ironclaw.extension_settings.v1";
const SETTINGS_PREFIX: &str = "/extension-settings";
const MAX_SETTINGS_RECORD_BYTES: usize = 256 * 1024;

/// Substrings that mark a declaration as a credential slot. Matched against
/// the lower-cased id and label.
const CREDENTIAL_MARKERS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "secret",
    "token",
    "api_key",
    "api-key",
    "apikey",
    "api key",
    "credential",
    "private_key",
    "private key",
    "access_key",
    "access key",
];

/// Manifest-local setting identity, e.g. `units` or `notify.on_failure`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExtensionSettingId(String);

impl ExtensionSettingId {
    fn validate(value: &str) -> Result<(), ExtensionSettingsError> {
        let valid = !value.is_empty()
            && value.len() <= MAX_SETTING_ID_BYTES
            && value
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
            && value.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || matches!(character, '_' | '.' | '-')
            });
        if valid {
            Ok(())
        } else {
            Err(ExtensionSettingsError::InvalidId {
                id: value.chars().take(MAX_SETTING_ID_BYTES).collect(),
            })
        }
    }

    pub fn new(value: impl Into<String>) -> Result<Self, ExtensionSettingsError> {
        let value = value.into();
        Self::validate(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ExtensionSettingId {
    type Error = ExtensionSettingsError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ExtensionSettingId> for String {
    fn from(id: ExtensionSettingId) -> Self {
        id.0
    }
}

impl std::fmt::Display for ExtensionSettingId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The closed setting vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSettingKind {
    Switch,
    Choice,
    Text,
    Number,
}

impl ExtensionSettingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Switch => "switch",
            Self::Choice => "choice",
            Self::Text => "text",
            Self::Number => "number",
        }
    }
}

/// One option of a `choice` setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSettingOption {
    pub value: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One validated `[[settings]]` declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSettingDecl {
    pub id: ExtensionSettingId,
    pub kind: ExtensionSettingKind,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ExtensionSettingOption>,
    pub default: Value,
}

impl ExtensionSettingDecl {
    /// Check `value` against this declaration. The error reason is plain,
    /// user-facing text naming the setting; it never echoes the value.
    pub fn validate_value(&self, value: &Value) -> Result<(), ExtensionSettingsError> {
        let reject = |reason: String| ExtensionSettingsError::InvalidValue {
            id: self.id.as_str().to_string(),
            reason,
        };
        match self.kind {
            ExtensionSettingKind::Switch => {
                if !value.is_boolean() {
                    return Err(reject("expected true or false".to_string()));
                }
            }
            ExtensionSettingKind::Number => {
                if !value.is_number() {
                    return Err(reject("expected a number".to_string()));
                }
            }
            ExtensionSettingKind::Text => {
                let Some(text) = value.as_str() else {
                    return Err(reject("expected text".to_string()));
                };
                if text.len() > MAX_SETTING_TEXT_VALUE_BYTES {
                    return Err(reject(format!(
                        "text is longer than {MAX_SETTING_TEXT_VALUE_BYTES} bytes"
                    )));
                }
                if text
                    .chars()
                    .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
                {
                    return Err(reject("text contains control characters".to_string()));
                }
            }
            ExtensionSettingKind::Choice => {
                let Some(choice) = value.as_str() else {
                    return Err(reject("expected one of the listed options".to_string()));
                };
                if !self.options.iter().any(|option| option.value == choice) {
                    let allowed = self
                        .options
                        .iter()
                        .map(|option| format!("`{}`", option.value))
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(reject(format!("expected one of {allowed}")));
                }
            }
        }
        Ok(())
    }

    fn validate_declaration(&self) -> Result<(), ExtensionSettingsError> {
        let invalid = |reason: String| ExtensionSettingsError::InvalidDeclaration {
            id: self.id.as_str().to_string(),
            reason,
        };
        if looks_like_credential(self.id.as_str()) || looks_like_credential(&self.label) {
            return Err(ExtensionSettingsError::CredentialSlot {
                id: self.id.as_str().to_string(),
            });
        }
        check_text("label", &self.label, MAX_SETTING_LABEL_CHARS, true).map_err(invalid)?;
        check_text(
            "description",
            &self.description,
            MAX_SETTING_DESCRIPTION_CHARS,
            false,
        )
        .map_err(invalid)?;
        if let Some(section) = &self.section {
            check_text("section", section, MAX_SETTING_SECTION_CHARS, true).map_err(invalid)?;
        }
        match self.kind {
            ExtensionSettingKind::Choice => {
                if self.options.is_empty() {
                    return Err(invalid(
                        "a `choice` setting must declare at least one option".to_string(),
                    ));
                }
                if self.options.len() > MAX_SETTING_OPTIONS {
                    return Err(invalid(format!(
                        "a `choice` setting may declare at most {MAX_SETTING_OPTIONS} options"
                    )));
                }
                let mut seen = BTreeSet::new();
                for option in &self.options {
                    if option.value.is_empty()
                        || option.value.len() > MAX_SETTING_OPTION_VALUE_BYTES
                        || option.value.chars().any(char::is_control)
                    {
                        return Err(invalid(format!(
                            "option values must be 1..={MAX_SETTING_OPTION_VALUE_BYTES} bytes \
                             without control characters"
                        )));
                    }
                    if !seen.insert(option.value.as_str()) {
                        return Err(invalid(format!(
                            "option value `{}` is declared twice",
                            option.value
                        )));
                    }
                    check_text("option label", &option.label, MAX_SETTING_LABEL_CHARS, true)
                        .map_err(invalid)?;
                    if let Some(description) = &option.description {
                        check_text(
                            "option description",
                            description,
                            MAX_SETTING_DESCRIPTION_CHARS,
                            false,
                        )
                        .map_err(invalid)?;
                    }
                }
            }
            _ => {
                if !self.options.is_empty() {
                    return Err(invalid(format!(
                        "only a `choice` setting may declare options, not `{}`",
                        self.kind.as_str()
                    )));
                }
            }
        }
        self.validate_value(&self.default)
            .map_err(|error| match error {
                ExtensionSettingsError::InvalidValue { reason, .. } => {
                    invalid(format!("default is invalid: {reason}"))
                }
                other => other,
            })
    }
}

fn check_text(field: &str, value: &str, max_chars: usize, required: bool) -> Result<(), String> {
    if required && value.trim().is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.chars().count() > max_chars {
        return Err(format!("{field} is longer than {max_chars} characters"));
    }
    if value
        .chars()
        .any(|character| character.is_control() && character != '\n')
    {
        return Err(format!("{field} contains control characters"));
    }
    Ok(())
}

fn looks_like_credential(text: &str) -> bool {
    let lowered = text.to_ascii_lowercase();
    CREDENTIAL_MARKERS
        .iter()
        .any(|marker| lowered.contains(marker))
}

/// Validate a manifest's whole `[[settings]]` array: count bound, unique ids,
/// and every declaration's own invariants.
pub fn validate_setting_declarations(
    settings: &[ExtensionSettingDecl],
) -> Result<(), ExtensionSettingsError> {
    if settings.len() > MAX_EXTENSION_SETTINGS {
        return Err(ExtensionSettingsError::TooMany {
            count: settings.len(),
            max: MAX_EXTENSION_SETTINGS,
        });
    }
    let mut seen = BTreeSet::new();
    for setting in settings {
        if !seen.insert(setting.id.as_str()) {
            return Err(ExtensionSettingsError::DuplicateId {
                id: setting.id.as_str().to_string(),
            });
        }
        setting.validate_declaration()?;
    }
    Ok(())
}

/// The effective value of every declared setting: the stored value when it is
/// still valid against the current declaration, otherwise the default. A
/// stored value for an id the manifest no longer declares is ignored, so an
/// upgrade that changes a setting's shape degrades to its new default instead
/// of handing the package a value it cannot read.
pub fn effective_setting_values(
    settings: &[ExtensionSettingDecl],
    stored: &BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    settings
        .iter()
        .map(|setting| {
            let value = stored
                .get(setting.id.as_str())
                .filter(|value| setting.validate_value(value).is_ok())
                .cloned()
                .unwrap_or_else(|| setting.default.clone());
            (setting.id.as_str().to_string(), value)
        })
        .collect()
}

/// Settings declaration and storage errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ExtensionSettingsError {
    #[error(
        "setting id `{id}` is invalid: use 1..=64 lowercase ASCII letters, digits, `_`, `.` or `-`"
    )]
    InvalidId { id: String },
    #[error("setting `{id}` is declared twice")]
    DuplicateId { id: String },
    #[error("a manifest may declare at most {max} settings, found {count}")]
    TooMany { count: usize, max: usize },
    #[error("setting `{id}` is invalid: {reason}")]
    InvalidDeclaration { id: String, reason: String },
    #[error(
        "setting `{id}` looks like a credential slot; settings are stored as plain, non-secret \
         values. Declare the credential with a `[[tools.credentials]]` handle backed by an \
         `[auth.<vendor>]` recipe instead"
    )]
    CredentialSlot { id: String },
    #[error("`{id}`: {reason}")]
    InvalidValue { id: String, reason: String },
    #[error("unknown setting `{id}`")]
    UnknownSetting { id: String },
    #[error("extension settings record is invalid")]
    InvalidRecord,
    #[error("extension settings storage failed during {operation}: {reason}")]
    Storage {
        operation: &'static str,
        reason: String,
    },
}

/// The durable per-extension record. Values are keyed by setting id and hold
/// only what the user explicitly set; defaults are never persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct StoredExtensionSettings {
    schema: String,
    extension_id: String,
    values: BTreeMap<String, Value>,
}

/// Read access to stored setting values for one caller scope, the port the
/// runtime lanes use to attach current values to an invocation context.
#[async_trait]
pub trait ExtensionSettingsReader: Send + Sync {
    /// Values the user explicitly stored for `extension_id` in `scope`
    /// (empty when none). Callers resolve effective values against the
    /// manifest with [`effective_setting_values`].
    async fn stored_values(
        &self,
        scope: &ResourceScope,
        extension_id: &ExtensionId,
    ) -> Result<BTreeMap<String, Value>, ExtensionSettingsError>;
}

/// The reader used where no settings store is wired: every package sees its
/// declared defaults.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultsOnlyExtensionSettings;

#[async_trait]
impl ExtensionSettingsReader for DefaultsOnlyExtensionSettings {
    async fn stored_values(
        &self,
        _scope: &ResourceScope,
        _extension_id: &ExtensionId,
    ) -> Result<BTreeMap<String, Value>, ExtensionSettingsError> {
        Ok(BTreeMap::new())
    }
}

/// Thin typed store over the scoped `/extension-settings` mount.
pub struct ExtensionSettingsStore<F: RootFilesystem + ?Sized> {
    filesystem: Arc<ScopedFilesystem<F>>,
}

impl<F: RootFilesystem + ?Sized> Clone for ExtensionSettingsStore<F> {
    fn clone(&self) -> Self {
        Self {
            filesystem: Arc::clone(&self.filesystem),
        }
    }
}

impl<F: RootFilesystem + ?Sized> std::fmt::Debug for ExtensionSettingsStore<F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ExtensionSettingsStore").finish()
    }
}

impl<F: RootFilesystem + ?Sized> ExtensionSettingsStore<F> {
    pub fn new(filesystem: Arc<ScopedFilesystem<F>>) -> Self {
        Self { filesystem }
    }

    async fn load(
        &self,
        scope: &ResourceScope,
        extension_id: &ExtensionId,
    ) -> Result<BTreeMap<String, Value>, ExtensionSettingsError> {
        let path = record_path(extension_id)?;
        let versioned = match self.filesystem.get(scope, &path).await {
            Ok(versioned) => versioned,
            Err(FilesystemError::NotFound { .. }) => None,
            Err(error) => return Err(storage_error("get", &error)),
        };
        let Some(versioned) = versioned else {
            return Ok(BTreeMap::new());
        };
        let record = decode_record(&versioned.entry.body)?;
        if record.extension_id != extension_id.as_str() {
            return Err(ExtensionSettingsError::InvalidRecord);
        }
        Ok(record.values)
    }

    /// Validate `value` against `settings` and persist it for one setting.
    /// Returns the effective value now stored.
    pub async fn set(
        &self,
        scope: &ResourceScope,
        extension_id: &ExtensionId,
        settings: &[ExtensionSettingDecl],
        setting_id: &str,
        value: Value,
    ) -> Result<Value, ExtensionSettingsError> {
        let declaration = settings
            .iter()
            .find(|setting| setting.id.as_str() == setting_id)
            .ok_or_else(|| ExtensionSettingsError::UnknownSetting {
                id: setting_id.chars().take(MAX_SETTING_ID_BYTES).collect(),
            })?;
        declaration.validate_value(&value)?;
        let path = record_path(extension_id)?;
        let extension = extension_id.as_str().to_string();
        let key = declaration.id.as_str().to_string();
        cas_update(
            self.filesystem.as_ref(),
            scope,
            &path,
            decode_record,
            encode_record,
            |current: Option<StoredExtensionSettings>| {
                let outcome = (|| {
                    let mut record = current.unwrap_or_else(|| StoredExtensionSettings {
                        schema: SETTINGS_RECORD_SCHEMA.to_string(),
                        extension_id: extension.clone(),
                        values: BTreeMap::new(),
                    });
                    if record.extension_id != extension {
                        return Err(ExtensionSettingsError::InvalidRecord);
                    }
                    // Drop values for ids the manifest no longer declares so
                    // the record stays bounded by the current schema.
                    record
                        .values
                        .retain(|id, _| settings.iter().any(|setting| setting.id.as_str() == id));
                    record.values.insert(key.clone(), value.clone());
                    Ok(CasApply::new(record, value.clone()))
                })();
                async move { outcome }
            },
        )
        .await
        .map_err(map_cas_error)
    }

    /// Forget every stored value for `extension_id` in `scope`. Called by the
    /// removal lifecycle so a reinstall starts from defaults.
    pub async fn clear(
        &self,
        scope: &ResourceScope,
        extension_id: &ExtensionId,
    ) -> Result<(), ExtensionSettingsError> {
        let path = record_path(extension_id)?;
        match self.filesystem.delete(scope, &path).await {
            Ok(()) | Err(FilesystemError::NotFound { .. }) => Ok(()),
            Err(error) => Err(storage_error("delete", &error)),
        }
    }
}

/// Forgets a user's stored values for one extension — the removal
/// lifecycle's hook into the settings store. Settings are configuration, not
/// LLM data, so removal clears them: a reinstall starts from defaults.
#[async_trait]
pub trait ExtensionSettingsCleaner: Send + Sync {
    async fn clear_settings(
        &self,
        scope: &ResourceScope,
        extension_id: &ExtensionId,
    ) -> Result<(), ExtensionSettingsError>;
}

#[async_trait]
impl<F: RootFilesystem + ?Sized> ExtensionSettingsCleaner for ExtensionSettingsStore<F> {
    async fn clear_settings(
        &self,
        scope: &ResourceScope,
        extension_id: &ExtensionId,
    ) -> Result<(), ExtensionSettingsError> {
        self.clear(scope, extension_id).await
    }
}

#[async_trait]
impl<F: RootFilesystem + ?Sized> ExtensionSettingsReader for ExtensionSettingsStore<F> {
    async fn stored_values(
        &self,
        scope: &ResourceScope,
        extension_id: &ExtensionId,
    ) -> Result<BTreeMap<String, Value>, ExtensionSettingsError> {
        self.load(scope, extension_id).await
    }
}

fn record_path(extension_id: &ExtensionId) -> Result<ScopedPath, ExtensionSettingsError> {
    ScopedPath::new(format!("{SETTINGS_PREFIX}/{}.json", extension_id.as_str())).map_err(|error| {
        ExtensionSettingsError::Storage {
            operation: "path",
            reason: error.to_string(),
        }
    })
}

fn decode_record(bytes: &[u8]) -> Result<StoredExtensionSettings, ExtensionSettingsError> {
    if bytes.len() > MAX_SETTINGS_RECORD_BYTES {
        return Err(ExtensionSettingsError::InvalidRecord);
    }
    let record: StoredExtensionSettings = serde_json::from_slice(bytes).map_err(|error| {
        tracing::debug!(%error, "extension settings record failed to decode");
        ExtensionSettingsError::InvalidRecord
    })?;
    if record.schema != SETTINGS_RECORD_SCHEMA {
        return Err(ExtensionSettingsError::InvalidRecord);
    }
    Ok(record)
}

fn encode_record(record: &StoredExtensionSettings) -> Result<Entry, ExtensionSettingsError> {
    let body = serde_json::to_vec(record).map_err(|error| ExtensionSettingsError::Storage {
        operation: "encode",
        reason: error.to_string(),
    })?;
    if body.len() > MAX_SETTINGS_RECORD_BYTES {
        return Err(ExtensionSettingsError::InvalidRecord);
    }
    let kind =
        RecordKind::new(SETTINGS_RECORD_KIND).map_err(|error| ExtensionSettingsError::Storage {
            operation: "encode",
            reason: error.to_string(),
        })?;
    let mut entry = Entry::bytes(body).with_content_type(ContentType::json());
    entry.kind = Some(kind);
    Ok(entry)
}

fn storage_error(operation: &'static str, error: &FilesystemError) -> ExtensionSettingsError {
    ExtensionSettingsError::Storage {
        operation,
        reason: error.to_string(),
    }
}

fn map_cas_error(error: CasUpdateError<ExtensionSettingsError>) -> ExtensionSettingsError {
    match error {
        CasUpdateError::Apply(error) => error,
        other => ExtensionSettingsError::Storage {
            operation: "set",
            reason: other.to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(toml_text: &str) -> Result<ExtensionSettingDecl, String> {
        toml::from_str::<ExtensionSettingDecl>(toml_text).map_err(|error| error.to_string())
    }

    #[test]
    fn choice_requires_options_and_a_listed_default() {
        let setting = decl(
            r#"
id = "units"
kind = "choice"
label = "Units"
default = "metric"
options = [{ value = "metric", label = "Metric" }, { value = "imperial", label = "Imperial" }]
"#,
        )
        .unwrap();
        validate_setting_declarations(std::slice::from_ref(&setting)).unwrap();
        assert!(setting.validate_value(&Value::from("imperial")).is_ok());
        let error = setting.validate_value(&Value::from("kelvin")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "`units`: expected one of `metric`, `imperial`"
        );

        let no_options =
            decl("id = \"units\"\nkind = \"choice\"\nlabel = \"Units\"\ndefault = \"metric\"")
                .unwrap();
        assert!(matches!(
            validate_setting_declarations(&[no_options]),
            Err(ExtensionSettingsError::InvalidDeclaration { .. })
        ));
    }

    #[test]
    fn defaults_must_match_their_kind() {
        let bad =
            decl("id = \"verbose\"\nkind = \"switch\"\nlabel = \"Verbose\"\ndefault = \"yes\"")
                .unwrap();
        let error = validate_setting_declarations(&[bad]).unwrap_err();
        assert_eq!(
            error.to_string(),
            "setting `verbose` is invalid: default is invalid: expected true or false"
        );
    }

    #[test]
    fn credential_shaped_settings_are_rejected() {
        for (id, label) in [
            ("api_key", "Key"),
            ("account", "Account password"),
            ("github_token", "GitHub"),
        ] {
            let setting = ExtensionSettingDecl {
                id: ExtensionSettingId::new(id).unwrap(),
                kind: ExtensionSettingKind::Text,
                label: label.to_string(),
                description: String::new(),
                section: None,
                options: Vec::new(),
                default: Value::from(""),
            };
            let error = validate_setting_declarations(&[setting]).unwrap_err();
            assert!(matches!(
                error,
                ExtensionSettingsError::CredentialSlot { .. }
            ));
            assert!(error.to_string().contains("[[tools.credentials]]"));
        }
    }

    #[test]
    fn options_on_non_choice_and_duplicates_are_rejected() {
        let switch = decl(
            "id = \"a\"\nkind = \"switch\"\nlabel = \"A\"\ndefault = true\noptions = [{ value = \"x\", label = \"X\" }]",
        )
        .unwrap();
        assert!(validate_setting_declarations(&[switch]).is_err());
        let one = decl("id = \"a\"\nkind = \"number\"\nlabel = \"A\"\ndefault = 1").unwrap();
        assert!(matches!(
            validate_setting_declarations(&[one.clone(), one]),
            Err(ExtensionSettingsError::DuplicateId { .. })
        ));
        assert!(ExtensionSettingId::new("Bad Id").is_err());
    }

    #[test]
    fn effective_values_fall_back_to_defaults_for_stale_entries() {
        let units = decl(
            "id = \"units\"\nkind = \"choice\"\nlabel = \"Units\"\ndefault = \"metric\"\noptions = [{ value = \"metric\", label = \"Metric\" }]",
        )
        .unwrap();
        let limit =
            decl("id = \"limit\"\nkind = \"number\"\nlabel = \"Limit\"\ndefault = 5").unwrap();
        let stored = BTreeMap::from([
            ("units".to_string(), Value::from("imperial")),
            ("limit".to_string(), Value::from(9)),
            ("gone".to_string(), Value::from(true)),
        ]);
        let effective = effective_setting_values(&[units, limit], &stored);
        assert_eq!(
            effective,
            BTreeMap::from([
                ("limit".to_string(), Value::from(9)),
                ("units".to_string(), Value::from("metric")),
            ])
        );
    }
}
