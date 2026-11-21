//! [`SkillUsageDocument`]s on the shared persistence plane: one JSON
//! document per owner at `/skill-usage/listing.json`, every change a
//! bounded `cas_update`.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_filesystem::{
    CasApply, CasUpdateError, ContentType, Entry, RootFilesystem, ScopedFilesystem, cas_update,
};
use ironclaw_host_api::{path::ScopedPath, resource::ResourceScope};
use serde::{Deserialize, Serialize};

use super::{
    SkillLedger, SkillPin, SkillUsageDocument, SkillUsageError, SkillUsageName,
    SkillUsageObservation, SkillUsageOwner, SkillUsageSource, SkillUsageStore,
};

/// Where the document lives inside the owner's `/skill-usage` mount.
pub const SKILL_USAGE_DOCUMENT_PATH: &str = "/skill-usage/listing.json";
const SCHEMA_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PersistedV1 {
    schema_version: u8,
    #[serde(flatten)]
    document: SkillUsageDocument,
}

/// [`SkillUsageStore`] over a mount-scoped filesystem. Composition mounts
/// `/skill-usage` per user; this store never chooses a backend.
pub struct FilesystemSkillUsageStore<F>
where
    F: RootFilesystem,
{
    filesystem: Arc<ScopedFilesystem<F>>,
}

impl<F: RootFilesystem> std::fmt::Debug for FilesystemSkillUsageStore<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilesystemSkillUsageStore")
            .finish_non_exhaustive()
    }
}

impl<F> FilesystemSkillUsageStore<F>
where
    F: RootFilesystem,
{
    pub fn new(filesystem: Arc<ScopedFilesystem<F>>) -> Self {
        Self { filesystem }
    }

    /// One read-modify-write of the owner's document. `apply` returns
    /// whether it changed the document; an unchanged one is not written.
    async fn update<A>(
        &self,
        owner: &SkillUsageOwner,
        apply: A,
    ) -> Result<SkillUsageDocument, SkillUsageError>
    where
        A: Fn(&mut SkillUsageDocument) -> Result<bool, SkillUsageError> + Send + Sync,
    {
        let scope = owner_scope(owner);
        let path = document_path()?;
        cas_update(
            self.filesystem.as_ref(),
            &scope,
            &path,
            decode,
            encode,
            |current: Option<SkillUsageDocument>| {
                let mut document = current.unwrap_or_default();
                let changed = apply(&mut document);
                async move {
                    if changed? {
                        Ok(CasApply::new(document.clone(), document))
                    } else {
                        Ok(CasApply::no_op(document.clone(), document))
                    }
                }
            },
        )
        .await
        .map_err(map_cas_error)
    }
}

#[async_trait]
impl<F> SkillUsageStore for FilesystemSkillUsageStore<F>
where
    F: RootFilesystem,
{
    async fn load(
        &self,
        owner: &SkillUsageOwner,
    ) -> Result<Option<SkillUsageDocument>, SkillUsageError> {
        let scope = owner_scope(owner);
        let path = document_path()?;
        let found =
            self.filesystem
                .get(&scope, &path)
                .await
                .map_err(|error| SkillUsageError::Backend {
                    reason: error.to_string(),
                })?;
        match found {
            Some(versioned) => decode(&versioned.entry.body).map(Some),
            None => Ok(Some(SkillUsageDocument::default())),
        }
    }

    async fn record(
        &self,
        owner: &SkillUsageOwner,
        source: &SkillUsageSource,
        observation: SkillUsageObservation,
        now_ms: u64,
    ) -> Result<(), SkillUsageError> {
        if observation.is_empty() {
            return Ok(());
        }
        self.update(owner, |document| {
            document.apply(source, &observation, now_ms)
        })
        .await
        .map(|_| ())
    }

    async fn import_ledger(
        &self,
        owner: &SkillUsageOwner,
        source: &SkillUsageSource,
        ledger: SkillLedger,
    ) -> Result<(), SkillUsageError> {
        if ledger.is_empty() {
            return Ok(());
        }
        self.update(owner, |document| {
            let present = document.sources.contains_key(source);
            if !present && document.sources.len() >= super::SKILL_USAGE_SOURCES_MAX {
                return Err(SkillUsageError::Full { reason: "sources" });
            }
            let record = document.sources.entry(source.clone()).or_default();
            let mut changed = !present;
            for (name, theirs) in &ledger {
                if !record.ledger.contains_key(name)
                    && record.ledger.len() >= super::SKILL_USAGE_LEDGER_MAX
                {
                    continue;
                }
                let entry = record.ledger.entry(name.clone()).or_default();
                let merged = entry.merged(*theirs);
                if merged != *entry {
                    *entry = merged;
                    changed = true;
                }
            }
            Ok(changed)
        })
        .await
        .map(|_| ())
    }

    async fn set_pin(
        &self,
        owner: &SkillUsageOwner,
        name: &SkillUsageName,
        pin: Option<SkillPin>,
    ) -> Result<SkillUsageDocument, SkillUsageError> {
        self.update(owner, |document| document.set_pin(name, pin))
            .await
    }
}

fn owner_scope(owner: &SkillUsageOwner) -> ResourceScope {
    let mut scope = ResourceScope::system();
    scope.tenant_id = owner.tenant_id.clone();
    scope.user_id = owner.user_id.clone();
    scope
}

fn document_path() -> Result<ScopedPath, SkillUsageError> {
    ScopedPath::new(SKILL_USAGE_DOCUMENT_PATH).map_err(|error| SkillUsageError::Backend {
        reason: error.to_string(),
    })
}

fn decode(body: &[u8]) -> Result<SkillUsageDocument, SkillUsageError> {
    let persisted: PersistedV1 =
        serde_json::from_slice(body).map_err(|error| SkillUsageError::Serialization {
            reason: error.to_string(),
        })?;
    if persisted.schema_version != SCHEMA_VERSION {
        return Err(SkillUsageError::Serialization {
            reason: format!("unknown schema version {}", persisted.schema_version),
        });
    }
    Ok(persisted.document)
}

fn encode(document: &SkillUsageDocument) -> Result<Entry, SkillUsageError> {
    let body = serde_json::to_vec(&PersistedV1 {
        schema_version: SCHEMA_VERSION,
        document: document.clone(),
    })
    .map_err(|error| SkillUsageError::Serialization {
        reason: error.to_string(),
    })?;
    Ok(Entry::bytes(body).with_content_type(ContentType::json()))
}

fn map_cas_error(error: CasUpdateError<SkillUsageError>) -> SkillUsageError {
    match error {
        CasUpdateError::Apply(error) => error,
        CasUpdateError::RetriesExhausted | CasUpdateError::Timeout => SkillUsageError::Conflict,
        CasUpdateError::CasUnsupported => SkillUsageError::Backend {
            reason: "backend does not support compare-and-swap".to_string(),
        },
        CasUpdateError::Backend(error) => SkillUsageError::Backend {
            reason: error.to_string(),
        },
    }
}
