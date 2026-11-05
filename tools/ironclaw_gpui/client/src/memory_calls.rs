//! Typed calls for the Memory page (local memory documents and the
//! Supermemory account) and the read-only hooks list. Request and response
//! bodies are the gateway's own contract types; the Supermemory key never
//! comes back from any of these.

use ironclaw_product_contracts::hooks_catalog::HookCatalog;
use ironclaw_product_contracts::memory_page::{
    MemoryArchiveRequest, MemoryArchiveResponse, MemoryDocument, MemorySearchResponse,
    MemoryTreeResponse, MemoryWriteRequest, MemoryWriteResponse,
    SupermemoryDeleteDocumentRequest, SupermemoryDeleteDocumentResponse, SupermemoryDocumentPage,
    SupermemoryForgetRequest, SupermemoryForgetResponse, SupermemorySearchResponse,
    SupermemoryStatus,
};

use crate::api::{Client, V2, seg};
use crate::error::ClientError;

fn push_param(url: &mut String, key: &str, value: Option<&str>) {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return;
    };
    url.push(if url.contains('?') { '&' } else { '?' });
    url.push_str(key);
    url.push('=');
    url.push_str(&seg(value));
}

impl Client {
    // --- local memory documents ---

    pub async fn memory_tree(&self) -> Result<MemoryTreeResponse, ClientError> {
        self.get(&format!("{V2}/memory/documents")).await
    }

    pub async fn memory_document(&self, path: &str) -> Result<MemoryDocument, ClientError> {
        self.get(&format!(
            "{V2}/memory/documents/content?path={}",
            seg(path)
        ))
        .await
    }

    pub async fn memory_search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<MemorySearchResponse, ClientError> {
        self.get(&format!(
            "{V2}/memory/search?q={}&limit={limit}",
            seg(query)
        ))
        .await
    }

    /// Full-document save. `expected_content_hash` is the hash the read
    /// returned (`None` creates); a changed document answers `conflict`.
    pub async fn memory_write(
        &self,
        path: &str,
        content: &str,
        expected_content_hash: Option<&str>,
    ) -> Result<MemoryWriteResponse, ClientError> {
        let body = MemoryWriteRequest {
            path: path.to_string(),
            content: content.to_string(),
            expected_content_hash: expected_content_hash.map(str::to_string),
        };
        self.post(&format!("{V2}/memory/documents"), &body).await
    }

    /// "Remove": moves the document into `archive/`; nothing is deleted.
    pub async fn memory_archive(
        &self,
        path: &str,
        expected_content_hash: &str,
    ) -> Result<MemoryArchiveResponse, ClientError> {
        let body = MemoryArchiveRequest {
            path: path.to_string(),
            expected_content_hash: expected_content_hash.to_string(),
        };
        self.post(&format!("{V2}/memory/documents/archive"), &body)
            .await
    }

    // --- Supermemory (remote) ---

    pub async fn supermemory_status(
        &self,
        profile_id: Option<&str>,
    ) -> Result<SupermemoryStatus, ClientError> {
        let mut url = format!("{V2}/memory/supermemory");
        push_param(&mut url, "profile_id", profile_id);
        self.get(&url).await
    }

    pub async fn supermemory_documents(
        &self,
        profile_id: Option<&str>,
        container: Option<&str>,
        page: u32,
    ) -> Result<SupermemoryDocumentPage, ClientError> {
        let mut url = format!("{V2}/memory/supermemory/documents?page={page}");
        push_param(&mut url, "profile_id", profile_id);
        push_param(&mut url, "container", container);
        self.get(&url).await
    }

    pub async fn supermemory_search(
        &self,
        profile_id: Option<&str>,
        query: &str,
        container: Option<&str>,
    ) -> Result<SupermemorySearchResponse, ClientError> {
        let mut url = format!("{V2}/memory/supermemory/search?q={}", seg(query));
        push_param(&mut url, "profile_id", profile_id);
        push_param(&mut url, "container", container);
        self.get(&url).await
    }

    pub async fn supermemory_delete_document(
        &self,
        profile_id: Option<&str>,
        id: &str,
    ) -> Result<SupermemoryDeleteDocumentResponse, ClientError> {
        let body = SupermemoryDeleteDocumentRequest {
            profile_id: profile_id.map(str::to_string),
            id: id.to_string(),
        };
        self.post(&format!("{V2}/memory/supermemory/documents/delete"), &body)
            .await
    }

    pub async fn supermemory_forget(
        &self,
        profile_id: Option<&str>,
        container: &str,
        id: &str,
    ) -> Result<SupermemoryForgetResponse, ClientError> {
        let body = SupermemoryForgetRequest {
            profile_id: profile_id.map(str::to_string),
            container: container.to_string(),
            id: id.to_string(),
        };
        self.post(&format!("{V2}/memory/supermemory/memories/forget"), &body)
            .await
    }

    // --- hooks ---

    pub async fn hooks_catalog(&self) -> Result<HookCatalog, ClientError> {
        self.get(&format!("{V2}/hooks")).await
    }
}

#[cfg(test)]
mod tests {
    use super::push_param;

    #[test]
    fn params_are_skipped_when_empty_and_encoded_when_set() {
        let mut url = "/x".to_string();
        push_param(&mut url, "profile_id", None);
        push_param(&mut url, "container", Some(""));
        assert_eq!(url, "/x");
        push_param(&mut url, "profile_id", Some("work"));
        push_param(&mut url, "container", Some("a b"));
        assert!(url.starts_with("/x?profile_id=work&container="));
        assert!(!url.contains(' '));
    }
}
