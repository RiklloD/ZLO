//! `zlo doctor --bundle <FILE>` — one local zip of sanitized diagnostics to
//! attach to a bug report.
//!
//! Read-only with respect to application state: it reuses the same read
//! paths `doctor`, `status`, `config list` and `models status` already take
//! (none of which create the home), lists extension manifests on disk, and
//! tails the service logs (macOS launchd logs under the home and the
//! updater log; on Linux the service logs to the systemd journal, which is
//! not collected). It never opens the database, the secret store, the
//! workspace, memory, or any conversation record, and never uploads. The
//! CLI resolves the home without the legacy-home migration for this command.
//!
//! Every string passes through [`redact::Scrubber`] before it is written,
//! and the archive is refused if a known secret value survives anyway in
//! any spelling the final check decodes. Known values are collected only to
//! redact them; free text in logs is otherwise matched by pattern only.

mod encodings;
mod line;
mod redact;

use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::context::RebornCliContext;
use crate::dto::ServiceStateDto;

/// Bumped when an entry's shape changes incompatibly.
const BUNDLE_FORMAT_VERSION: u16 = 1;
/// Per log file: read at most this many trailing bytes …
const LOG_TAIL_BYTES: u64 = 256 * 1024;
/// … and keep at most this many trailing lines of them.
const LOG_TAIL_LINES: usize = 2000;
/// Extension manifests larger than this are listed without parsing.
const MANIFEST_MAX_BYTES: u64 = 1024 * 1024;
/// Bound on how many extension directories are listed.
const MAX_EXTENSIONS: usize = 256;
/// Length of the build commit written to `system.json`; below the scrubber's
/// 32-hex secret shape.
const SHORT_COMMIT_LEN: usize = 12;

pub(crate) struct BundleOptions {
    pub(crate) out: PathBuf,
    pub(crate) include_logs: bool,
    pub(crate) force: bool,
}

pub(crate) struct BundleReport {
    pub(crate) out: PathBuf,
    pub(crate) entries: Vec<String>,
    /// Why the list of known secret values may be incomplete; empty when
    /// every source of known values was readable.
    pub(crate) degraded: Vec<String>,
    /// A known secret is too short to redact safely, so every log tail was
    /// left out of the bundle.
    pub(crate) logs_withheld_for_short_secret: bool,
}

/// Process-level facts the bundle reads, injectable so tests stay hermetic
/// (no live service query, no process-env mutation).
pub(crate) struct BundleInputs<'a> {
    pub(crate) service: ServiceStateDto,
    /// The process environment, as `(name, value)` pairs.
    pub(crate) env: &'a [(String, String)],
}

