use std::io::Read;

use ironclaw_extension_contracts::runtime::ExtensionAssetPath;

/// Zip-bomb guards for uploaded extension bundles. The HTTP route caps only
/// the compressed body, so these bounds the archive entries and decompressed
/// bytes held during validation.
pub const MAX_EXTENSION_BUNDLE_FILES: usize = 512;
pub const MAX_EXTENSION_BUNDLE_UNCOMPRESSED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub struct ExtensionBundleError {
    reason: String,
}

impl ExtensionBundleError {
    fn invalid(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// Decode an uploaded extension bundle while enforcing the complete archive
/// boundary: entry count, safe relative paths, duplicate names, and the
/// decompressed byte budget. The returned entries are the exact bytes later
/// validated and materialized by the import path.
pub fn unzip_extension_bundle(
    bundle: &[u8],
) -> Result<Vec<(String, Vec<u8>)>, ExtensionBundleError> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bundle)).map_err(|error| {
        ExtensionBundleError::invalid(format!("uploaded tool bundle is not a valid zip: {error}"))
    })?;
    if archive.len() > MAX_EXTENSION_BUNDLE_FILES {
        return Err(ExtensionBundleError::invalid(format!(
            "uploaded tool bundle contains too many archive entries (limit {MAX_EXTENSION_BUNDLE_FILES})"
        )));
    }
    let mut files = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    let mut total_bytes = 0usize;
    for index in 0..archive.len() {
        let entry = archive.by_index(index).map_err(|error| {
            ExtensionBundleError::invalid(format!(
                "uploaded tool bundle has a corrupt entry: {error}"
            ))
        })?;
        if !entry.is_file() {
            continue;
        }
        let name = entry
            .name()
            .map_err(|error| {
                ExtensionBundleError::invalid(format!(
                    "uploaded tool bundle has an undecodable entry name: {error}"
                ))
            })?
            .into_owned();
        ExtensionAssetPath::new(name.clone()).map_err(|error| {
            ExtensionBundleError::invalid(format!(
                "uploaded tool bundle contains an unsafe path: {name}: {error}"
            ))
        })?;
        // Zip archives may legally repeat an entry name. Rejecting duplicates
        // keeps validation and materialization over the same byte set.
        if !seen_names.insert(name.clone()) {
            return Err(ExtensionBundleError::invalid(format!(
                "uploaded tool bundle contains a duplicate entry: {name}"
            )));
        }
        // Declared zip sizes are attacker-controlled, so bound the actual
        // decompressed stream rather than trusting entry metadata.
        let allowance = MAX_EXTENSION_BUNDLE_UNCOMPRESSED_BYTES - total_bytes;
        let mut bytes = Vec::new();
        entry
            .take(allowance as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| {
                ExtensionBundleError::invalid(format!(
                    "failed to read `{name}` from the uploaded bundle: {error}"
                ))
            })?;
        if bytes.len() > allowance {
            return Err(ExtensionBundleError::invalid(format!(
                "uploaded tool bundle expands past the {MAX_EXTENSION_BUNDLE_UNCOMPRESSED_BYTES}-byte decompressed limit"
            )));
        }
        total_bytes += bytes.len();
        files.push((name, bytes));
    }
    Ok(files)
}

