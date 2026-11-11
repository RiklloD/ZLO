//! Remote-access records of one installation (an "environment"): its
//! permanent id, one-time pairing codes, and per-device grants.
//!
//! Implements [`EnvironmentAccessService`] over the control-plane
//! `ScopedFilesystem` mount the rest of this crate rides. These records are
//! principal identity — "which device is allowed to act as the operator" —
//! so they sit beside the user and project records, out of the agent's
//! reach. Layout:
//!
//! ```text
//! /tenant-shared/reborn-environment/environment.json
//! /tenant-shared/reborn-environment/pairing-codes/<sha256(code)>.json
//! /tenant-shared/reborn-environment/devices/<device_id>.json
//! ```
//!
//! Secrets are never stored: a pairing code is kept as the SHA-256 of its
//! canonical form (it is the file name), a device token as the SHA-256 of
//! the whole token. Every mutation of an existing record goes through the
//! shared bounded [`cas_update`] helper, so two concurrent redemptions of one
//! code cannot both observe it unused. Revoked devices are kept with
//! `revoked_at` set; nothing here deletes a record.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use ironclaw_filesystem::{
    CasApply, CasExpectation, CasUpdateError, ContentType, Entry, FileType, FilesystemError,
    RootFilesystem, ScopedFilesystem, cas_update,
};
use ironclaw_host_api::{
    ids::{AgentId, InvocationId, ProjectId, TenantId, UserId},
    path::ScopedPath,
    resource::ResourceScope,
};
use ironclaw_product_contracts::environment_access::{
    DEVICE_TOKEN_PREFIX, DeviceId, DeviceList, DevicePairedBy, DeviceSummary,
    EnvironmentAccessError, EnvironmentAccessService, EnvironmentId, EnvironmentIdentity,
    PAIRING_CODE_ALPHABET, PAIRING_CODE_LEN, PairClient, PairedDevice, PairingCodeIssued,
    display_pairing_code, normalize_pairing_code,
};
use rand::RngExt as _;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// Scoped root of this installation's environment identity, pairing codes
/// and device grants. Public so a setup export can leave these records out:
/// a moved setup must get its own identity and devices on the new machine.
pub const ENVIRONMENT_ACCESS_ROOT: &str = "/tenant-shared/reborn-environment";
const ENVIRONMENT_ROOT: &str = ENVIRONMENT_ACCESS_ROOT;
/// How long an issued pairing code stays redeemable.
pub const PAIRING_CODE_TTL: Duration = Duration::minutes(5);
/// `last_seen_at` is rewritten at most this often per device.
const LAST_SEEN_RESOLUTION: Duration = Duration::minutes(1);
/// Bound on client-supplied label / os / app_version, in characters.
const MAX_CLIENT_FIELD_CHARS: usize = 128;
/// Bound on how many device records one listing reads.
const MAX_LISTED_DEVICES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredEnvironment {
    environment_id: EnvironmentId,
    label: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredPairingCode {
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    consumed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredPairedBy {
    OneTimeCode,
    TailnetIdentity,
}

impl From<StoredPairedBy> for DevicePairedBy {
    fn from(value: StoredPairedBy) -> Self {
        match value {
            StoredPairedBy::OneTimeCode => DevicePairedBy::OneTimeCode,
            StoredPairedBy::TailnetIdentity => DevicePairedBy::TailnetIdentity,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoredDevice {
    device_id: DeviceId,
    label: String,
    os: String,
    app_version: String,
    /// Lowercase hex SHA-256 of the whole `icd_…` token.
    token_hash: String,
    created_at: DateTime<Utc>,
    last_seen_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    paired_by: StoredPairedBy,
    tailnet_login: Option<String>,
}

/// [`EnvironmentAccessService`] over a [`ScopedFilesystem`].
pub struct FilesystemEnvironmentAccess<F>
where
    F: RootFilesystem + 'static,
{
    filesystem: Arc<ScopedFilesystem<F>>,
    scope: ResourceScope,
}

impl<F> FilesystemEnvironmentAccess<F>
where
    F: RootFilesystem + 'static,
{
    pub fn new(
        filesystem: Arc<ScopedFilesystem<F>>,
        tenant_id: TenantId,
        user_id: UserId,
        agent_id: AgentId,
        project_id: Option<ProjectId>,
    ) -> Self {
        Self {
            filesystem,
            scope: ResourceScope {
                tenant_id,
                user_id,
                agent_id: Some(agent_id),
                project_id,
                mission_id: None,
                thread_id: None,
                invocation_id: InvocationId::new(),
            },
        }
    }

    /// Create the environment record on first start (a new UUID and
    /// `default_label`), or return the existing one unchanged. Two racing
    /// first starts converge on whichever record was written first.
    pub async fn ensure_environment(
        &self,
        default_label: &str,
    ) -> Result<EnvironmentIdentity, EnvironmentAccessError> {
        let path = environment_path()?;
        if let Some(existing) = self.read_record::<StoredEnvironment>(&path).await? {
            return Ok(identity_of(existing));
        }
        let environment_id = EnvironmentId::new(uuid::Uuid::new_v4().to_string())
            .map_err(|error| unavailable("mint environment id", error))?;
        let record = StoredEnvironment {
            environment_id,
            label: bounded_client_field(default_label, "IronClaw"),
            created_at: Utc::now(),
        };
        match self
            .put_record(&path, &record, CasExpectation::Absent)
            .await
        {
            Ok(()) => Ok(identity_of(record)),
            Err(FilesystemError::VersionMismatch { .. }) => self
                .read_record::<StoredEnvironment>(&path)
                .await?
                .map(identity_of)
                .ok_or_else(|| unavailable("environment record", "vanished after conflict")),
            Err(error) => Err(unavailable("write environment record", error)),
        }
    }

    async fn read_record<T>(&self, path: &ScopedPath) -> Result<Option<T>, EnvironmentAccessError>
    where
        T: DeserializeOwned,
    {
        let Some(versioned) = self
            .filesystem
            .get(&self.scope, path)
            .await
            .map_err(|error| unavailable("read record", error))?
        else {
            return Ok(None);
        };
        serde_json::from_slice(&versioned.entry.body)
            .map(Some)
            .map_err(|error| unavailable("decode record", error))
    }

    async fn put_record<T>(
        &self,
        path: &ScopedPath,
        value: &T,
        cas: CasExpectation,
    ) -> Result<(), FilesystemError>
    where
        T: Serialize,
    {
        let body =
            serde_json::to_vec(value).map_err(|error| FilesystemError::BackendInfrastructure {
                operation: ironclaw_filesystem::FilesystemOperation::WriteFile,
                reason: format!("environment record could not be serialized: {error}"),
            })?;
        self.filesystem
            .put(
                &self.scope,
                path,
                Entry::bytes(body).with_content_type(ContentType::json()),
                cas,
            )
            .await
            .map(|_version| ())
    }

    /// Mint and persist a fresh device grant; returns the token once.
    async fn create_device(
        &self,
        client: PairClient,
        paired_by: StoredPairedBy,
        tailnet_login: Option<String>,
    ) -> Result<PairedDevice, EnvironmentAccessError> {
        let environment = self.environment().await?;
        let (device_id, token) = mint_device_token()?;
        let record = StoredDevice {
            device_id: device_id.clone(),
            label: bounded_client_field(&client.label, "Unnamed device"),
            os: bounded_client_field(&client.os, ""),
            app_version: bounded_client_field(&client.app_version, ""),
            token_hash: sha256_hex(token.as_bytes()),
            created_at: Utc::now(),
            last_seen_at: None,
            expires_at: None,
            revoked_at: None,
            paired_by,
            tailnet_login,
        };
        let path = device_path(&device_id)?;
        self.put_record(&path, &record, CasExpectation::Absent)
            .await
            .map_err(|error| unavailable("write device record", error))?;
        Ok(PairedDevice {
            environment_id: environment.environment_id.into_inner(),
            device_id: device_id.into_inner(),
            token,
        })
    }

    async fn mutate_device<M>(
        &self,
        device_id: &DeviceId,
        mutate: M,
    ) -> Result<StoredDevice, EnvironmentAccessError>
    where
        M: Fn(&mut StoredDevice) + Send + Sync,
    {
        let path = device_path(device_id)?;
        cas_update(
            &self.filesystem,
            &self.scope,
            &path,
            |bytes| {
                serde_json::from_slice::<StoredDevice>(bytes)
                    .map_err(|error| unavailable("decode device record", error))
            },
            encode_json::<StoredDevice>,
            |current: Option<StoredDevice>| {
                let mutate = &mutate;
                async move {
                    let Some(mut device) = current else {
                        return Err(EnvironmentAccessError::DeviceNotFound);
                    };
                    mutate(&mut device);
                    Ok(CasApply::new(device.clone(), device))
                }
            },
        )
        .await
        .map_err(|error| map_cas_error("update device record", error))
    }
}

#[async_trait]
impl<F> EnvironmentAccessService for FilesystemEnvironmentAccess<F>
where
    F: RootFilesystem + 'static,
{
    async fn environment(&self) -> Result<EnvironmentIdentity, EnvironmentAccessError> {
        let path = environment_path()?;
        self.read_record::<StoredEnvironment>(&path)
            .await?
            .map(identity_of)
            .ok_or_else(|| unavailable("environment record", "missing; ensure_environment not run"))
    }

    async fn issue_pairing_code(&self) -> Result<PairingCodeIssued, EnvironmentAccessError> {
        let canonical = mint_pairing_code();
        let now = Utc::now();
        let record = StoredPairingCode {
            created_at: now,
            expires_at: now + PAIRING_CODE_TTL,
            consumed_at: None,
        };
        let path = pairing_code_path(&canonical)?;
        self.put_record(&path, &record, CasExpectation::Absent)
            .await
            .map_err(|error| unavailable("write pairing code", error))?;
        Ok(PairingCodeIssued {
            code: display_pairing_code(&canonical),
            expires_at: record.expires_at,
        })
    }

    async fn redeem_pairing_code(
        &self,
        code: &str,
        client: PairClient,
    ) -> Result<PairedDevice, EnvironmentAccessError> {
        let canonical = normalize_pairing_code(code).ok_or(EnvironmentAccessError::InvalidCode)?;
        let path = pairing_code_path(&canonical)?;
        let now = Utc::now();
        // One CAS write consumes the code: a racer re-reads the consumed
        // record on retry and gets the same `InvalidCode` as any other miss.
        cas_update(
            &self.filesystem,
            &self.scope,
            &path,
            |bytes| {
                serde_json::from_slice::<StoredPairingCode>(bytes)
                    .map_err(|error| unavailable("decode pairing code", error))
            },
            encode_json::<StoredPairingCode>,
            |current: Option<StoredPairingCode>| async move {
                let Some(mut record) = current else {
                    return Err(EnvironmentAccessError::InvalidCode);
                };
                if record.consumed_at.is_some() || now >= record.expires_at {
                    return Err(EnvironmentAccessError::InvalidCode);
                }
                record.consumed_at = Some(now);
                Ok(CasApply::new(record, ()))
            },
        )
        .await
        .map_err(|error| map_cas_error("consume pairing code", error))?;
        self.create_device(client, StoredPairedBy::OneTimeCode, None)
            .await
    }

    async fn pair_tailnet_owner(
        &self,
        tailnet_login: &str,
        client: PairClient,
    ) -> Result<PairedDevice, EnvironmentAccessError> {
        self.create_device(
            client,
            StoredPairedBy::TailnetIdentity,
            Some(bounded_client_field(tailnet_login, "")),
        )
        .await
    }

    async fn authenticate_device(
        &self,
        token: &str,
    ) -> Result<Option<DeviceId>, EnvironmentAccessError> {
        let Some(device_id) = DeviceId::from_device_token(token) else {
            return Ok(None);
        };
        let path = device_path(&device_id)?;
        let Some(device) = self.read_record::<StoredDevice>(&path).await? else {
            return Ok(None);
        };
        let presented = sha256_hex(token.as_bytes());
        if !bool::from(presented.as_bytes().ct_eq(device.token_hash.as_bytes())) {
            return Ok(None);
        }
        let now = Utc::now();
        if device.revoked_at.is_some() || device.expires_at.is_some_and(|at| now >= at) {
            return Ok(None);
        }
        let stale = device
            .last_seen_at
            .is_none_or(|seen| now - seen >= LAST_SEEN_RESOLUTION);
        if stale {
            // `last_seen_at` is audit metadata: a failed refresh must not
            // turn a valid token into a 401, so it is logged and dropped.
            if let Err(error) = self
                .mutate_device(&device_id, move |record| {
                    if record
                        .last_seen_at
                        .is_none_or(|seen| now - seen >= LAST_SEEN_RESOLUTION)
                    {
                        record.last_seen_at = Some(now);
                    }
                })
                .await
            {
                tracing::debug!(
                    target: "ironclaw::reborn::environment_access",
                    device_id = %device_id,
                    %error,
                    "device last_seen_at refresh failed"
                );
            }
        }
        Ok(Some(device_id))
    }

    async fn list_devices(
        &self,
        current_device: Option<&DeviceId>,
    ) -> Result<DeviceList, EnvironmentAccessError> {
        let dir = devices_dir_path()?;
        let entries = match self.filesystem.list_dir(&self.scope, &dir).await {
            Ok(entries) => entries,
            Err(FilesystemError::NotFound { .. }) => Vec::new(),
            Err(error) => return Err(unavailable("list devices", error)),
        };
        let mut devices = Vec::new();
        for entry in entries.into_iter().take(MAX_LISTED_DEVICES) {
            if entry.file_type != FileType::File {
                continue;
            }
            let Some(device_id) = entry
                .name
                .strip_suffix(".json")
                .and_then(|stem| DeviceId::new(stem).ok())
            else {
                continue; // a foreign file under the directory is not a device
            };
            let Some(record) = self
                .read_record::<StoredDevice>(&device_path(&device_id)?)
                .await?
            else {
                continue; // listed then gone: nothing to show
            };
            devices.push(DeviceSummary {
                current: current_device == Some(&record.device_id),
                device_id: record.device_id.into_inner(),
                label: record.label,
                os: record.os,
                app_version: record.app_version,
                paired_by: record.paired_by.into(),
                created_at: record.created_at,
                last_seen_at: record.last_seen_at,
                revoked_at: record.revoked_at,
            });
        }
        devices.sort_by_key(|device| device.created_at);
        Ok(DeviceList { devices })
    }

    async fn revoke_device(&self, device_id: &DeviceId) -> Result<(), EnvironmentAccessError> {
        let now = Utc::now();
        self.mutate_device(device_id, move |record| {
            if record.revoked_at.is_none() {
                record.revoked_at = Some(now);
            }
        })
        .await
        .map(|_| ())
    }
}

fn identity_of(record: StoredEnvironment) -> EnvironmentIdentity {
    EnvironmentIdentity {
        environment_id: record.environment_id,
        label: record.label,
    }
}

/// 12 Crockford characters from the thread CSPRNG. 256 is a multiple of 32,
/// so masking a byte to its low five bits is already uniform; the rejection
/// branch keeps that true if the alphabet size ever changes.
fn mint_pairing_code() -> String {
    let alphabet_len = PAIRING_CODE_ALPHABET.len();
    let limit = 256 - (256 % alphabet_len);
    let mut rng = rand::rng();
    let mut out = String::with_capacity(PAIRING_CODE_LEN);
    while out.len() < PAIRING_CODE_LEN {
        let mut byte = [0u8; 1];
        rng.fill(&mut byte);
        let value = usize::from(byte[0]);
        if value >= limit {
            continue;
        }
        out.push(char::from(PAIRING_CODE_ALPHABET[value % alphabet_len]));
    }
    out
}

fn mint_device_token() -> Result<(DeviceId, String), EnvironmentAccessError> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let mut rng = rand::rng();
    let mut id_bytes = [0u8; 16];
    rng.fill(&mut id_bytes);
    let mut secret = [0u8; 32];
    rng.fill(&mut secret);
    let device_id = DeviceId::new(hex_lower(&id_bytes))
        .map_err(|error| unavailable("mint device id", error))?;
    let token = format!(
        "{DEVICE_TOKEN_PREFIX}{}_{}",
        device_id.as_str(),
        URL_SAFE_NO_PAD.encode(secret)
    );
    Ok((device_id, token))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_lower(&Sha256::digest(bytes))
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        #[allow(
            clippy::let_underscore_must_use,
            reason = "fmt::Write for String cannot fail"
        )]
        let _ = write!(out, "{byte:02x}"); // writing to a String cannot fail
    }
    out
}

/// Trim, strip control characters, and cap a client-supplied field.
fn bounded_client_field(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_CLIENT_FIELD_CHARS)
        .collect();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned
    }
}

