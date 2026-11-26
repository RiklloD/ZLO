//! Service keys Hermes already holds: `~/.hermes/.env` and every
//! `~/.hermes/profiles/*/.env`. Read only; Hermes' files are never written.
//! A key is offered only when every file that sets it agrees on one value.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ironclaw_product_contracts::service_keys::{FoundServiceKey, ServiceKeyImportSource};
use secrecy::SecretString;

use super::profiles::ProfileHome;

/// Largest `.env` file read, in bytes; anything bigger is not a dotenv file.
const MAX_ENV_FILE_BYTES: u64 = 256 * 1024;

pub struct HermesKeySource {
    root: PathBuf,
}

impl HermesKeySource {
    pub fn new(home: &ProfileHome) -> Self {
        Self {
            root: home.hermes_root(),
        }
    }

    fn env_files(&self) -> Vec<PathBuf> {
        let mut files = vec![self.root.join(".env")];
        let profiles = self.root.join("profiles");
        match std::fs::read_dir(&profiles) {
            Ok(entries) => {
                let mut dirs: Vec<PathBuf> = entries
                    .filter_map(|entry| entry.ok()) // silent-ok: an unreadable entry holds no key we can offer
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .map(|entry| entry.path().join(".env"))
                    .collect();
                dirs.sort();
                files.extend(dirs);
            }
            Err(error) => tracing::debug!(%error, "hermes profiles folder unreadable"),
        }
        files
    }
}

#[async_trait]
impl ServiceKeyImportSource for HermesKeySource {
    async fn find(&self, env_var: &str) -> Option<FoundServiceKey> {
        let files = self.env_files();
        let env_var = env_var.to_string();
        let found = tokio::task::spawn_blocking(move || {
            let mut values = BTreeSet::new();
            let mut holders = 0usize;
            for file in &files {
                if let Some(value) = read_env_value(file, &env_var) {
                    values.insert(value);
                    holders += 1;
                }
            }
            (values, holders)
        })
        .await;
        let (values, holders) = match found {
            Ok(found) => found,
            Err(error) => {
                tracing::debug!(%error, "hermes key lookup task failed");
                return None;
            }
        };
        if values.len() > 1 {
            tracing::debug!(
                distinct = values.len(),
                "hermes holds different values for one key; not offering any"
            );
            return None;
        }
        let value = values.into_iter().next()?;
        Some(FoundServiceKey {
            value: SecretString::from(value),
            source: if holders == 1 {
                "Hermes".to_string()
            } else {
                format!("Hermes ({holders} profiles)")
            },
        })
    }
}

/// The last non-empty `NAME=value` (or `export NAME=value`) in a dotenv file.
fn read_env_value(file: &Path, name: &str) -> Option<String> {
    let metadata = std::fs::symlink_metadata(file).ok()?; // silent-ok: no .env is the common case
    if !metadata.is_file() || metadata.len() > MAX_ENV_FILE_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(file).ok()?; // silent-ok: an unreadable .env offers nothing
    parse_env_value(&text, name)
}

fn parse_env_value(text: &str, name: &str) -> Option<String> {
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        let line = line
            .strip_prefix("export ")
            .map(str::trim_start)
            .unwrap_or(line);
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != name {
            continue;
        }
        let raw = raw.trim();
        let value = match raw.chars().next() {
            Some(quote @ ('"' | '\'')) => raw
                .strip_prefix(quote)
                .and_then(|rest| rest.split_once(quote))
                .map(|(inner, _)| inner),
            _ => Some(raw.split(" #").next().unwrap_or(raw).trim()),
        };
        match value.map(str::trim) {
            Some(value) if !value.is_empty() => found = Some(value.to_string()),
            _ => found = None,
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn parses_plain_quoted_exported_and_commented_values() {
        assert_eq!(
            parse_env_value("A=1\nKEY=abc\n", "KEY").as_deref(),
            Some("abc")
        );
        assert_eq!(
            parse_env_value("KEY=\"a b\"", "KEY").as_deref(),
            Some("a b")
        );
        assert_eq!(
            parse_env_value("export KEY='x'", "KEY").as_deref(),
            Some("x")
        );
        assert_eq!(parse_env_value("KEY=v # note", "KEY").as_deref(), Some("v"));
        assert_eq!(parse_env_value("# KEY=v\nOTHER=1", "KEY"), None);
        assert_eq!(
            parse_env_value("KEY=v\nKEY=", "KEY"),
            None,
            "last assignment wins"
        );
        assert_eq!(parse_env_value("MYKEY=v", "KEY"), None);
    }

    fn write(dir: &Path, rel: &str, text: &str) {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[tokio::test]
    async fn offers_one_value_shared_by_profiles_and_names_how_many() {
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            ".hermes/profiles/a/.env",
            "SUPERMEMORY_API_KEY=sm_one\n",
        );
        write(
            home.path(),
            ".hermes/profiles/b/.env",
            "SUPERMEMORY_API_KEY=\"sm_one\"\n",
        );
        write(home.path(), ".hermes/profiles/c/.env", "OTHER=1\n");
        let source = HermesKeySource::new(&ProfileHome::new(home.path().to_path_buf()));
        let found = source.find("SUPERMEMORY_API_KEY").await.unwrap();
        assert_eq!(found.value.expose_secret(), "sm_one");
        assert_eq!(found.source, "Hermes (2 profiles)");
        assert!(source.find("MISSING").await.is_none());
    }

    #[tokio::test]
    async fn conflicting_values_offer_nothing() {
        let home = tempfile::tempdir().unwrap();
        write(home.path(), ".hermes/.env", "SUPERMEMORY_API_KEY=sm_one\n");
        write(
            home.path(),
            ".hermes/profiles/a/.env",
            "SUPERMEMORY_API_KEY=sm_two\n",
        );
        let source = HermesKeySource::new(&ProfileHome::new(home.path().to_path_buf()));
        assert!(source.find("SUPERMEMORY_API_KEY").await.is_none());
    }

    #[tokio::test]
    async fn no_hermes_offers_nothing() {
        let home = tempfile::tempdir().unwrap();
        let source = HermesKeySource::new(&ProfileHome::new(home.path().to_path_buf()));
        assert!(source.find("SUPERMEMORY_API_KEY").await.is_none());
    }
}