pub(crate) fn write_bundle(
    context: &RebornCliContext,
    options: &BundleOptions,
) -> anyhow::Result<BundleReport> {
    let env: Vec<(String, String)> = std::env::vars_os()
        .map(|(name, value)| {
            (
                name.to_string_lossy().into_owned(),
                value.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let inputs = BundleInputs {
        service: crate::commands::status::resolve_service_state(),
        env: &env,
    };
    write_bundle_with(context, options, &inputs)
}

pub(crate) fn write_bundle_with(
    context: &RebornCliContext,
    options: &BundleOptions,
    inputs: &BundleInputs<'_>,
) -> anyhow::Result<BundleReport> {
    let out = std::path::absolute(&options.out)
        .map_err(|error| anyhow::anyhow!("resolve {}: {error}", options.out.display()))?;
    if !options.force && std::fs::symlink_metadata(&out).is_ok() {
        anyhow::bail!(
            "diagnostics bundle already exists at {}; pass --force to overwrite",
            out.display()
        );
    }
    let (entries, degraded, logs_withheld_for_short_secret) =
        collect_entries(context, inputs, options.include_logs)?;
    let bytes = encode_zip(&entries)?;
    crate::file_write::write_atomic_bytes(&out, &bytes, options.force, "diagnostics bundle")?;
    Ok(BundleReport {
        out,
        entries: entries.into_iter().map(|(name, _)| name).collect(),
        degraded,
        logs_withheld_for_short_secret,
    })
}

type Entry = (String, Vec<u8>);

fn collect_entries(
    context: &RebornCliContext,
    inputs: &BundleInputs<'_>,
    include_logs: bool,
) -> anyhow::Result<(Vec<Entry>, Vec<String>, bool)> {
    let home = context.boot_config().home();
    let profile = context.boot_config().profile();
    let config_list = crate::commands::config::config_list_dto(context);
    let admin = ironclaw_operator::RebornProviderAdmin::new(context.boot_config().clone());
    let providers = admin.list(None, true);
    let llm_status = admin.status();

    let known = known_secret_values(
        context,
        config_list.as_ref().ok(),
        providers.as_ref().ok(),
        inputs.env,
    );
    let scrubber = redact::Scrubber::new(known.values);
    let degraded = known.degraded;
    let excluded_names = known.excluded_names;
    // A known secret under 4 characters cannot be redacted without
    // destroying unrelated text, and the final check could not tell it from
    // ordinary text either: leave every log tail out instead.
    let withhold_logs = scrubber.has_unredactable_secret();
    let mut redaction_reasons = degraded.clone();
    if withhold_logs {
        redaction_reasons.push(SHORT_SECRET_REASON.to_string());
    }

    let mut entries: Vec<(String, Value)> = Vec::new();

    let doctor = crate::commands::doctor::build_doctor_dto(context);
    entries.push(("doctor.json".to_string(), serde_json::to_value(&doctor)?));

    let status =
        crate::commands::status::build_status_dto_with_service_state(context, inputs.service);
    entries.push((
        "status.json".to_string(),
        match status {
            Ok(dto) => serde_json::to_value(&dto)?,
            Err(error) => json!({ "error": error.to_string() }),
        },
    ));

    // `None` when config.toml failed to load (already reported in
    // config.json and doctor.json); `Some(None)` when it is absent.
    let config_file = config_list
        .as_ref()
        .ok()
        // silent-ok: the load error is reported in config.json/doctor.json and
        // system.json says the backend is unknown.
        .and_then(|list| ironclaw_config::RebornConfigFile::load(&list.config_file).ok());
    entries.push((
        "system.json".to_string(),
        system_json(context, config_file.as_ref().map(Option::as_ref)),
    ));

    entries.push((
        "config.json".to_string(),
        match &config_list {
            Ok(list) => {
                let values: serde_json::Map<String, Value> = list
                    .entries
                    .iter()
                    .map(|entry| Ok((entry.key.clone(), serde_json::to_value(&entry.value)?)))
                    .collect::<Result<_, serde_json::Error>>()?;
                json!({
                    "source": "config.toml layered over compiled defaults (env overrides are \
                               reflected in llm.json)",
                    "config_file_present": list.config_file.exists(),
                    "entries": values,
                })
            }
            Err(error) => json!({ "error": error.to_string() }),
        },
    ));

    entries.push((
        "llm.json".to_string(),
        llm_json(llm_status.as_ref(), providers.as_ref()),
    ));

    entries.push((
        "extensions.json".to_string(),
        extensions_json(&home.path().join(profile.local_runtime_storage_subdir()))?,
    ));

    let mut logs = Vec::new();
    let mut log_notes = Vec::new();
    if include_logs && withhold_logs {
        log_notes.push(format!("logs withheld: {SHORT_SECRET_REASON}"));
    } else if include_logs {
        let forms = scrubber.forms();
        for path in log_files(home.path()) {
            match tail_log(&path, &forms)? {
                Some(text) => {
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "log".to_string());
                    logs.push((format!("logs/{name}.tail.txt"), scrubber.text(&text)));
                }
                None => log_notes.push(format!("{}: absent", path.display())),
            }
        }
    } else {
        log_notes.push("logs omitted (--no-logs)".to_string());
    }

    let mut names: Vec<String> = entries.iter().map(|(name, _)| name.clone()).collect();
    names.extend(logs.iter().map(|(name, _)| name.clone()));
    let manifest = json!({
        "format_version": BUNDLE_FORMAT_VERSION,
        "created_at": chrono::Utc::now().to_rfc3339(),
        "entries": names,
        "logs": log_notes,
        "redaction_inputs": {
            "degraded": !redaction_reasons.is_empty(),
            "reasons": redaction_reasons,
            "setting_names": excluded_names,
        },
        "redaction": "Values this machine knows are secret (WebUI token, the local secrets \
                      master key, env vars named by config.toml, the built-in provider \
                      catalog or a credential-like name) are replaced in literal, \
                      JSON-escaped, percent-encoded and base64 form, and the archive is \
                      refused if one survives. Credential-named keys, URL userinfo/query \
                      strings, PEM blocks and recognized token shapes are replaced with \
                      [REDACTED]. Free text in logs is matched by pattern only: a secret the \
                      bundle does not know, in a shape it does not recognize, can survive \
                      in a log tail. Not collected at all: the database, the secret store, \
                      conversations, memory, workspace files, LLM payloads.",
    });

    let mut out: Vec<Entry> = Vec::new();
    for (name, mut value) in std::iter::once(("manifest.json".to_string(), manifest)).chain(entries)
    {
        scrubber.json(&mut value);
        out.push((name, serde_json::to_vec_pretty(&value)?));
    }
    out.extend(
        logs.into_iter()
            .map(|(name, text)| (name, text.into_bytes())),
    );

    if let Some((name, _)) = out
        .iter()
        .find(|(_, bytes)| scrubber.contains_known_secret(bytes))
    {
        let hint = if name.starts_with("logs/") {
            "; rerun with --no-logs to leave the log tails out"
        } else {
            ""
        };
        anyhow::bail!(
            "refusing to write diagnostics bundle: {name} still contains a known secret value \
             (possibly in an encoding the scrubber does not undo){hint}"
        );
    }
    Ok((out, degraded, withhold_logs))
}

/// Recorded in `manifest.json` (and shown by `doctor`) when a known secret
/// is too short to redact, so the log tails were left out.
const SHORT_SECRET_REASON: &str = "a known secret value is shorter than 4 characters and cannot be \
     redacted safely; log tails were left out of this bundle";

/// Env vars whose values are credentials by default, whatever config says.
const DEFAULT_SECRET_ENV_NAMES: &[&str] = &[
    "SECRETS_MASTER_KEY",
    "IRONCLAW_REBORN_SECRET_MASTER_KEY",
    "IRONCLAW_REBORN_POSTGRES_URL",
    "DATABASE_URL",
];
/// Bound on files read only to learn secret values.
const SECRET_SOURCE_MAX_BYTES: u64 = 1024 * 1024;

struct KnownSecrets {
    values: Vec<String>,
    /// Sources that could not be read, so `values` may be incomplete.
    degraded: Vec<String>,
    /// Credential-like env var / config key names left out of `values`
    /// because they name a setting ([`is_setting_name`]); names only, sorted.
    excluded_names: Vec<String>,
}

/// Values on this machine that are secret by construction, collected only to
/// redact them (never written): the WebUI token file, the local secrets
/// master key file, and the values of env vars that are credential sources —
/// default names, names the built-in provider catalog uses, names
/// `config.toml` and `providers.json` configure (from the parsed config and,
/// independently, from a raw scan of the file so a broken config still
/// counts), and any env var whose own name is credential-like.
fn known_secret_values(
    context: &RebornCliContext,
    config_list: Option<&crate::dto::ConfigListDto>,
    providers: Option<&ironclaw_product_contracts::operator_llm::RebornProviderList>,
    env: &[(String, String)],
) -> KnownSecrets {
    let home = context.boot_config().home();
    let mut values = Vec::new();
    let mut degraded = Vec::new();
    let mut excluded_names = Vec::new();

    match crate::webui_token::login_link(home) {
        Ok(Some(link)) => {
            if let Some((_, token)) = link.split_once("token=") {
                values.push(token.to_string());
            }
        }
        Ok(None) => {}
        Err(error) => degraded.push(format!("WebUI token file unreadable: {error}")),
    }
    let master_key = home
        .path()
        .join(
            context
                .boot_config()
                .profile()
                .local_runtime_storage_subdir(),
        )
        .join(ironclaw_composition::STANDALONE_SECRETS_MASTER_KEY_PATH);
    match read_bounded_regular_file(&master_key) {
        Ok(Some(key)) => values.push(key),
        Ok(None) => {}
        Err(error) => degraded.push(format!("secrets master key file unreadable: {error}")),
    }

    let mut env_names: Vec<String> = DEFAULT_SECRET_ENV_NAMES
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    env_names.push(ironclaw_config::WEBUI_TOKEN_ENV.to_string());
    env_names.push(ironclaw_config::LEGACY_WEBUI_TOKEN_ENV.to_string());
    env_names.push(crate::webui_token::default_env_token_var().to_string());
    env_names.extend(ironclaw_operator::RebornProviderAdmin::builtin_credential_env_names());

    match config_list {
        Some(list) => {
            for entry in &list.entries {
                if (entry.key.ends_with("_env") || entry.key.ends_with("_var"))
                    && let Some(crate::dto::ConfigValue::String(name)) = &entry.value
                {
                    env_names.push(name.clone());
                }
            }
        }
        None => degraded.push("config.toml did not load; using a raw scan of it".to_string()),
    }
    match providers {
        Some(providers) => env_names.extend(
            providers
                .providers
                .iter()
                .filter_map(|provider| provider.metadata.as_ref()?.api_key_env.clone()),
        ),
        None => degraded.push(
            "providers.json did not load; only built-in provider env names are known".to_string(),
        ),
    }
    match read_bounded_regular_file(&home.config_file_path()) {
        Ok(Some(text)) => {
            if !scan_raw_config(&text, &mut env_names, &mut values, &mut excluded_names) {
                degraded.push("config.toml is not valid TOML; scanned it line by line".to_string());
            }
        }
        Ok(None) => {}
        Err(error) => degraded.push(format!("config.toml unreadable: {error}")),
    }

    for (name, value) in env {
        let credential_named = redact::is_sensitive_key(name)
            || (name.to_ascii_uppercase().ends_with("_URL") && value.contains('@'));
        let configured = env_names.iter().any(|known| known == name);
        if configured {
            // Named by config/the catalog as a credential source: always a
            // secret, whatever the name looks like.
            values.push(value.clone());
        } else if credential_named {
            if is_setting_name(name) {
                excluded_names.push(name.clone());
            } else {
                values.push(value.clone());
            }
        }
    }
    // A known URL's userinfo password is a secret on its own: a log can
    // quote it outside that exact URL.
    let passwords: Vec<String> = values
        .iter()
        .flat_map(|value| url_userinfo_passwords(value))
        .collect();
    // Always kept, however short or plain: a URL password is a secret.
    // (Empty values are dropped by the scrubber itself.)
    values.extend(passwords);
    excluded_names.sort();
    excluded_names.dedup();
    KnownSecrets {
        values,
        degraded,
        excluded_names,
    }
}

/// Last name segments that make a credential-like name a *setting* about
/// the credential rather than the credential itself (`AUTH_ENABLED=1`,
/// `ACME_TOKEN_TTL=3600`).
const SETTING_NAME_SUFFIXES: &[&str] = &[
    "enabled", "disabled", "enable", "mode", "count", "port", "timeout", "level", "ttl", "retries",
];

/// A credential-like env var or config key whose last segment (split on
/// `_`, `.` or `-`) is a flag/count word names a setting: its value is not
/// a secret. Decided by the name alone — under any other credential-like
/// name the value is kept whatever it looks like, so `DEVICE_PIN=123456`
/// and `SOME_AUTH=1` stay known secrets. A URL's userinfo password is never
/// exempt.
fn is_setting_name(name: &str) -> bool {
    name.rsplit(['_', '.', '-']).next().is_some_and(|last| {
        SETTING_NAME_SUFFIXES
            .iter()
            .any(|suffix| last.eq_ignore_ascii_case(suffix))
    })
}

/// The password of a `scheme://user:password@host…` value, as written and
/// percent-decoded (when that differs and is valid UTF-8). Empty when the
/// value carries no userinfo password.
fn url_userinfo_passwords(value: &str) -> Vec<String> {
    let Some((_, rest)) = value.split_once("://") else {
        return Vec::new();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let Some((userinfo, _host)) = authority.rsplit_once('@') else {
        return Vec::new();
    };
    let Some((_user, password)) = userinfo.split_once(':') else {
        return Vec::new();
    };
    if password.is_empty() {
        return Vec::new();
    }
    let mut out = vec![password.to_string()];
    if let Some(decoded) = encodings::percent_decode_utf8(password)
        && decoded != password
    {
        out.push(decoded);
    }
    out
}

/// Collect `*_env`/`*_var` names and credential-keyed values from raw
/// `config.toml` text. Returns `false` when the text is not valid TOML (it is
/// then scanned line by line instead).
fn scan_raw_config(
    text: &str,
    env_names: &mut Vec<String>,
    values: &mut Vec<String>,
    excluded_names: &mut Vec<String>,
) -> bool {
    let mut classify = |key: &str, value: &str| {
        if key.ends_with("_env") || key.ends_with("_var") {
            env_names.push(value.to_string());
        } else if redact::is_sensitive_key(key) {
            if is_setting_name(key) {
                excluded_names.push(key.to_string());
            } else {
                values.push(value.to_string());
            }
        }
    };
    match toml::from_str::<toml::Table>(text) {
        Ok(table) => {
            let mut stack: Vec<&toml::Table> = vec![&table];
            while let Some(table) = stack.pop() {
                for (key, value) in table {
                    match value {
                        toml::Value::String(text) => classify(key, text),
                        toml::Value::Table(inner) => stack.push(inner),
                        _ => {}
                    }
                }
            }
            true
        }
        Err(_) => {
            // silent-ok: the parse failure is recorded as a degraded source
            // by the caller; the line scan below is the fallback.
            for line in text.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    let value = value.trim().trim_matches(|ch| ch == '"' || ch == '\'');
                    classify(key.trim(), value);
                }
            }
            false
        }
    }
}

/// Contents of a regular, non-symlink file of bounded size; `None` when
/// absent.
fn read_bounded_regular_file(path: &Path) -> anyhow::Result<Option<String>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => anyhow::bail!("stat {}: {error}", path.display()),
    };
    if !meta.is_file() || meta.len() > SECRET_SOURCE_MAX_BYTES {
        anyhow::bail!("{} is not a regular file of bounded size", path.display());
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))
}

