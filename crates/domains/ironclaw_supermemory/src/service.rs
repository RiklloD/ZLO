//! The Supermemory operations IronClaw runs: recall, capture, search, write.
//!
//! Every operation first resolves the caller: no API key or no binding means
//! Supermemory is off for that caller and the operation reports `None` /
//! [`SupermemoryError::Disabled`], so the host falls back to local memory.

use std::sync::{Arc, Mutex, PoisonError};

use chrono::Utc;
use ironclaw_product_contracts::operator_secrets::OperatorSecretValueStore;
use secrecy::SecretString;
use serde_json::{Map, Value, json};

use crate::binding::{
    ContainerTag, StaticSupermemoryBindingSource, SupermemoryBindingSource, SupermemoryCaller,
};
use crate::capture::{self, CapturedTurn};
use crate::client::{SOURCE_TAG, SupermemoryClient};
use crate::config::SupermemoryConfig;
use crate::error::SupermemoryError;
use crate::recall::{self, READ_CONTAINER_RECALL_LIMIT, RecallItem};

const GUIDANCE: &str = include_str!("../prompts/guidance.md");
const DEFAULT_SEARCH_LIMIT: usize = 5;
const MAX_SEARCH_LIMIT: usize = 20;
const MAX_WRITE_CHARS: usize = 20_000;

/// The binding plus key for a caller Supermemory is on for.
struct Active {
    key: SecretString,
    primary: ContainerTag,
    readable: Vec<ContainerTag>,
}

/// What recall produced for one turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Recall {
    /// Host-authored guidance naming the containers and tools.
    pub guidance: String,
    pub primary: ContainerTag,
    pub items: Vec<RecallItem>,
    /// Recall was attempted and failed; `items` is empty.
    pub degraded: bool,
}

/// One `supermemory.search` hit.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub id: String,
    pub container: ContainerTag,
    pub content: String,
    pub similarity: Option<f64>,
    pub updated_at: Option<String>,
}

/// Provider evidence for a `supermemory.write`: the document id the API
/// issued and, when the read-back succeeded, the processing status it reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteEvidence {
    pub id: String,
    pub container: ContainerTag,
    pub status: Option<String>,
    /// The document was read back by id after the write.
    pub verified: bool,
}

pub struct Supermemory {
    client: SupermemoryClient,
    bindings: Arc<dyn SupermemoryBindingSource>,
    config: SupermemoryConfig,
    /// Captures whose write failed, retried with the next capture. Never held
    /// across an await.
    pending: Mutex<Vec<CapturedTurn>>,
}

