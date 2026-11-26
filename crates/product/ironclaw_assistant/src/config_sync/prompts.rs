//! `prompts` domain handler: user-editable `<storage_root>/SYSTEM.md`.
//!
//! Enforces storage-root containment, rejects symlinks, bounds size to
//! [`CONFIG_SYNC_MAX_SYSTEM_PROMPT_BYTES`] (64 KiB), and writes atomically via
//! temp file + rename.

use std::io::{self, Write as _};
use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ironclaw_product_contracts::config_sync::{
    CONFIG_SYNC_MAX_SYSTEM_PROMPT_BYTES, ConfigSyncDomain, ConfigSyncDomainDigest,
    ConfigSyncDomainPayload, PromptsDomainPayload,
};
use uuid::Uuid;

use super::{
    ConfigSyncContext, ConfigSyncDomainHandler, ConfigSyncError, DomainApplyResult,
    canonical_json_sha256,
};

pub struct PromptsDomainHandler {
    storage_root: PathBuf,
    prompt_path: PathBuf,
}

impl PromptsDomainHandler {
    pub fn new(storage_root: PathBuf, prompt_path: PathBuf) -> Self {
        Self {
            storage_root,
            prompt_path,
        }
    }

    fn check_containment(&self) -> Result<(), ConfigSyncError> {
        if !self.prompt_path.starts_with(&self.storage_root) {
            return Err(ConfigSyncError::Store {
                domain: ConfigSyncDomain::Prompts,
                reason: "SYSTEM.md path escapes storage root".to_string(),
            });
        }
        Ok(())
    }

    fn read_system_md(&self) -> Result<(Option<String>, Option<DateTime<Utc>>), ConfigSyncError> {
        self.check_containment()?;
        let meta = match self.prompt_path.symlink_metadata() {
            Ok(meta) => meta,
            Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok((None, None)),
            Err(source) => {
                return Err(ConfigSyncError::Io {
                    domain: ConfigSyncDomain::Prompts,
                    source,
                });
            }
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(ConfigSyncError::Store {
                domain: ConfigSyncDomain::Prompts,
                reason: "SYSTEM.md must be a regular file and not a symlink".to_string(),
            });
        }
        if meta.len() as usize > CONFIG_SYNC_MAX_SYSTEM_PROMPT_BYTES {
            return Err(ConfigSyncError::Store {
                domain: ConfigSyncDomain::Prompts,
                reason: format!(
                    "SYSTEM.md exceeds maximum size ({} > {} bytes)",
                    meta.len(),
                    CONFIG_SYNC_MAX_SYSTEM_PROMPT_BYTES
                ),
            });
        }
        let updated_at = meta.modified().ok().map(DateTime::<Utc>::from);
        let text =
            std::fs::read_to_string(&self.prompt_path).map_err(|source| ConfigSyncError::Io {
                domain: ConfigSyncDomain::Prompts,
                source,
            })?;
        Ok((Some(text), updated_at))
    }

    fn write_system_md_atomic(&self, content: &str) -> Result<(), ConfigSyncError> {
        self.check_containment()?;
        if let Ok(meta) = self.prompt_path.symlink_metadata()
            && (meta.file_type().is_symlink() || !meta.is_file())
        {
            return Err(ConfigSyncError::Store {
                domain: ConfigSyncDomain::Prompts,
                reason: "SYSTEM.md target must be a regular file and not a symlink".to_string(),
            });
        }
        let Some(parent) = self.prompt_path.parent() else {
            return Err(ConfigSyncError::Store {
                domain: ConfigSyncDomain::Prompts,
                reason: "SYSTEM.md path has no parent directory".to_string(),
            });
        };
        std::fs::create_dir_all(parent).map_err(|source| ConfigSyncError::Io {
            domain: ConfigSyncDomain::Prompts,
            source,
        })?;
        let tmp_path = parent.join(format!(".SYSTEM.md.sync.{}.tmp", Uuid::new_v4()));
        let write_result = (|| -> io::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_path)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp_path, &self.prompt_path)?;
            Ok(())
        })();
        if let Err(source) = write_result {
            if let Err(cleanup) = std::fs::remove_file(&tmp_path) {
                tracing::debug!(error = %cleanup, "temporary SYSTEM.md not removed");
            }
            return Err(ConfigSyncError::Io {
                domain: ConfigSyncDomain::Prompts,
                source,
            });
        }
        Ok(())
    }

    fn hash_content(system_md: &Option<String>) -> Result<String, ConfigSyncError> {
        let value = serde_json::json!({ "system_md": system_md });
        canonical_json_sha256(ConfigSyncDomain::Prompts, &value)
    }
}

#[async_trait]
impl ConfigSyncDomainHandler for PromptsDomainHandler {
    fn domain(&self) -> ConfigSyncDomain {
        ConfigSyncDomain::Prompts
    }

    async fn manifest(
        &self,
        _ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainDigest, ConfigSyncError> {
        let (content, updated_at) = self.read_system_md()?;
        let hash = Self::hash_content(&content)?;
        Ok(ConfigSyncDomainDigest {
            hash,
            updated_at,
            item_count: u64::from(content.is_some()),
        })
    }

    async fn export(
        &self,
        _ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainPayload, ConfigSyncError> {
        let (system_md, _) = self.read_system_md()?;
        let hash = Self::hash_content(&system_md)?;
        Ok(ConfigSyncDomainPayload::Prompts(PromptsDomainPayload {
            hash,
            system_md,
        }))
    }

    fn validate(&self, payload: &ConfigSyncDomainPayload) -> Result<(), ConfigSyncError> {
        let ConfigSyncDomainPayload::Prompts(p) = payload else {
            return Err(ConfigSyncError::validation(
                "prompts",
                "expected prompts payload",
            ));
        };
        if let Some(text) = &p.system_md
            && text.len() > CONFIG_SYNC_MAX_SYSTEM_PROMPT_BYTES
        {
            return Err(ConfigSyncError::validation(
                "prompts.system_md",
                format!(
                    "SYSTEM.md size {} bytes exceeds maximum {} bytes",
                    text.len(),
                    CONFIG_SYNC_MAX_SYSTEM_PROMPT_BYTES
                ),
            ));
        }
        Ok(())
    }

    async fn apply(
        &self,
        _ctx: &ConfigSyncContext,
        payload: &ConfigSyncDomainPayload,
    ) -> Result<DomainApplyResult, ConfigSyncError> {
        let ConfigSyncDomainPayload::Prompts(p) = payload else {
            return Err(ConfigSyncError::validation(
                "prompts",
                "expected prompts payload",
            ));
        };
        let Some(incoming) = p.system_md.as_deref() else {
            return Ok(DomainApplyResult::skipped_with_reason(
                "source has no SYSTEM.md",
                0,
            ));
        };
        let (existing, _) = self.read_system_md()?;
        if existing.as_deref() == Some(incoming) {
            return Ok(DomainApplyResult::unchanged(1));
        }
        self.write_system_md_atomic(incoming)?;
        Ok(DomainApplyResult::applied(1, 0))
    }
}