fn system_json(
    context: &RebornCliContext,
    config_file: Option<Option<&ironclaw_config::RebornConfigFile>>,
) -> Value {
    let build = crate::commands::update::current_build();
    let profile = context.boot_config().profile();
    let backend = config_file
        .flatten()
        .and_then(|file| file.storage.as_ref())
        .and_then(|storage| storage.backend.as_ref());
    // silent-ok: StorageBackend serializes as a plain string; cannot fail.
    let configured_storage = backend.and_then(|backend| serde_json::to_value(backend).ok());
    let effective_kind = match (config_file, backend) {
        (None, _) => "unknown (config.toml did not load)",
        (Some(_), None) => "local_libsql",
        (Some(_), Some(ironclaw_config::StorageBackend::Postgres)) => "postgres",
        (Some(_), Some(ironclaw_config::StorageBackend::Unknown(_))) => {
            "unsupported (startup refuses this backend)"
        }
    };
    // The short SHA: a full 40-hex commit matches the scrubber's long-hex
    // secret shape (>= 32 hex chars) and would be written as `[REDACTED]`.
    // `current_build` only yields full ASCII-hex hashes, so the byte slice is
    // on a char boundary.
    let commit = build
        .commit
        .as_deref()
        .map(|commit| commit.get(..SHORT_COMMIT_LEN).unwrap_or(commit));
    json!({
        "zlo": { "version": build.version, "commit": commit },
        "platform": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "family": std::env::consts::FAMILY,
        },
        "profile": profile.as_str(),
        "home_source": context.boot_config().home().source_label(),
        "storage": {
            "configured_backend": configured_storage,
            "effective_kind": effective_kind,
            "local_storage_subdir": profile.local_runtime_storage_subdir(),
        },
    })
}

