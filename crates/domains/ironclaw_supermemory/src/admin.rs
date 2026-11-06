//! What the Memory page does with Supermemory: show the binding, list the
//! captured documents, search memories, delete a wrong document and forget a
//! wrong memory.
//!
//! Every operation takes an explicit binding (the page names a profile, not a
//! thread) or resolves the caller's own. Reads may target any bound
//! container; deletes and forgets only the primary container, the one IronClaw
//! writes — read containers are never written, and removing from them is a
//! write. Remote user data, not IronClaw LLM history: deleting here is the
//! user correcting their own Supermemory account.

use serde_json::json;

use crate::binding::{ContainerTag, SupermemoryBinding, SupermemoryCaller};
use crate::error::SupermemoryError;
use crate::service::{SearchHit, Supermemory};
use crate::wire::WireDocument;

const DEFAULT_PAGE_LIMIT: u32 = 25;
const MAX_PAGE_LIMIT: u32 = 100;
const PREVIEW_CHARS: usize = 280;
const FORGET_REASON: &str = "removed by the user from the ZLO Memory page";

/// The binding as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminBinding {
    pub primary: ContainerTag,
    pub readable: Vec<ContainerTag>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminStatus {
    /// An API key is stored (its value is never returned).
    pub key_configured: bool,
    /// The validated binding, when there is one.
    pub binding: Option<AdminBinding>,
    pub auto_recall: bool,
    pub auto_capture: bool,
}

impl AdminStatus {
    /// Supermemory replaces local memory under this binding.
    pub fn active(&self) -> bool {
        self.key_configured && self.binding.is_some()
    }
}

/// One captured document, trimmed for a list row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentSummary {
    pub id: String,
    pub custom_id: Option<String>,
    pub title: Option<String>,
    pub preview: Option<String>,
    pub status: Option<String>,
    pub container_tags: Vec<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentPage {
    pub container: ContainerTag,
    pub documents: Vec<DocumentSummary>,
    pub page: u32,
    pub total_pages: Option<u32>,
    pub total_items: Option<u64>,
}

/// Evidence for a delete: the id, and whether a read by id afterwards no
/// longer found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteEvidence {
    pub id: String,
    pub verified: bool,
}

/// Evidence for a forget: the API's own `forgotten` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetEvidence {
    pub id: String,
    pub forgotten: bool,
}

fn preview(document: &WireDocument) -> Option<String> {
    let text = document
        .summary
        .as_deref()
        .or(document.content.as_deref())?
        .trim();
    if text.is_empty() {
        return None;
    }
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(PREVIEW_CHARS).collect();
    Some(if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    })
}

impl Supermemory {
    /// Key presence plus the binding `binding` (or the caller's own) resolves
    /// to. A binding with an invalid container tag is an error, not "off".
    pub async fn admin_status(
        &self,
        caller: &SupermemoryCaller,
        binding: Option<&SupermemoryBinding>,
    ) -> Result<AdminStatus, SupermemoryError> {
        let key_configured = self.client().api_key().await?.is_some();
        let binding = match binding {
            Some(binding) => Some(binding.clone()),
            None => self.resolve_binding(caller).await?,
        };
        let binding = match binding {
            Some(binding) => Some(AdminBinding {
                primary: binding.primary()?,
                readable: binding.readable()?,
            }),
            None => None,
        };
        Ok(AdminStatus {
            key_configured,
            binding,
            auto_recall: self.config().auto_recall,
            auto_capture: self.config().auto_capture,
        })
    }

