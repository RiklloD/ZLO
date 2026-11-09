//! What `zlo export` takes from the data folder, and how.
//!
//! - Every storage root (a top-level folder holding `reborn-local-dev.db`)
//!   contributes a consistent database snapshot (composition's `VACUUM INTO`,
//!   safe while the backend runs, with this machine's environment identity,
//!   pairing codes and devices removed) and, when the backend has one, its
//!   secrets master key — from the dotfile, `SECRETS_MASTER_KEY`, or the OS
//!   keychain, exactly as the backend resolves it.
//! - Every other regular file is copied as-is, except what [`left_out`]
//!   names: logs, pid/lock files, update job state, retired service files,
//!   the installed binary, the WebUI master token, database side files.
//!   Symbolic links are skipped, never followed.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use ironclaw_composition::setup_transfer;
use secrecy::{ExposeSecret, SecretString};
use sha2::Digest as _;

use super::format::{BundleWriter, TAG_END, TAG_FILE, TAG_MANIFEST};
use super::{ItemKind, Manifest, ManifestItem, SourceInfo, ZloBuild, entry_name, sha256_hex};

const DATABASE_FILE: &str = "reborn-local-dev.db";

/// What an export took and left out.
#[derive(Debug)]
pub(crate) struct ExportReport {
    pub(crate) out: PathBuf,
    pub(crate) items: usize,
    pub(crate) bytes: u64,
    pub(crate) databases: usize,
    pub(crate) master_keys: usize,
    pub(crate) secrets: u64,
    pub(crate) environment_records_left_out: u64,
    pub(crate) left_out: Vec<String>,
    /// Storage roots whose stored secrets have no master key to go with them.
    pub(crate) keyless_storage_roots: Vec<String>,
}

/// Inputs for [`export_setup`].
pub(crate) struct ExportRequest<'a> {
    pub(crate) data_folder: &'a Path,
    pub(crate) user_home: &'a Path,
    pub(crate) hostname: &'a str,
    pub(crate) out: &'a Path,
    pub(crate) passphrase: &'a SecretString,
    pub(crate) iterations: u32,
}

/// The plain-words reason a path is not exported, or `None` to take it.
fn left_out(relative: &Path) -> Option<&'static str> {
    let mut components = relative.components();
    let top = components.next()?.as_os_str().to_str()?;
    let name = relative.file_name()?.to_str()?;
    let depth = relative.components().count();
    match top {
        "logs" => return Some("logs"),
        "update" => return Some("update job state"),
        "bin" => return Some("the installed zlo binary (install ZLO on the new computer)"),
        "webui-token" => {
            return Some("the WebUI master token (the new computer makes its own)");
        }
        "cache" | "caches" | "tmp" => return Some("caches"),
        _ => {}
    }
    if depth == 1 && (name == ".env" || name.starts_with(".env.")) {
        return Some("this computer's environment file (.env)");
    }
    if name.starts_with("retired-") {
        return Some("retired service files");
    }
    // ZLO's own, at the top of the data folder. Deeper down a `.lock` is
    // somebody's file (a workspace `Cargo.lock`) and travels.
    if (depth == 1
        && (name.ends_with(".pid") || name.ends_with(".lock") || name.ends_with(".sock")))
        || name == ".zlo-migration-pending"
    {
        return Some("pid and lock files");
    }
    if name.ends_with("-wal") || name.ends_with("-shm") || name.ends_with("-journal") {
        return Some("database side files (the snapshot replaces them)");
    }
    if name.ends_with(".zlobundle") {
        return Some("other setup files");
    }
    if name == ".DS_Store" {
        return Some("Finder metadata");
    }
    // Taken separately, as a snapshot and as the master key.
    if depth == 2 && (name == DATABASE_FILE || name == setup_transfer::SECRETS_MASTER_KEY_FILE) {
        return Some("");
    }
    None
}