fn llm_json(
    status: Result<
        &ironclaw_product_contracts::operator_llm::RebornProviderStatus,
        &ironclaw_operator::RebornProviderAdminError,
    >,
    providers: Result<
        &ironclaw_product_contracts::operator_llm::RebornProviderList,
        &ironclaw_operator::RebornProviderAdminError,
    >,
) -> Value {
    let status = match status {
        Ok(status) => status,
        Err(error) => return json!({ "error": error.to_string() }),
    };
    let active_backend_kind = providers.ok().and_then(|list| {
        list.providers
            .iter()
            .find(|provider| provider.active)
            .and_then(|provider| provider.metadata.as_ref())
            .map(|metadata| metadata.protocol.clone())
    });
    json!({
        "status": status,
        "enabled_backend_kinds": active_backend_kind.into_iter().collect::<Vec<_>>(),
    })
}

/// Extension packages present under `<storage root>/system/extensions`:
/// id and version from each `manifest.toml`, nothing else.
fn extensions_json(storage_root: &Path) -> anyhow::Result<Value> {
    #[derive(serde::Deserialize)]
    struct ManifestIdentity {
        id: Option<String>,
        version: Option<String>,
    }

    let root = storage_root.join("system").join("extensions");
    let read_dir = match std::fs::read_dir(&root) {
        Ok(read_dir) => read_dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({ "installed": [], "note": "no extension directory" }));
        }
        Err(error) => anyhow::bail!("list {}: {error}", root.display()),
    };
    let mut installed = Vec::new();
    for dir_entry in read_dir.take(MAX_EXTENSIONS) {
        let dir_entry =
            dir_entry.map_err(|error| anyhow::anyhow!("list {}: {error}", root.display()))?;
        let manifest = dir_entry.path().join("manifest.toml");
        let directory = dir_entry.file_name().to_string_lossy().into_owned();
        let parsed = match std::fs::symlink_metadata(&manifest) {
            Ok(meta) if meta.is_file() && meta.len() <= MANIFEST_MAX_BYTES => {
                std::fs::read_to_string(&manifest)
                    .map_err(|error| error.to_string())
                    .and_then(|text| {
                        toml::from_str::<ManifestIdentity>(&text).map_err(|error| error.to_string())
                    })
            }
            Ok(_) => Err("manifest is not a regular file or is too large".to_string()),
            Err(error) => Err(error.to_string()),
        };
        installed.push(match parsed {
            Ok(identity) => json!({
                "directory": directory,
                "id": identity.id,
                "version": identity.version,
            }),
            Err(error) => json!({ "directory": directory, "error": error }),
        });
    }
    installed.sort_by(|a, b| a["directory"].as_str().cmp(&b["directory"].as_str()));
    Ok(json!({ "installed": installed }))
}

