//! `zlo import`: verify everything into a private staging folder next to
//! the data folder, and only then move it into place.
//!
//! Nothing is written before the passphrase opens the first chunk; a
//! damaged, tampered, truncated or mismatching archive leaves only the
//! staging folder, which is removed. An existing data folder is never
//! deleted: with `--replace` its contents move to
//! `<data folder>.before-import-<timestamp>` first (the installed `bin/`
//! stays where it is).

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use ironclaw_composition::setup_transfer::{self, DatabaseSummary};
use secrecy::SecretString;
use sha2::Digest as _;

use super::format::{BundleReader, FORMAT_VERSION, TAG_END, TAG_FILE, TAG_MANIFEST, TAG_SYMLINK};
use super::paths::HomePathRewrite;
use super::{
    BundleError, ItemKind, MAX_ITEM_BYTES, MAX_ITEMS, MAX_MANIFEST_BYTES, MAX_MASTER_KEY_BYTES,
    MAX_TOTAL_BYTES, Manifest, ManifestItem, SourceInfo, entry_path, sha256_hex,
};

const DATABASE_FILE: &str = "reborn-local-dev.db";
/// Entries a fresh install may already have that are not "data".
const NOT_DATA: &[&str] = &["bin", "logs", ".DS_Store"];
/// Top-level text files whose absolute paths are rewritten.
const MAX_REWRITTEN_TEXT_BYTES: u64 = 4 * 1024 * 1024;

/// Where and how to import.
pub(crate) struct ImportRequest<'a> {
    pub(crate) data_folder: &'a Path,
    pub(crate) user_home: &'a Path,
    pub(crate) passphrase: &'a SecretString,
    pub(crate) replace: bool,
    pub(crate) now: chrono::DateTime<chrono::Utc>,
}

/// What an import did.
#[derive(Debug)]
pub(crate) struct ImportReport {
    pub(crate) source: SourceInfo,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) items: usize,
    pub(crate) bytes: u64,
    pub(crate) databases: Vec<(String, DatabaseSummary)>,
    pub(crate) master_keys: usize,
    pub(crate) paths_rewritten: u64,
    pub(crate) moved_aside: Option<PathBuf>,
    pub(crate) environment_records_kept: u64,
    /// Referenced folders that do not exist on this machine.
    pub(crate) missing_folders: Vec<String>,
    pub(crate) left_out: Vec<String>,
}

/// Whether `folder` holds anything beyond what a fresh install has.
pub(crate) fn has_data(folder: &Path) -> anyhow::Result<bool> {
    match std::fs::read_dir(folder) {
        Ok(entries) => {
            for entry in entries {
                let entry =
                    entry.with_context(|| format!("could not read {}", folder.display()))?;
                let name = entry.file_name();
                if !NOT_DATA.iter().any(|skip| name == *skip) {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("could not read {}", folder.display())),
    }
}

fn read_u8(reader: &mut BundleReader<impl Read>) -> Result<u8, BundleError> {
    let mut byte = [0u8; 1];
    reader.read_exact(&mut byte)?;
    Ok(byte[0])
}

fn read_name_and_size(reader: &mut BundleReader<impl Read>) -> Result<(String, u64), BundleError> {
    let mut len = [0u8; 2];
    reader.read_exact(&mut len)?;
    let len = usize::from(u16::from_be_bytes(len));
    if len == 0 || len > super::format::MAX_NAME_LEN {
        return Err(BundleError::Damaged);
    }
    let mut name = vec![0u8; len];
    reader.read_exact(&mut name)?;
    let name = String::from_utf8(name).map_err(|_| BundleError::Damaged)?;
    let mut size = [0u8; 8];
    reader.read_exact(&mut size)?;
    Ok((name, u64::from_be_bytes(size)))
}

fn copy_out(
    reader: &mut BundleReader<impl Read>,
    size: u64,
    out: &mut impl Write,
) -> Result<String, BundleError> {
    let mut hasher = sha2::Sha256::new();
    let mut remaining = size;
    let mut buf = vec![0u8; 256 * 1024];
    while remaining > 0 {
        let take = usize::try_from(remaining.min(buf.len() as u64)).unwrap_or(buf.len()); // silent-ok: bounded by buf.len()
        reader.read_exact(&mut buf[..take])?;
        hasher.update(&buf[..take]);
        out.write_all(&buf[..take])?;
        remaining -= take as u64;
    }
    Ok(sha256_hex(hasher.finalize()))
}

fn is_master_key(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == setup_transfer::SECRETS_MASTER_KEY_FILE)
}

fn is_database(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == DATABASE_FILE)
}