/// Read an extension package from a folder on the local disk while enforcing
/// the same boundary as [`unzip_extension_bundle`]: entry count, safe relative
/// paths, and the total byte budget. The returned entries feed the same import
/// validation an uploaded bundle goes through.
///
/// Symbolic links are refused rather than followed, so a package cannot pull
/// in files from outside its folder. Hidden entries (a name starting with
/// `.`, e.g. `.git`) are skipped: they are authoring state, not package
/// content, and an asset path may not begin with a dot segment anyway.
pub fn read_extension_folder(
    root: &std::path::Path,
) -> Result<Vec<(String, Vec<u8>)>, ExtensionBundleError> {
    let root_meta = std::fs::symlink_metadata(root).map_err(|error| {
        ExtensionBundleError::invalid(format!("cannot read extension folder: {error}"))
    })?;
    if !root_meta.is_dir() {
        return Err(ExtensionBundleError::invalid(
            "extension folder path is not a directory",
        ));
    }
    let mut files = Vec::new();
    let mut total_bytes = 0usize;
    let mut pending = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|error| {
            ExtensionBundleError::invalid(format!("cannot list extension folder: {error}"))
        })?;
        let mut entries = entries.collect::<Result<Vec<_>, _>>().map_err(|error| {
            ExtensionBundleError::invalid(format!("cannot list extension folder: {error}"))
        })?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                return Err(ExtensionBundleError::invalid(
                    "extension folder contains a file name that is not UTF-8",
                ));
            };
            if name.starts_with('.') {
                continue;
            }
            let relative = format!("{prefix}{name}");
            let file_type = entry.file_type().map_err(|error| {
                ExtensionBundleError::invalid(format!("cannot read `{relative}`: {error}"))
            })?;
            if file_type.is_symlink() {
                return Err(ExtensionBundleError::invalid(format!(
                    "extension folder contains a symbolic link: {relative}"
                )));
            }
            if file_type.is_dir() {
                pending.push((entry.path(), format!("{relative}/")));
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            ExtensionAssetPath::new(relative.clone()).map_err(|error| {
                ExtensionBundleError::invalid(format!(
                    "extension folder contains an unsafe path: {relative}: {error}"
                ))
            })?;
            if files.len() >= MAX_EXTENSION_BUNDLE_FILES {
                return Err(ExtensionBundleError::invalid(format!(
                    "extension folder contains too many files (limit {MAX_EXTENSION_BUNDLE_FILES})"
                )));
            }
            let allowance = MAX_EXTENSION_BUNDLE_UNCOMPRESSED_BYTES - total_bytes;
            let file = std::fs::File::open(entry.path()).map_err(|error| {
                ExtensionBundleError::invalid(format!("cannot read `{relative}`: {error}"))
            })?;
            let mut bytes = Vec::new();
            file.take(allowance as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| {
                    ExtensionBundleError::invalid(format!("cannot read `{relative}`: {error}"))
                })?;
            if bytes.len() > allowance {
                return Err(ExtensionBundleError::invalid(format!(
                    "extension folder is larger than the {MAX_EXTENSION_BUNDLE_UNCOMPRESSED_BYTES}-byte limit"
                )));
            }
            total_bytes += bytes.len();
            files.push((relative, bytes));
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an in-memory zip from `(entry_name, bytes)` pairs for the decoder
    /// boundary tests.
    fn zip_bundle(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write;

        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, bytes) in entries {
            writer.start_file(*name, options).expect("start zip entry");
            writer.write_all(bytes).expect("write zip entry");
        }
        writer.finish().expect("finish zip").into_inner()
    }

    fn zip_bundle_with_directories(directory_count: usize) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for index in 0..directory_count {
            writer
                .add_directory(format!("assets/directory-{index}/"), options)
                .expect("start zip directory");
        }
        writer.finish().expect("finish zip").into_inner()
    }

    /// The path contract rejects backslash separators instead of normalizing
    /// them into an accepted path shape.
    #[test]
    fn unzip_extension_bundle_rejects_backslash_entry_names() {
        let bundle = zip_bundle(&[("wasm\\module.wasm", b"x".as_slice())]);
        let error = unzip_extension_bundle(&bundle)
            .expect_err("backslash separators must be rejected, not normalized");
        assert!(
            format!("{error}").contains("unsafe path"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn unzip_extension_bundle_rejects_empty_and_dot_path_segments() {
        for name in ["assets//module.wasm", "assets/./module.wasm"] {
            let bundle = zip_bundle(&[(name, b"x".as_slice())]);
            let error = unzip_extension_bundle(&bundle)
                .expect_err("empty and dot path segments must be rejected");
            assert!(
                format!("{error}").contains("unsafe path"),
                "unexpected error for {name}: {error}"
            );
        }
    }

    /// A small compressed upload must not expand past the decompressed cap.
    #[test]
    fn unzip_extension_bundle_caps_total_decompressed_bytes() {
        let oversized = vec![0u8; MAX_EXTENSION_BUNDLE_UNCOMPRESSED_BYTES + 1];
        let bundle = zip_bundle(&[("payload.bin", oversized.as_slice())]);
        assert!(
            bundle.len() < MAX_EXTENSION_BUNDLE_UNCOMPRESSED_BYTES,
            "test premise: the bomb must be small compressed"
        );
        let error = unzip_extension_bundle(&bundle)
            .expect_err("expansion past the decompressed cap must be rejected");
        assert!(
            format!("{error}").contains("expands past"),
            "unexpected error: {error}"
        );
    }

    /// Duplicate archive entries must never reach validation and materialize
    /// as two different byte values for one path. `ZipWriter` refuses a
    /// duplicate, so patch a same-length placeholder into both ZIP headers.
    #[test]
    fn unzip_extension_bundle_never_returns_duplicate_entry_names() {
        let placeholder = zip_bundle(&[
            ("manifest.toml", b"validated".as_slice()),
            ("manifest.tomX", b"materialized".as_slice()),
        ]);
        let needle = b"manifest.tomX";
        let replacement = b"manifest.toml";
        let mut bundle = placeholder;
        let mut patched = 0;
        let mut index = 0;
        while index + needle.len() <= bundle.len() {
            if &bundle[index..index + needle.len()] == needle {
                bundle[index..index + needle.len()].copy_from_slice(replacement);
                patched += 1;
            }
            index += 1;
        }
        assert!(
            patched >= 2,
            "test premise: placeholder name must appear in both ZIP headers; patched {patched} occurrence(s)"
        );
        match unzip_extension_bundle(&bundle) {
            Err(error) => {
                assert!(
                    format!("{error}").contains("duplicate"),
                    "unexpected error: {error}"
                );
            }
            Ok(files) => {
                let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
                assert_eq!(names, vec!["manifest.toml"]);
            }
        }
    }

    /// Directory entries count toward the archive-entry limit even though
    /// only regular files are returned to the importer.
    #[test]
    fn unzip_extension_bundle_caps_total_entry_count() {
        let bundle = zip_bundle_with_directories(MAX_EXTENSION_BUNDLE_FILES + 1);
        let error =
            unzip_extension_bundle(&bundle).expect_err("entry-count flooding must be rejected");
        assert!(
            format!("{error}").contains("too many archive entries"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn read_extension_folder_reads_nested_files_and_skips_hidden_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("schemas/acme")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("mkdir");
        std::fs::write(dir.path().join("manifest.toml"), b"m").expect("write");
        std::fs::write(dir.path().join("schemas/acme/run.json"), b"{}").expect("write");
        std::fs::write(dir.path().join(".git/HEAD"), b"ref").expect("write");
        let files = read_extension_folder(dir.path()).expect("folder reads");
        let paths: Vec<&str> = files.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(paths, vec!["manifest.toml", "schemas/acme/run.json"]);
    }

    #[cfg(unix)]
    #[test]
    fn read_extension_folder_refuses_symbolic_links() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        std::fs::write(outside.path().join("secret"), b"s").expect("write");
        std::fs::write(dir.path().join("manifest.toml"), b"m").expect("write");
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("leak"))
            .expect("symlink");
        let error = read_extension_folder(dir.path()).expect_err("symlinks are refused");
        assert!(error.reason().contains("symbolic link"), "{error}");
    }

    #[test]
    fn read_extension_folder_enforces_the_file_count_limit() {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..=MAX_EXTENSION_BUNDLE_FILES {
            std::fs::write(dir.path().join(format!("f{index}.txt")), b"x").expect("write");
        }
        let error = read_extension_folder(dir.path()).expect_err("too many files");
        assert!(error.reason().contains("too many files"), "{error}");
    }
}