impl std::fmt::Debug for Supermemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supermemory")
            .field("client", &self.client)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Supermemory {
    pub fn new(
        client: SupermemoryClient,
        bindings: Arc<dyn SupermemoryBindingSource>,
        config: SupermemoryConfig,
    ) -> Self {
        Self {
            client,
            bindings,
            config,
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Production assembly: env config, the static env binding and the
    /// hardened egress. The key is read from `keys` on use, so the feature
    /// turns on and off with the secret, without a restart.
    pub fn from_env(
        keys: Arc<dyn OperatorSecretValueStore>,
    ) -> Result<Arc<Self>, SupermemoryError> {
        let config = SupermemoryConfig::from_env()?;
        let bindings = Arc::new(StaticSupermemoryBindingSource::from_env()?);
        let client = SupermemoryClient::production(&config, keys)?;
        Ok(Arc::new(Self::new(client, bindings, config)))
    }

    pub fn config(&self) -> &SupermemoryConfig {
        &self.config
    }

    async fn active(&self, caller: &SupermemoryCaller) -> Result<Option<Active>, SupermemoryError> {
        let Some(key) = self.client.api_key().await? else {
            return Ok(None);
        };
        let Some(binding) = self.bindings.binding_for(caller).await? else {
            return Ok(None);
        };
        Ok(Some(Active {
            key,
            primary: binding.primary()?,
            readable: binding.readable()?,
        }))
    }

    /// Whether Supermemory replaces local memory for this caller. A key or
    /// binding that cannot be read counts as off, so local memory keeps
    /// working when the secret store is unavailable.
    pub async fn is_active(&self, caller: &SupermemoryCaller) -> bool {
        match self.active(caller).await {
            Ok(active) => active.is_some(),
            Err(error) => {
                tracing::warn!(
                    kind = error.kind(),
                    "supermemory state unreadable; using local memory"
                );
                false
            }
        }
    }

    /// Recall for one user message. `Ok(None)` when Supermemory is off for
    /// the caller. A failing primary recall yields a degraded result (guidance
    /// only); a failing read container is skipped.
    pub async fn recall(
        &self,
        caller: &SupermemoryCaller,
        message: &str,
    ) -> Result<Option<Recall>, SupermemoryError> {
        let Some(active) = self.active(caller).await? else {
            return Ok(None);
        };
        let guidance = guidance(&active.primary, &active.readable);
        let empty = |degraded| Recall {
            guidance: guidance.clone(),
            primary: active.primary.clone(),
            items: Vec::new(),
            degraded,
        };
        if !self.config.auto_recall {
            return Ok(Some(empty(false)));
        }
        let Some(query) = recall::recall_query(message) else {
            return Ok(Some(empty(false)));
        };
        let profile_body = json!({"containerTag": active.primary.as_str(), "q": query});
        let key = &active.key;
        let profile = self.client.profile(caller, key, &profile_body);
        let reads = futures::future::join_all(active.readable.iter().map(|container| {
            let body = search_body(&query, container, READ_CONTAINER_RECALL_LIMIT);
            async move {
                let hits = self.client.search(caller, key, &body).await;
                (container.clone(), hits)
            }
        }));
        let (profile, reads) = futures::join!(profile, reads);
        let profile = match profile {
            Ok(profile) => profile,
            Err(error) => {
                tracing::debug!(kind = error.kind(), "supermemory recall failed");
                return Ok(Some(empty(true)));
            }
        };
        let read_hits = reads
            .into_iter()
            .filter_map(|(container, hits)| match hits {
                Ok(hits) => Some((container, hits)),
                Err(error) => {
                    tracing::debug!(%container, kind = error.kind(), "supermemory read-container recall failed");
                    None
                }
            })
            .collect();
        Ok(Some(Recall {
            items: recall::merge(
                &active.primary,
                profile,
                read_hits,
                self.config.max_recall_results,
            ),
            ..empty(false)
        }))
    }

    /// Capture one completed turn into the primary container. Returns whether
    /// anything was written; `false` when off, capture is disabled, or the turn
    /// is empty after cleaning. A failed write is kept for the next capture.
    pub async fn capture(
        &self,
        caller: &SupermemoryCaller,
        thread_key: &str,
        user: &str,
        assistant: &str,
    ) -> Result<bool, SupermemoryError> {
        if !self.config.auto_capture {
            return Ok(false);
        }
        let Some(active) = self.active(caller).await? else {
            return Ok(false);
        };
        let Some(turn) = CapturedTurn::new(thread_key, user, assistant) else {
            return Ok(false);
        };
        let mut turns = std::mem::take(&mut *self.pending_lock());
        turns.push(turn);
        let mut threads: Vec<String> = Vec::new();
        for turn in &turns {
            if !threads.contains(&turn.thread_key) {
                threads.push(turn.thread_key.clone());
            }
        }
        let mut failed = Vec::new();
        let mut current_written = false;
        for thread in threads {
            let batch: Vec<CapturedTurn> = turns
                .iter()
                .filter(|turn| turn.thread_key == thread)
                .cloned()
                .collect();
            let body = capture::capture_body(active.primary.as_str(), &thread, &batch, Utc::now());
            match self.client.add_document(caller, &active.key, &body).await {
                Ok(_) => current_written |= thread == thread_key,
                Err(error) => {
                    tracing::debug!(
                        kind = error.kind(),
                        turns = batch.len(),
                        "supermemory capture failed; kept pending"
                    );
                    failed.extend(batch);
                }
            }
        }
        let mut pending = self.pending_lock();
        // Captures that raced this one were appended meanwhile; keep them after ours.
        failed.append(&mut pending);
        *pending = failed;
        let dropped = capture::bound_pending(&mut pending);
        if dropped > 0 {
            tracing::warn!(dropped, "supermemory dropped oldest pending captures");
        }
        Ok(current_written)
    }

    fn pending_lock(&self) -> std::sync::MutexGuard<'_, Vec<CapturedTurn>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Semantic search in the primary container, or in `container` when it is
    /// one of the caller's readable containers.
    pub async fn search(
        &self,
        caller: &SupermemoryCaller,
        query: &str,
        container: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<SearchHit>, SupermemoryError> {
        let active = self
            .active(caller)
            .await?
            .ok_or(SupermemoryError::Disabled)?;
        let query = query.trim();
        if query.is_empty() {
            return Err(SupermemoryError::InvalidInput {
                reason: "query is required".to_string(),
            });
        }
        let container = match container.map(str::trim).filter(|tag| !tag.is_empty()) {
            None => active.primary.clone(),
            Some(raw) => {
                let tag = ContainerTag::new(raw)?;
                if tag != active.primary && !active.readable.contains(&tag) {
                    return Err(SupermemoryError::InvalidContainer {
                        reason: format!(
                            "'{tag}' is not readable here; allowed: {}",
                            allowed_list(&active.primary, &active.readable)
                        ),
                    });
                }
                tag
            }
        };
        let limit = limit
            .unwrap_or(DEFAULT_SEARCH_LIMIT)
            .clamp(1, MAX_SEARCH_LIMIT);
        let body = search_body(query, &container, limit);
        let hits = self.client.search(caller, &active.key, &body).await?;
        Ok(hits
            .into_iter()
            .map(|hit| SearchHit {
                id: hit.id,
                container: container.clone(),
                content: hit.text,
                similarity: hit.similarity,
                updated_at: hit.updated_at,
            })
            .collect())
    }

    /// Store an explicit memory in the primary container (never a read
    /// container), then read the document back by id as evidence.
    pub async fn write(
        &self,
        caller: &SupermemoryCaller,
        content: &str,
        metadata: Option<&Map<String, Value>>,
    ) -> Result<WriteEvidence, SupermemoryError> {
        let active = self
            .active(caller)
            .await?
            .ok_or(SupermemoryError::Disabled)?;
        let content = content.trim();
        if content.is_empty() {
            return Err(SupermemoryError::InvalidInput {
                reason: "content is required".to_string(),
            });
        }
        if content.chars().count() > MAX_WRITE_CHARS {
            return Err(SupermemoryError::InvalidInput {
                reason: format!("content exceeds {MAX_WRITE_CHARS} characters"),
            });
        }
        let mut meta = Map::new();
        if let Some(extra) = metadata {
            for (key, value) in extra {
                // `sm_source` routes the document; callers cannot re-route it.
                if key != "sm_source" && key != "source" && is_flat_metadata(value) {
                    meta.insert(key.clone(), value.clone());
                }
            }
        }
        meta.entry("type")
            .or_insert_with(|| Value::String(detect_category(content).to_string()));
        meta.insert(
            "sm_source".to_string(),
            Value::String(SOURCE_TAG.to_string()),
        );
        let body = json!({
            "content": content,
            "containerTags": [active.primary.as_str()],
            "entityContext": capture::entity_context(),
            "metadata": meta,
        });
        let created = self.client.add_document(caller, &active.key, &body).await?;
        match self
            .client
            .get_document(caller, &active.key, &created.id)
            .await
        {
            Ok(read_back) if read_back.id == created.id => Ok(WriteEvidence {
                id: created.id,
                container: active.primary,
                status: read_back.status.or(created.status),
                verified: true,
            }),
            outcome => {
                if let Err(error) = outcome {
                    tracing::debug!(kind = error.kind(), "supermemory write read-back failed");
                }
                Ok(WriteEvidence {
                    id: created.id,
                    container: active.primary,
                    status: created.status,
                    verified: false,
                })
            }
        }
    }
}

fn search_body(query: &str, container: &ContainerTag, limit: usize) -> Value {
    json!({
        "q": query,
        "containerTag": container.as_str(),
        "limit": limit,
        "searchMode": "hybrid",
    })
}

fn allowed_list(primary: &ContainerTag, readable: &[ContainerTag]) -> String {
    std::iter::once(primary)
        .chain(readable)
        .map(ContainerTag::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

fn guidance(primary: &ContainerTag, readable: &[ContainerTag]) -> String {
    let readable = if readable.is_empty() {
        "none".to_string()
    } else {
        readable
            .iter()
            .map(ContainerTag::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    };
    GUIDANCE
        .trim()
        .replace("{primary}", primary.as_str())
        .replace("{readable}", &readable)
}

/// Supermemory metadata values are strings, numbers, booleans or string lists.
fn is_flat_metadata(value: &Value) -> bool {
    match value {
        Value::String(_) | Value::Number(_) | Value::Bool(_) => true,
        Value::Array(items) => items.iter().all(Value::is_string),
        _ => false,
    }
}

/// Hermes `_detect_category`: first matching pattern wins.
fn detect_category(text: &str) -> &'static str {
    let lowered = text.to_lowercase();
    let has_word = |word: &str| {
        lowered
            .split(|ch: char| !ch.is_alphanumeric())
            .any(|token| token == word)
    };
    if ["prefer", "like", "love", "hate", "want"]
        .iter()
        .any(|needle| lowered.contains(needle))
    {
        "preference"
    } else if ["decided", "will use", "going with"]
        .iter()
        .any(|needle| lowered.contains(needle))
    {
        "decision"
    } else if ["is", "are", "has", "have"]
        .iter()
        .any(|word| has_word(word))
    {
        "fact"
    } else {
        "other"
    }
}

#[cfg(test)]
mod test_support {
    use super::*;

    impl Supermemory {
        pub(crate) fn pending_len(&self) -> usize {
            self.pending_lock().len()
        }
    }
}
