//! Moving a ZLO setup to another computer: the `.zlobundle` archive that
//! `zlo export` writes and `zlo import` restores (`zlo transfer` does both
//! over SSH). The commands in `commands/{export,import,transfer}.rs` are
//! thin; the work is here:
//!
//! - [`format`]: the encrypted container (passphrase → PBKDF2 → chunked
//!   AES-256-GCM) and its record stream.
//! - [`export`]: what is taken from the data folder and how.
//! - [`import`]: verification into a staging folder, then the move into
//!   place.
//! - [`paths`]: the home-path rewrite.
//! - [`passphrase`]: prompting or reading the passphrase (never argv).
//!
//! The database snapshot, environment-record scrub, record path rewrite and
//! the secrets master key are composition's (`ironclaw_composition::setup_transfer`).

pub(crate) mod export;
pub(crate) mod format;
pub(crate) mod import;
pub(crate) mod passphrase;
pub(crate) mod paths;
pub(crate) mod ssh;

#[cfg(test)]
mod round_trip_tests;

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Why an archive could not be written or read, in words for the person.
#[derive(Debug, thiserror::Error)]
pub(crate) enum BundleError {
    #[error("this is not a ZLO setup file")]
    NotABundle,
    #[error(
        "this setup file was made by a newer ZLO (format {found}; this one reads up to \
         {supported}). Update ZLO on this computer first."
    )]
    NewerFormat { found: u16, supported: u16 },
    #[error("the passphrase is wrong, or the file is damaged")]
    WrongPassphraseOrDamaged,
    #[error("the setup file is damaged or was changed after it was made")]
    Damaged,
    #[error("the setup file is cut short (an incomplete copy?)")]
    Truncated,
    #[error("the setup file is larger than ZLO accepts")]
    TooLarge,
    #[error("the setup file holds an entry ZLO refuses ({name}): {reason}")]
    UnsafeEntry { name: String, reason: &'static str },
    #[error("the setup file's contents do not match its list of items: {0}")]
    ManifestMismatch(String),
    #[error("encryption failed")]
    Crypto,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Bounds on what an import accepts.
pub(crate) const MAX_ITEMS: usize = 200_000;
pub(crate) const MAX_ITEM_BYTES: u64 = 64 * 1024 * 1024 * 1024;
pub(crate) const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024 * 1024;
pub(crate) const MAX_MANIFEST_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const MAX_MASTER_KEY_BYTES: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ItemKind {
    /// A file copied from the data folder.
    File,
    /// A consistent snapshot of a storage root's database.
    Database,
    /// A storage root's secrets master key.
    SecretsMasterKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ManifestItem {
    /// Relative to the data folder, `/`-separated.
    pub(crate) path: String,
    pub(crate) kind: ItemKind,
    pub(crate) size: u64,
    /// Lowercase hex SHA-256 of the bytes.
    pub(crate) sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SourceInfo {
    pub(crate) os: String,
    pub(crate) arch: String,
    pub(crate) hostname: String,
    pub(crate) user_home: String,
    pub(crate) data_folder: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ZloBuild {
    pub(crate) version: String,
    pub(crate) commit: Option<String>,
}

/// The list of what an archive holds, written last inside the encrypted
/// stream and checked item by item on import.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub(crate) format_version: u16,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) source: SourceInfo,
    pub(crate) zlo: ZloBuild,
    pub(crate) items: Vec<ManifestItem>,
    /// Plain-words notes on what was deliberately not taken.
    pub(crate) left_out: Vec<String>,
}

impl ZloBuild {
    pub(crate) fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: option_env!("IRONCLAW_BUILD_COMMIT").map(str::to_string),
        }
    }
}

/// Check an entry name and turn it into a relative path: `/`-separated
/// plain components only (no `..`, `.`, empty, absolute, drive or
/// backslash parts), bounded length.
pub(crate) fn entry_path(name: &str) -> Result<PathBuf, BundleError> {
    let refuse = |reason| BundleError::UnsafeEntry {
        name: name.chars().take(80).collect(),
        reason,
    };
    if name.is_empty() || name.len() > format::MAX_NAME_LEN {
        return Err(refuse("bad name length"));
    }
    if name.contains(['\\', '\0', ':']) {
        return Err(refuse("forbidden character"));
    }
    let mut path = PathBuf::new();
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(refuse("path escapes the data folder"));
        }
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => path.push(part),
            _ => return Err(refuse("path escapes the data folder")),
        }
    }
    Ok(path)
}

/// `path` relative to `root` as an entry name, when it is plain UTF-8.
pub(crate) fn entry_name(relative: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

pub(crate) fn sha256_hex(digest: impl AsRef<[u8]>) -> String {
    hex::encode(digest)
}

/// The OS name the manifest records.
pub(crate) fn current_os() -> &'static str {
    std::env::consts::OS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_names_must_stay_inside_the_data_folder() {
        assert_eq!(
            entry_path("local-dev/reborn-local-dev.db").expect("ok"),
            PathBuf::from("local-dev").join("reborn-local-dev.db")
        );
        for bad in [
            "",
            "/etc/passwd",
            "../outside",
            "a/../../b",
            "a/./b",
            "a//b",
            "a\\b",
            "C:/x",
            "trailing/",
            "nul\0byte",
        ] {
            assert!(entry_path(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(entry_path(&"a".repeat(format::MAX_NAME_LEN + 1)).is_err());
    }
}