struct Unpacked {
    manifest: Manifest,
    master_keys: Vec<(PathBuf, SecretString)>,
}

/// Decrypt, bound and verify the whole archive into `staging`.
fn unpack(mut reader: BundleReader<impl Read>, staging: &Path) -> Result<Unpacked, BundleError> {
    let mut seen: Vec<(String, u64, String)> = Vec::new();
    let mut total = 0u64;
    let mut master_keys = Vec::new();
    let manifest_bytes = loop {
        match read_u8(&mut reader)? {
            TAG_FILE => {
                let (name, size) = read_name_and_size(&mut reader)?;
                let path = entry_path(&name)?;
                if seen.len() >= MAX_ITEMS || size > MAX_ITEM_BYTES {
                    return Err(BundleError::TooLarge);
                }
                total = total.checked_add(size).ok_or(BundleError::TooLarge)?;
                if total > MAX_TOTAL_BYTES {
                    return Err(BundleError::TooLarge);
                }
                let digest = if is_master_key(&path) {
                    if size > MAX_MASTER_KEY_BYTES || path.components().count() != 2 {
                        return Err(BundleError::UnsafeEntry {
                            name,
                            reason: "not a master key",
                        });
                    }
                    let mut key = zeroize::Zeroizing::new(Vec::new());
                    let digest = copy_out(&mut reader, size, &mut *key)?;
                    let text = String::from_utf8(key.to_vec()).map_err(|_| BundleError::Damaged)?;
                    master_keys.push((path, SecretString::from(text)));
                    digest
                } else {
                    let dest = staging.join(&path);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    let mut file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&dest)
                        .map_err(|_| BundleError::UnsafeEntry {
                            name: name.clone(),
                            reason: "duplicate or clashing entry",
                        })?;
                    let digest = copy_out(&mut reader, size, &mut file)?;
                    file.sync_all()?;
                    digest
                };
                seen.push((name, size, digest));
            }
            TAG_SYMLINK => {
                let (name, _) = read_name_and_size(&mut reader)?;
                return Err(BundleError::UnsafeEntry {
                    name,
                    reason: "symbolic links are refused",
                });
            }
            TAG_MANIFEST => {
                let (_, size) = read_name_and_size(&mut reader)?;
                if size > MAX_MANIFEST_BYTES {
                    return Err(BundleError::TooLarge);
                }
                let mut bytes = Vec::new();
                copy_out(&mut reader, size, &mut bytes)?;
                if read_u8(&mut reader)? != TAG_END {
                    return Err(BundleError::Damaged);
                }
                break bytes;
            }
            _ => return Err(BundleError::Damaged),
        }
    };
    reader.finish()?;

    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| BundleError::ManifestMismatch(format!("unreadable list: {error}")))?;
    if manifest.format_version > FORMAT_VERSION {
        return Err(BundleError::NewerFormat {
            found: manifest.format_version,
            supported: FORMAT_VERSION,
        });
    }
    if manifest.items.len() != seen.len() {
        return Err(BundleError::ManifestMismatch(format!(
            "{} items listed, {} present",
            manifest.items.len(),
            seen.len()
        )));
    }
    for (item, (name, size, digest)) in manifest.items.iter().zip(&seen) {
        let ManifestItem {
            path,
            kind,
            size: listed_size,
            sha256,
        } = item;
        if path != name || listed_size != size || sha256 != digest {
            return Err(BundleError::ManifestMismatch(format!("{name} differs")));
        }
        let entry = entry_path(name)?;
        let kind_ok = match kind {
            ItemKind::SecretsMasterKey => is_master_key(&entry),
            ItemKind::Database => is_database(&entry) && entry.components().count() == 2,
            ItemKind::File => !is_master_key(&entry),
        };
        if !kind_ok {
            return Err(BundleError::ManifestMismatch(format!(
                "{name} has the wrong kind"
            )));
        }
    }
    Ok(Unpacked {
        manifest,
        master_keys,
    })
}

