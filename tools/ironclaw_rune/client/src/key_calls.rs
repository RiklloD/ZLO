//! Typed calls for Settings → Keys (deployment service API keys, operator
//! only) and the local knowledge wiki. A key value goes up on a set and never
//! comes back.

use ironclaw_product_contracts::memory_page::{
    MemoryWikiInitRequest, MemoryWikiInitResponse, MemoryWikiStatus,
};
use ironclaw_product_contracts::service_keys::{
    ServiceKeyIdRequest, ServiceKeyRemoveResponse, ServiceKeySetRequest, ServiceKeySetResponse,
    ServiceKeysListResponse,
};

use crate::api::{Client, V2};
use crate::error::ClientError;

impl Client {
    pub async fn service_keys(&self) -> Result<ServiceKeysListResponse, ClientError> {
        self.get(&format!("{V2}/keys")).await
    }

    /// Verified by the service before it is stored; a rejected key is a
    /// validation error on `value`.
    pub async fn service_key_set(
        &self,
        id: &str,
        value: &str,
    ) -> Result<ServiceKeySetResponse, ClientError> {
        let body = ServiceKeySetRequest {
            id: id.to_string(),
            value: value.to_string(),
        };
        self.post(&format!("{V2}/keys"), &body).await
    }

    /// Store the copy another local agent tool (Hermes) holds.
    pub async fn service_key_import(&self, id: &str) -> Result<ServiceKeySetResponse, ClientError> {
        let body = ServiceKeyIdRequest { id: id.to_string() };
        self.post(&format!("{V2}/keys/import"), &body).await
    }

    pub async fn service_key_remove(
        &self,
        id: &str,
    ) -> Result<ServiceKeyRemoveResponse, ClientError> {
        let body = ServiceKeyIdRequest { id: id.to_string() };
        self.post(&format!("{V2}/keys/remove"), &body).await
    }

    pub async fn memory_wiki_status(&self) -> Result<MemoryWikiStatus, ClientError> {
        self.get(&format!("{V2}/memory/wiki")).await
    }

    /// Scaffold `wiki/SCHEMA.md`, `index.md`, `log.md`; never overwrites.
    pub async fn memory_wiki_init(&self) -> Result<MemoryWikiInitResponse, ClientError> {
        self.post(&format!("{V2}/memory/wiki/init"), &MemoryWikiInitRequest {})
            .await
    }
}