/// The log files the OS service and the updater write under the home.
fn log_files(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join("logs").join("serve.stdout.log"),
        home.join("logs").join("serve.stderr.log"),
        crate::commands::update::UpdatePaths::new(home).log_file(),
    ]
}

/// The last [`LOG_TAIL_LINES`] lines within the last [`LOG_TAIL_BYTES`] of
/// a regular (non-symlink) file; `None` when absent. Not yet scrubbed, but
/// cut so that scrubbing it is sound: see [`select_tail`].
fn tail_log(path: &Path, secret_forms: &[String]) -> anyhow::Result<Option<String>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => anyhow::bail!("stat {}: {error}", path.display()),
    };
    if !meta.is_file() {
        return Ok(None);
    }
    // Read enough before the tail that any known secret overlapping the
    // retained part is wholly inside what was read.
    let overlap = secret_forms
        .iter()
        .map(String::len)
        .max()
        .map_or(0, |longest| longest as u64 + 1)
        .min(LOG_TAIL_BYTES);
    let window = LOG_TAIL_BYTES + overlap;
    let mut file = std::fs::File::open(path)
        .map_err(|error| anyhow::anyhow!("open {}: {error}", path.display()))?;
    let start = meta.len().saturating_sub(window);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| anyhow::anyhow!("seek {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(window)
        .read_to_end(&mut bytes)
        .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(Some(select_tail(
        &text,
        start > 0,
        overlap as usize,
        secret_forms,
    )))
}