fn encode_json<T: Serialize>(value: &T) -> Result<Entry, EnvironmentAccessError> {
    let body = serde_json::to_vec(value).map_err(|error| unavailable("encode record", error))?;
    Ok(Entry::bytes(body).with_content_type(ContentType::json()))
}

fn map_cas_error(
    operation: &'static str,
    error: CasUpdateError<EnvironmentAccessError>,
) -> EnvironmentAccessError {
    match error {
        CasUpdateError::Apply(inner) => inner,
        other => unavailable(operation, other),
    }
}

/// Log the cause server-side and return the sanitized error.
fn unavailable(operation: &'static str, cause: impl std::fmt::Display) -> EnvironmentAccessError {
    tracing::warn!(
        target: "ironclaw::reborn::environment_access",
        operation,
        error = %cause,
        "environment access store failure"
    );
    EnvironmentAccessError::Unavailable
}

fn environment_path() -> Result<ScopedPath, EnvironmentAccessError> {
    scoped_path(format!("{ENVIRONMENT_ROOT}/environment.json"))
}

fn pairing_code_path(canonical: &str) -> Result<ScopedPath, EnvironmentAccessError> {
    scoped_path(format!(
        "{ENVIRONMENT_ROOT}/pairing-codes/{}.json",
        sha256_hex(canonical.as_bytes())
    ))
}

fn devices_dir_path() -> Result<ScopedPath, EnvironmentAccessError> {
    scoped_path(format!("{ENVIRONMENT_ROOT}/devices"))
}

fn device_path(device_id: &DeviceId) -> Result<ScopedPath, EnvironmentAccessError> {
    scoped_path(format!(
        "{ENVIRONMENT_ROOT}/devices/{}.json",
        device_id.as_str()
    ))
}

fn scoped_path(raw: String) -> Result<ScopedPath, EnvironmentAccessError> {
    ScopedPath::new(raw).map_err(|error| unavailable("build environment path", error))
}

#[cfg(test)]
mod tests;