fn rewrite_text_files(staging: &Path, rewrite: &HomePathRewrite) -> anyhow::Result<u64> {
    let mut changed = 0;
    for entry in std::fs::read_dir(staging)? {
        let entry = entry?;
        let path = entry.path();
        let is_text = path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                ext.eq_ignore_ascii_case("toml") || ext.eq_ignore_ascii_case("json")
            });
        let meta = std::fs::symlink_metadata(&path)?;
        if !is_text || !meta.is_file() || meta.len() > MAX_REWRITTEN_TEXT_BYTES {
            continue;
        }
        let Ok(text) = String::from_utf8(std::fs::read(&path)?) else {
            continue; // silent-ok: not text, nothing to rewrite
        };
        if let Some(new) = rewrite.rewrite(&text) {
            std::fs::write(&path, new)
                .with_context(|| format!("could not update {}", path.display()))?;
            changed += 1;
        }
    }
    Ok(changed)
}

fn aside_path(data_folder: &Path, now: chrono::DateTime<chrono::Utc>) -> anyhow::Result<PathBuf> {
    let name = data_folder
        .file_name()
        .and_then(|n| n.to_str())
        .context("the data folder has no name")?;
    let parent = data_folder
        .parent()
        .context("the data folder has no parent")?;
    let stamp = now.format("%Y%m%d-%H%M%S");
    for attempt in 0..100 {
        let candidate = if attempt == 0 {
            parent.join(format!("{name}.before-import-{stamp}"))
        } else {
            parent.join(format!("{name}.before-import-{stamp}-{attempt}"))
        };
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "could not find a free name to move {} aside",
        data_folder.display()
    )
}

fn top_level(folder: &Path) -> anyhow::Result<Vec<std::ffi::OsString>> {
    let mut names = std::fs::read_dir(folder)?
        .map(|entry| entry.map(|e| e.file_name()))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    Ok(names)
}

/// Move staging's entries into the data folder, moving the old contents
/// aside first when `aside` is set. Undoes what it did on failure.
fn move_into_place(staging: &Path, data_folder: &Path, aside: Option<&Path>) -> anyhow::Result<()> {
    let mut moved_aside: Vec<std::ffi::OsString> = Vec::new();
    let mut moved_in: Vec<std::ffi::OsString> = Vec::new();
    let result = (|| -> anyhow::Result<()> {
        if let Some(aside) = aside {
            create_private_dir(aside)?;
            for name in top_level(data_folder)? {
                if name == "bin" {
                    continue;
                }
                std::fs::rename(data_folder.join(&name), aside.join(&name)).with_context(|| {
                    format!("could not move {} aside", data_folder.join(&name).display())
                })?;
                moved_aside.push(name);
            }
        }
        create_private_dir(data_folder)?;
        for name in top_level(staging)? {
            let dest = data_folder.join(&name);
            if dest.exists() {
                anyhow::bail!("{} is in the way", dest.display());
            }
            std::fs::rename(staging.join(&name), &dest)
                .with_context(|| format!("could not move {} into place", dest.display()))?;
            moved_in.push(name);
        }
        Ok(())
    })();
    if result.is_err() {
        for name in moved_in.iter().rev() {
            let _ = std::fs::rename(data_folder.join(name), staging.join(name)); // silent-ok: best-effort rollback; the original error is returned
        }
        if let Some(aside) = aside {
            for name in moved_aside.iter().rev() {
                let _ = std::fs::rename(aside.join(name), data_folder.join(name)); // silent-ok: best-effort rollback; the original error is returned
            }
        }
    }
    result
}

fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .with_context(|| format!("could not create {}", path.display()))
}

