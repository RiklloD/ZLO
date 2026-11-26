//! Settings → Keys: the deployment's service API keys (not model-provider
//! logins, which the Providers page owns).
//!
//! A *service key* is an operator secret a built-in service needs, declared by
//! the crate that owns the service as a [`ServiceKeyDescriptor`]: its secret
//! handle, a label and what it is for. Composition gathers the descriptors,
//! with an optional [`ServiceKeyVerifier`] each, and hands them to the
//! product decorator that answers these descriptors
//! (`ironclaw_assistant::service_keys`).
//!
//! The key value travels in exactly one direction: into the host, on a set or
//! an import. No view returns it, and errors never carry it.
//!
//! Nothing here names a vendor: the descriptors are data supplied from above.

use async_trait::async_trait;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

use crate::descriptors::{ProductSurfaceCommandDescriptor, ProductView};
use crate::surface::ProductSurfaceCaller;

/// Longest key a client may paste, in bytes.
pub const MAX_SERVICE_KEY_BYTES: usize = 4096;

/// One service key, as its owning crate declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceKeyDescriptor {
    /// The secret handle the key is stored under; also the key's wire id.
    pub id: String,
    /// Shown as the row title.
    pub label: String,
    /// One sentence: what turns on when the key is stored.
    pub purpose: String,
    /// Where to get a key.
    pub docs_url: Option<String>,
    /// The environment variable other local agent tools keep this key in,
    /// looked up by [`ServiceKeyImportSource`]s.
    pub import_env_var: Option<String>,
}

/// Whether a stored key was proven to work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceKeyCheck {
    /// The service accepted the key on an authenticated call.
    Verified,
    /// The service could not be reached; the key is stored untested.
    Unverified,
    /// The key has no verifier.
    NotChecked,
}

/// Why a verifier refused to vouch for a key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServiceKeyVerifyError {
    /// The service answered and refused the key.
    #[error("the service rejected the key")]
    Rejected,
    /// The service could not be asked (network, timeout, 5xx).
    #[error("the service could not be reached: {reason}")]
    Unreachable { reason: String },
}

/// Proves a key works with one authenticated call, and drops any copy of
/// the old key the service caches once the stored key changes.
#[async_trait]
pub trait ServiceKeyVerifier: Send + Sync {
    /// One authenticated call with `key`, as `caller` (whose scope the
    /// egress is audited under).
    async fn verify(
        &self,
        caller: &ProductSurfaceCaller,
        key: &SecretString,
    ) -> Result<(), ServiceKeyVerifyError>;

    /// The stored key was set or removed.
    async fn key_changed(&self);
}

/// A key found in another local agent tool's configuration.
pub struct FoundServiceKey {
    pub value: SecretString,
    /// Where it was found, for the UI ("Hermes (10 profiles)").
    pub source: String,
}

/// Looks for a key other local agent tools already hold.
#[async_trait]
pub trait ServiceKeyImportSource: Send + Sync {
    /// The value of `env_var` in that tool's configuration, when it has
    /// exactly one.
    async fn find(&self, env_var: &str) -> Option<FoundServiceKey>;
}

// --- list ---

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceKeysListRequest {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceKeyStatus {
    pub id: String,
    pub label: String,
    pub purpose: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub docs_url: Option<String>,
    /// A value is stored. The value itself never leaves the host.
    pub stored: bool,
    /// Where an importable copy was found, when none is stored here or it
    /// differs from the stored one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub import_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceKeysListResponse {
    pub keys: Vec<ServiceKeyStatus>,
}

/// `GET /api/webchat/v2/keys`.
pub const SERVICE_KEYS_VIEW: ProductView<ServiceKeysListRequest, ServiceKeysListResponse> =
    ProductView::unpaginated("service_keys.list");

// --- set / import / remove ---

/// `value` is redacted from `Debug`.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceKeySetRequest {
    pub id: String,
    pub value: String,
}

impl std::fmt::Debug for ServiceKeySetRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceKeySetRequest")
            .field("id", &self.id)
            .field("value", &"<redacted>")
            .finish()
    }
}

/// Evidence for a set or an import: the store's read-back and the check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceKeySetResponse {
    pub id: String,
    /// Read back from the store after the write.
    pub stored: bool,
    pub check: ServiceKeyCheck,
    /// Where an imported key came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

pub const SERVICE_KEY_SET_COMMAND_ID: &str = "service_keys.set";
/// `POST /api/webchat/v2/keys`.
pub const SERVICE_KEY_SET_COMMAND: ProductSurfaceCommandDescriptor<
    ServiceKeySetRequest,
    ServiceKeySetResponse,
> = ProductSurfaceCommandDescriptor::new(SERVICE_KEY_SET_COMMAND_ID);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceKeyIdRequest {
    pub id: String,
}

pub const SERVICE_KEY_IMPORT_COMMAND_ID: &str = "service_keys.import";
/// `POST /api/webchat/v2/keys/import`: store the copy
/// [`ServiceKeyStatus::import_from`] names.
pub const SERVICE_KEY_IMPORT_COMMAND: ProductSurfaceCommandDescriptor<
    ServiceKeyIdRequest,
    ServiceKeySetResponse,
> = ProductSurfaceCommandDescriptor::new(SERVICE_KEY_IMPORT_COMMAND_ID);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceKeyRemoveResponse {
    pub id: String,
    /// Read back from the store after the delete.
    pub stored: bool,
}

pub const SERVICE_KEY_REMOVE_COMMAND_ID: &str = "service_keys.remove";
/// `POST /api/webchat/v2/keys/remove`.
pub const SERVICE_KEY_REMOVE_COMMAND: ProductSurfaceCommandDescriptor<
    ServiceKeyIdRequest,
    ServiceKeyRemoveResponse,
> = ProductSurfaceCommandDescriptor::new(SERVICE_KEY_REMOVE_COMMAND_ID);