    /// One page of the documents in `container` (a bound container; the
    /// primary when `None`), most recently updated first.
    pub async fn list_documents(
        &self,
        caller: &SupermemoryCaller,
        binding: Option<&SupermemoryBinding>,
        container: Option<&str>,
        page: Option<u32>,
        limit: Option<u32>,
    ) -> Result<DocumentPage, SupermemoryError> {
        let active = self
            .active_with(caller, binding)
            .await?
            .ok_or(SupermemoryError::Disabled)?;
        let container = active.container(container)?;
        let page = page.unwrap_or(1).max(1);
        let limit = limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT);
        let body = json!({
            "containerTags": [container.as_str()],
            "limit": limit,
            "page": page,
            "sort": "updatedAt",
            "order": "desc",
        });
        let listed = self
            .client()
            .list_documents(caller, &active.key, &body)
            .await?;
        Ok(DocumentPage {
            documents: listed
                .documents
                .iter()
                .map(|document| DocumentSummary {
                    id: document.id.clone(),
                    custom_id: document.custom_id.clone(),
                    title: document.title.clone(),
                    preview: preview(document),
                    status: document.status.clone(),
                    container_tags: document.container_tags.clone(),
                    created_at: document.created_at.clone(),
                    updated_at: document.updated_at.clone(),
                })
                .collect(),
            container,
            page: listed.current_page.unwrap_or(page),
            total_pages: listed.total_pages,
            total_items: listed.total_items,
        })
    }

    /// Semantic search for the page; same container rules as the tool.
    pub async fn search_memories(
        &self,
        caller: &SupermemoryCaller,
        binding: Option<&SupermemoryBinding>,
        query: &str,
        container: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<SearchHit>, SupermemoryError> {
        self.search_with(caller, binding, query, container, limit)
            .await
    }

    /// Delete document `id`, which must live in the primary container and no
    /// other (delete-by-id is global). The document is read first; read back
    /// afterwards, a 404 is the evidence.
    pub async fn delete_document(
        &self,
        caller: &SupermemoryCaller,
        binding: Option<&SupermemoryBinding>,
        id: &str,
    ) -> Result<DeleteEvidence, SupermemoryError> {
        let active = self
            .active_with(caller, binding)
            .await?
            .ok_or(SupermemoryError::Disabled)?;
        let id = id.trim();
        let document = self.client().get_document(caller, &active.key, id).await?;
        // Delete-by-id removes the document from every container it is in,
        // and the API has no container-scoped or conditional delete. So only
        // a document that lives in the main container ALONE may go: one
        // shared with another container (or with no tags to check) is
        // refused rather than removed from places this profile never writes.
        // Residual race, documented: tags could change between this read and
        // the delete; Supermemory offers no precondition to close it.
        let only_primary = !document.container_tags.is_empty()
            && document
                .container_tags
                .iter()
                .all(|tag| tag == active.primary.as_str());
        if !only_primary {
            return Err(SupermemoryError::InvalidContainer {
                reason: format!(
                    "document is not only in '{}', the container this profile writes",
                    active.primary
                ),
            });
        }
        self.client()
            .delete_document(caller, &active.key, &document.id)
            .await?;
        let verified = match self
            .client()
            .get_document(caller, &active.key, &document.id)
            .await
        {
            Err(SupermemoryError::Http { status: 404 }) => true,
            Ok(_) => false,
            Err(error) => {
                tracing::debug!(kind = error.kind(), "supermemory delete read-back failed");
                false
            }
        };
        Ok(DeleteEvidence {
            id: document.id,
            verified,
        })
    }

    /// Forget memory `id` in `container`, which must be the primary.
    pub async fn forget_memory(
        &self,
        caller: &SupermemoryCaller,
        binding: Option<&SupermemoryBinding>,
        container: &str,
        id: &str,
    ) -> Result<ForgetEvidence, SupermemoryError> {
        let active = self
            .active_with(caller, binding)
            .await?
            .ok_or(SupermemoryError::Disabled)?;
        let container = ContainerTag::new(container.trim())?;
        if container != active.primary {
            return Err(SupermemoryError::InvalidContainer {
                reason: format!(
                    "only '{}', the container this profile writes, can be changed",
                    active.primary
                ),
            });
        }
        let id = id.trim();
        if id.is_empty() || id.chars().count() > 200 || id.chars().any(char::is_control) {
            return Err(SupermemoryError::InvalidInput {
                reason: "memory id is required".to_string(),
            });
        }
        let body = json!({
            "containerTag": container.as_str(),
            "id": id,
            "reason": FORGET_REASON,
        });
        let response = self
            .client()
            .forget_memory(caller, &active.key, &body)
            .await?;
        let forgotten = response
            .get("forgotten")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let id = response
            .get("id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| id.to_string());
        Ok(ForgetEvidence { id, forgotten })
    }
}