/// Import the archive read from `input` into `request.data_folder`. The
/// caller has already checked that no backend is running here.
pub(crate) async fn import_setup(
    input: impl Read,
    request: ImportRequest<'_>,
) -> anyhow::Result<ImportReport> {
    let data_folder = request.data_folder;
    let occupied = has_data(data_folder)?;
    if occupied && !request.replace {
        anyhow::bail!(
            "{} already holds a ZLO setup. Run again with --replace to move it aside to \
             {}.before-import-<time> first (nothing is deleted).",
            data_folder.display(),
            data_folder.display()
        );
    }
    let parent = data_folder
        .parent()
        .context("the data folder must have a parent folder")?;
    create_private_dir(parent)?;

    // Fails on a wrong passphrase before anything is created.
    let reader = BundleReader::open(input, request.passphrase)?;
    let staging = tempfile::Builder::new()
        .prefix(".zlo-import-")
        .tempdir_in(parent)
        .context("could not create a staging folder next to the data folder")?;
    let unpacked = unpack(reader, staging.path())?;
    let manifest = unpacked.manifest;

    for (path, key) in &unpacked.master_keys {
        let root = staging.path().join(path.parent().unwrap_or(Path::new("")));
        create_private_dir(&root)?;
        setup_transfer::install_standalone_secret_master_key(&root, key)
            .with_context(|| format!("could not install the master key for {}", root.display()))?;
    }

    let rewrite = HomePathRewrite::new(
        &manifest.source.data_folder,
        &data_folder.display().to_string(),
        &manifest.source.user_home,
        &request.user_home.display().to_string(),
    );
    let mut paths_rewritten = 0;
    let mut databases = Vec::new();
    let mut environment_records_kept = 0;
    for item in manifest
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::Database)
    {
        let relative = entry_path(&item.path)?;
        let db = staging.path().join(&relative);
        if !rewrite.is_identity() {
            paths_rewritten +=
                setup_transfer::rewrite_standalone_database_paths(&db, &|v| rewrite.rewrite(v))
                    .await
                    .with_context(|| format!("could not rewrite paths in {}", item.path))?;
        }
        let previous = data_folder.join(&relative);
        if occupied && previous.is_file() {
            environment_records_kept += setup_transfer::carry_environment_records(&previous, &db)
                .await
                .with_context(|| {
                    format!("could not keep this computer's identity for {}", item.path)
                })?;
        }
        let summary = setup_transfer::summarize_standalone_database(&db)
            .await
            .with_context(|| format!("could not read {}", item.path))?;
        databases.push((item.path.clone(), summary));
    }
    if !rewrite.is_identity() {
        paths_rewritten += rewrite_text_files(staging.path(), &rewrite)?;
    }

    let aside = if occupied {
        Some(aside_path(data_folder, request.now)?)
    } else {
        None
    };
    move_into_place(staging.path(), data_folder, aside.as_deref())?;

    let mut missing_folders: Vec<String> = databases
        .iter()
        .flat_map(|(_, summary)| summary.referenced_folders.iter().cloned())
        .filter(|folder| {
            let path = match folder.strip_prefix("~/") {
                Some(rest) => request.user_home.join(rest),
                None => PathBuf::from(folder),
            };
            !path.exists()
        })
        .collect();
    missing_folders.sort();
    missing_folders.dedup();

    Ok(ImportReport {
        source: manifest.source,
        created_at: manifest.created_at,
        items: manifest.items.len(),
        bytes: manifest.items.iter().map(|i| i.size).sum(),
        databases,
        master_keys: unpacked.master_keys.len(),
        paths_rewritten,
        moved_aside: aside,
        environment_records_kept,
        missing_folders,
        left_out: manifest.left_out,
    })
}

/// Open the archive file at `path` (bounded: a regular file, not a link).
pub(crate) fn open_archive(path: &Path) -> anyhow::Result<File> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    if !meta.is_file() {
        anyhow::bail!("{} is not a regular file", path.display());
    }
    File::open(path).with_context(|| format!("could not open {}", path.display()))
}
