//! `zlo doctor --bundle <FILE>` — one local zip of sanitized diagnostics to
//! attach to a bug report.
//!
//! Read-only with respect to application state: it reuses the same read
//! paths `doctor`, `status`, `config list` and `models status` already take
//! (none of which create the home), lists extension manifests on disk, and
//! tails the service logs. It never opens the database, the secret store,
//! the workspace, memory, or any conversation record, and never uploads.
//! Every string passes through [`redact::Scrubber`] before it is written,
//! and the archive is refused if a known secret value survives anyway.

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

pub(crate) struct BundleOptions {
    pub(crate) out: PathBuf,
    pub(crate) include_logs: bool,
    pub(crate) force: bool,
}

pub(crate) struct BundleReport {
    pub(crate) out: PathBuf,
    pub(crate) entries: Vec<String>,
}

/// Process-level facts the bundle reads, injectable so tests stay hermetic
/// (no live service query, no process-env mutation).
pub(crate) struct BundleInputs<'a> {
    pub(crate) service: ServiceStateDto,
    pub(crate) env: &'a dyn Fn(&str) -> Option<String>,
}

pub(crate) fn write_bundle(
    context: &RebornCliContext,
    options: &BundleOptions,
) -> anyhow::Result<BundleReport> {
    let env = |name: &str| std::env::var(name).ok();
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
    let entries = collect_entries(context, inputs, options.include_logs)?;
    let bytes = encode_zip(&entries)?;
    let out = std::path::absolute(&options.out)
        .map_err(|error| anyhow::anyhow!("resolve {}: {error}", options.out.display()))?;
    crate::file_write::write_atomic_bytes(&out, &bytes, options.force, "diagnostics bundle")?;
    Ok(BundleReport {
        out,
        entries: entries.into_iter().map(|(name, _)| name).collect(),
    })
}

type Entry = (String, Vec<u8>);

fn collect_entries(
    context: &RebornCliContext,
    inputs: &BundleInputs<'_>,
    include_logs: bool,
) -> anyhow::Result<Vec<Entry>> {
    let home = context.boot_config().home();
    let profile = context.boot_config().profile();
    let config_list = crate::commands::config::config_list_dto(context);
    let admin = ironclaw_operator::RebornProviderAdmin::new(context.boot_config().clone());
    let providers = admin.list(None, true);
    let llm_status = admin.status();

    let scrubber = redact::Scrubber::new(known_secret_values(
        home,
        config_list.as_ref().ok(),
        providers.as_ref().ok(),
        inputs.env,
    )?);

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

    let config_file = config_list
        .as_ref()
        .ok()
        // silent-ok: a config.toml that fails to load is already reported in
        // config.json and doctor.json; system.json then shows no backend.
        .and_then(|list| ironclaw_config::RebornConfigFile::load(&list.config_file).ok())
        .flatten();
    entries.push((
        "system.json".to_string(),
        system_json(context, config_file.as_ref()),
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
    if include_logs {
        for path in log_files(home.path()) {
            match tail_log(&path)? {
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
        "redaction": "Secrets, tokens, credentials, URL userinfo/query strings and API keys \
                      are replaced with [REDACTED]. Not collected at all: the database, \
                      the secret store, conversations, memory, workspace files, LLM payloads.",
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
        anyhow::bail!("refusing to write diagnostics bundle: {name} still contains a secret value");
    }
    Ok(out)
}

/// Values on this machine that are secret by construction: the WebUI token
/// file and the values of env vars the config or provider catalog names as
/// credential sources. Only read to redact them; never written.
fn known_secret_values(
    home: &ironclaw_config::RebornHome,
    config_list: Option<&crate::dto::ConfigListDto>,
    providers: Option<&ironclaw_product_contracts::operator_llm::RebornProviderList>,
    env: &dyn Fn(&str) -> Option<String>,
) -> anyhow::Result<Vec<String>> {
    let mut values = Vec::new();
    if let Some(link) = crate::webui_token::login_link(home)?
        && let Some((_, token)) = link.split_once("token=")
    {
        values.push(token.to_string());
    }
    let mut env_names = vec![crate::webui_token::default_env_token_var().to_string()];
    if let Some(list) = config_list {
        for entry in &list.entries {
            if (entry.key.ends_with("_env") || entry.key.ends_with("_var"))
                && let Some(crate::dto::ConfigValue::String(name)) = &entry.value
            {
                env_names.push(name.clone());
            }
        }
    }
    if let Some(providers) = providers {
        env_names.extend(
            providers
                .providers
                .iter()
                .filter_map(|provider| provider.metadata.as_ref()?.api_key_env.clone()),
        );
    }
    values.extend(env_names.iter().filter_map(|name| env(name)));
    Ok(values)
}

fn system_json(
    context: &RebornCliContext,
    config_file: Option<&ironclaw_config::RebornConfigFile>,
) -> Value {
    let build = crate::commands::update::current_build();
    let profile = context.boot_config().profile();
    let configured_storage = config_file
        .and_then(|file| file.storage.as_ref())
        .and_then(|storage| storage.backend.as_ref())
        // silent-ok: StorageBackend serializes as a plain string; cannot fail.
        .and_then(|backend| serde_json::to_value(backend).ok());
    json!({
        "zlo": { "version": build.version, "commit": build.commit },
        "platform": {
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "family": std::env::consts::FAMILY,
        },
        "profile": profile.as_str(),
        "home_source": context.boot_config().home().source_label(),
        "storage": {
            "configured_backend": configured_storage,
            "effective_kind": if configured_storage.is_some() { "configured" } else { "local_libsql" },
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
/// a regular (non-symlink) file; `None` when absent.
fn tail_log(path: &Path) -> anyhow::Result<Option<String>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => anyhow::bail!("stat {}: {error}", path.display()),
    };
    if !meta.is_file() {
        return Ok(None);
    }
    let mut file = std::fs::File::open(path)
        .map_err(|error| anyhow::anyhow!("open {}: {error}", path.display()))?;
    let start = meta.len().saturating_sub(LOG_TAIL_BYTES);
    file.seek(SeekFrom::Start(start))
        .map_err(|error| anyhow::anyhow!("seek {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(LOG_TAIL_BYTES)
        .read_to_end(&mut bytes)
        .map_err(|error| anyhow::anyhow!("read {}: {error}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        // The first line was cut mid-way by the byte window.
        lines.remove(0);
    }
    let keep = lines.len().saturating_sub(LOG_TAIL_LINES);
    Ok(Some(lines[keep..].join("\n")))
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