/// Regular files under `root`, sorted, with what was skipped.
fn walk(
    root: &Path,
    relative: &Path,
    files: &mut Vec<(PathBuf, u64)>,
    skipped: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    let dir = root.join(relative);
    let mut entries = std::fs::read_dir(&dir)
        .with_context(|| format!("could not read {}", dir.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("could not read {}", dir.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let rel = relative.join(entry.file_name());
        if let Some(reason) = left_out(&rel) {
            if !reason.is_empty() {
                skipped.insert(reason.to_string());
            }
            continue;
        }
        let meta = std::fs::symlink_metadata(entry.path())
            .with_context(|| format!("could not inspect {}", entry.path().display()))?;
        if meta.file_type().is_symlink() {
            skipped.insert(format!("symbolic link {}", rel.display()));
        } else if meta.is_dir() {
            walk(root, &rel, files, skipped)?;
        } else if meta.is_file() {
            if entry_name(&rel).is_none() {
                skipped.insert(format!("{} (name is not UTF-8)", rel.display()));
                continue;
            }
            files.push((rel, meta.len()));
        } else {
            skipped.insert(format!("special file {}", rel.display()));
        }
    }
    Ok(())
}

fn write_header(
    writer: &mut BundleWriter<File>,
    tag: u8,
    name: &str,
    size: u64,
) -> anyhow::Result<()> {
    let name_len = u16::try_from(name.len()).context("entry name too long")?;
    writer.write_all(&[tag])?;
    writer.write_all(&name_len.to_be_bytes())?;
    writer.write_all(name.as_bytes())?;
    writer.write_all(&size.to_be_bytes())?;
    Ok(())
}

/// Stream exactly `size` bytes of `source` as one record.
fn write_file_record(
    writer: &mut BundleWriter<File>,
    name: &str,
    kind: ItemKind,
    size: u64,
    mut source: impl Read,
) -> anyhow::Result<ManifestItem> {
    write_header(writer, TAG_FILE, name, size)?;
    let mut hasher = sha2::Sha256::new();
    let mut remaining = size;
    let mut buf = vec![0u8; 256 * 1024];
    while remaining > 0 {
        let want = usize::try_from(remaining.min(buf.len() as u64)).unwrap_or(buf.len()); // silent-ok: bounded by buf.len()
        let n = source
            .read(&mut buf[..want])
            .with_context(|| format!("could not read {name}"))?;
        if n == 0 {
            anyhow::bail!("{name} got shorter while it was being exported; run the export again");
        }
        hasher.update(&buf[..n]);
        writer.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(ManifestItem {
        path: name.to_string(),
        kind,
        size,
        sha256: sha256_hex(hasher.finalize()),
    })
}

/// Write the archive for `request.data_folder` to `request.out` (which must
/// not exist). The file appears only when complete.
pub(crate) async fn export_setup(request: ExportRequest<'_>) -> anyhow::Result<ExportReport> {
    let data_folder = request.data_folder;
    if !data_folder.is_dir() {
        anyhow::bail!(
            "there is no ZLO data folder at {} — nothing to export",
            data_folder.display()
        );
    }
    let out_parent = request
        .out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let out_parent = std::fs::canonicalize(out_parent)
        .with_context(|| format!("the folder for {} does not exist", request.out.display()))?;
    let canonical_data = std::fs::canonicalize(data_folder)
        .with_context(|| format!("could not resolve {}", data_folder.display()))?;
    if out_parent.starts_with(&canonical_data) {
        anyhow::bail!(
            "write the setup file outside the data folder ({})",
            data_folder.display()
        );
    }
    if request.out.exists() {
        anyhow::bail!(
            "{} already exists; pick another --out",
            request.out.display()
        );
    }

    let mut files = Vec::new();
    let mut skipped = BTreeSet::new();
    walk(data_folder, Path::new(""), &mut files, &mut skipped)?;

    // Snapshots are decrypted databases: keep them in a private folder next
    // to the output, removed when this returns.
    let scratch = tempfile::Builder::new()
        .prefix(".zlo-export-")
        .tempdir_in(&out_parent)
        .context("could not create a private scratch folder for the export")?;
    let partial = tempfile::Builder::new()
        .prefix(".zlo-export-")
        .suffix(".partial")
        .tempfile_in(&out_parent)
        .context("could not create the setup file")?;
    let (partial_file, partial_path) = partial.into_parts();
    let mut writer = BundleWriter::new(partial_file, request.passphrase, request.iterations)?;

    let mut items = Vec::new();
    let mut report = ExportReport {
        out: request.out.to_path_buf(),
        items: 0,
        bytes: 0,
        databases: 0,
        master_keys: 0,
        secrets: 0,
        environment_records_left_out: 0,
        left_out: Vec::new(),
        keyless_storage_roots: Vec::new(),
    };

    let mut storage_roots = std::fs::read_dir(data_folder)
        .with_context(|| format!("could not read {}", data_folder.display()))?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|entry| entry.path().join(DATABASE_FILE).is_file())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect::<Vec<_>>();
    storage_roots.sort();
    for (index, storage) in storage_roots.iter().enumerate() {
        let root = data_folder.join(storage);
        let snapshot = scratch.path().join(format!("{index}.db"));
        let taken = setup_transfer::snapshot_standalone_database(&root, &snapshot)
            .await
            .with_context(|| format!("could not snapshot the {storage} database"))?;
        report.environment_records_left_out += taken.environment_records_left_out;
        let summary = setup_transfer::summarize_standalone_database(&snapshot)
            .await
            .with_context(|| format!("could not read the {storage} snapshot"))?;
        report.secrets += summary.secrets;
        let size = std::fs::metadata(&snapshot)?.len();
        items.push(write_file_record(
            &mut writer,
            &format!("{storage}/{DATABASE_FILE}"),
            ItemKind::Database,
            size,
            File::open(&snapshot)?,
        )?);
        report.databases += 1;
        match setup_transfer::read_standalone_secret_master_key(&root)
            .await
            .with_context(|| format!("could not read the {storage} secrets master key"))?
        {
            Some(key) => {
                let text = zeroize::Zeroizing::new(format!("{}\n", key.expose_secret()));
                items.push(write_file_record(
                    &mut writer,
                    &format!("{storage}/{}", setup_transfer::SECRETS_MASTER_KEY_FILE),
                    ItemKind::SecretsMasterKey,
                    text.len() as u64,
                    text.as_bytes(),
                )?);
                report.master_keys += 1;
            }
            None if summary.secrets > 0 => report.keyless_storage_roots.push(storage.clone()),
            None => {}
        }
    }

    for (relative, size) in &files {
        let Some(name) = entry_name(relative) else {
            continue;
        };
        let path = data_folder.join(relative);
        let file =
            File::open(&path).with_context(|| format!("could not open {}", path.display()))?;
        items.push(write_file_record(
            &mut writer,
            &name,
            ItemKind::File,
            *size,
            file,
        )?);
    }

    report.left_out = skipped.into_iter().collect();
    report.left_out.extend(
        [
            "this computer's environment identity, pairing codes and paired devices",
            "Claude Code / Codex session files and the agent folders under ~/.ironclaw \
             (they live outside the data folder)",
            "project source folders",
        ]
        .map(str::to_string),
    );
    report.items = items.len();
    report.bytes = items.iter().map(|item| item.size).sum();

    let manifest = Manifest {
        format_version: super::format::FORMAT_VERSION,
        created_at: chrono::Utc::now(),
        source: SourceInfo {
            os: super::current_os().to_string(),
            arch: std::env::consts::ARCH.to_string(),
            hostname: request.hostname.to_string(),
            user_home: request.user_home.display().to_string(),
            data_folder: data_folder.display().to_string(),
        },
        zlo: ZloBuild::current(),
        items,
        left_out: report.left_out.clone(),
    };
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    write_header(
        &mut writer,
        TAG_MANIFEST,
        "manifest.json",
        manifest_bytes.len() as u64,
    )?;
    writer.write_all(&manifest_bytes)?;
    writer.write_all(&[TAG_END])?;
    let mut file = writer.finish()?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    partial_path
        .persist_noclobber(request.out)
        .map_err(|error| {
            anyhow::anyhow!("could not write {}: {}", request.out.display(), error.error)
        })?;
    Ok(report)
}

/// `zlo-setup-<computer>-<date>.zlobundle` in the current folder.
pub(crate) fn default_out(hostname: &str, today: chrono::NaiveDate) -> PathBuf {
    let host: String = hostname
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let host = if host.is_empty() {
        "computer".to_string()
    } else {
        host
    };
    PathBuf::from(format!(
        "zlo-setup-{host}-{}.zlobundle",
        today.format("%Y-%m-%d")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_local_state_is_left_out() {
        for path in [
            "logs/serve.stdout.log",
            "update/run.json",
            "bin/zlo",
            "webui-token",
            "retired-com.ironclaw.reborn.plist",
            "local-dev/reborn-local-dev.db-wal",
            "local-dev/reborn-local-dev.db",
            "local-dev/.reborn-local-dev-secrets-master-key",
            "serve.pid",
            ".zlo-migration.lock",
            "config.toml.lock",
            ".env",
            ".env.bak-20261026",
            "old.zlobundle",
        ] {
            assert!(
                left_out(Path::new(path)).is_some(),
                "{path} must be left out"
            );
        }
        for path in [
            "config.toml",
            "providers.json",
            "local-dev/workspace/notes.md",
            "local-dev/system/skills/x/SKILL.md",
            "workspace/app/Cargo.lock",
            "workspace/app/.env",
            "SYSTEM.md",
        ] {
            assert!(left_out(Path::new(path)).is_none(), "{path} must be taken");
        }
    }

    #[test]
    fn default_name_is_safe() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 11, 9).expect("date");
        assert_eq!(
            default_out("Ana's MacBook Pro", day),
            PathBuf::from("zlo-setup-ana-s-macbook-pro-2026-11-09.zlobundle")
        );
        assert_eq!(
            default_out("../..", day),
            PathBuf::from("zlo-setup-computer-2026-11-09.zlobundle")
        );
    }
}