/// Pick the retained tail of `raw` without cutting through a known secret.
///
/// - The start is a line start at least `overlap` bytes in when `raw` began
///   mid-file (so a secret straddling the read window is never retained in
///   part), within the byte and line limits, and never inside an occurrence
///   of a known secret (a multiline value is kept whole or dropped whole).
/// - The end drops an incomplete last record (no trailing newline: possibly
///   still being written) and never lands inside a known secret; if the kept
///   text then ends with the leading lines of a known multiline secret, they
///   are replaced with `[REDACTED]`.
fn select_tail(raw: &str, began_mid_file: bool, overlap: usize, secret_forms: &[String]) -> String {
    let bytes = raw.as_bytes();
    let mut spans: Vec<(usize, usize)> = secret_forms
        .iter()
        .flat_map(|form| {
            raw.match_indices(form.as_str())
                .map(|(index, found)| (index, index + found.len()))
        })
        .collect();
    spans.sort_unstable();
    let inside = |pos: usize| spans.iter().find(|&&(s, e)| s < pos && pos < e).copied();

    let mut end = if raw.ends_with('\n') {
        raw.len()
    } else {
        raw.rfind('\n').map_or(0, |index| index + 1)
    };
    while let Some((span_start, _)) = inside(end) {
        end = raw[..span_start].rfind('\n').map_or(0, |index| index + 1);
    }

    // Smallest safe line start at or after `from`.
    let line_start_from = |from: usize| -> usize {
        let mut pos = from;
        loop {
            let candidate = if pos == 0 && !began_mid_file {
                0
            } else {
                let search = pos.saturating_sub(1);
                bytes[search..]
                    .iter()
                    .position(|&byte| byte == b'\n')
                    .map_or(raw.len(), |offset| search + offset + 1)
            };
            match inside(candidate) {
                Some((_, span_end)) => pos = span_end,
                None => return candidate,
            }
        }
    };
    let mut min_start = raw.len().saturating_sub(LOG_TAIL_BYTES as usize);
    if began_mid_file {
        min_start = min_start.max(overlap).max(1);
    }
    let mut start = line_start_from(min_start).min(end);
    let lines = raw[start..end].lines().count();
    if lines > LOG_TAIL_LINES {
        let skip = lines - LOG_TAIL_LINES;
        let cut = raw[start..end]
            .match_indices('\n')
            .nth(skip - 1)
            .map_or(end, |(index, _)| start + index + 1);
        start = line_start_from(cut).min(end);
    }

    let mut kept = raw[start..end].to_string();
    let partial = secret_forms
        .iter()
        .flat_map(|form| {
            form.match_indices('\n')
                .map(|(index, _)| &form[..=index])
                .filter(move |prefix| prefix.len() < form.len())
        })
        .filter(|prefix| kept.ends_with(prefix))
        .map(str::len)
        .max();
    if let Some(len) = partial {
        kept.truncate(kept.len() - len);
        kept.push_str(redact::REDACTED);
    }
    kept.trim_end_matches('\n').to_string()
}

fn encode_zip(entries: &[Entry]) -> anyhow::Result<Vec<u8>> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o600);
    for (name, bytes) in entries {
        writer
            .start_file(name.as_str(), options)
            .map_err(|error| anyhow::anyhow!("add {name} to bundle: {error}"))?;
        writer
            .write_all(bytes)
            .map_err(|error| anyhow::anyhow!("write {name} to bundle: {error}"))?;
    }
    let cursor = writer
        .finish()
        .map_err(|error| anyhow::anyhow!("finish bundle: {error}"))?;
    Ok(cursor.into_inner())
}

#[cfg(test)]
mod tests;
