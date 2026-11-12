//! Extension capability descriptors, the static channel-connection service, and
//! the extension credential setup port (`extensions` charter row).

use super::*;

pub const EXTENSION_INSTALL_CAPABILITY_ID: &str = "builtin.extension_install";

pub const EXTENSION_INSTALL_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(EXTENSION_INSTALL_CAPABILITY_ID);

pub const EXTENSION_REGISTER_HOSTED_MCP_CAPABILITY_ID: &str =
    "builtin.extension_register_hosted_mcp";

pub const EXTENSION_REGISTER_HOSTED_MCP_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(EXTENSION_REGISTER_HOSTED_MCP_CAPABILITY_ID);

pub const EXTENSION_IMPORT_CAPABILITY_ID: &str = "builtin.extension_import";

pub const EXTENSION_IMPORT_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(EXTENSION_IMPORT_CAPABILITY_ID);

pub const EXTENSION_ACTIVATE_CAPABILITY_ID: &str = "builtin.extension_activate";

pub const EXTENSION_ACTIVATE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(EXTENSION_ACTIVATE_CAPABILITY_ID);

pub const EXTENSION_REMOVE_CAPABILITY_ID: &str = "builtin.extension_remove";

pub const EXTENSION_REMOVE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(EXTENSION_REMOVE_CAPABILITY_ID);

pub const EXTENSION_SETUP_SUBMIT_CAPABILITY_ID: &str = "builtin.extension_setup_submit";

pub const EXTENSION_SETUP_SUBMIT_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(EXTENSION_SETUP_SUBMIT_CAPABILITY_ID);

#[derive(Debug, Clone, Default)]
pub struct StaticChannelConnectionService;

#[async_trait]
impl ChannelConnectionService for StaticChannelConnectionService {
    async fn caller_channel_connections(
        &self,
        _caller: ProductSurfaceCaller,
    ) -> Result<
        std::collections::HashMap<ironclaw_host_api::ids::ExtensionId, bool>,
        ProductSurfaceError,
    > {
        Ok(std::collections::HashMap::new())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionCredentialStatusRequest {
    pub scope: AuthProductScope,
    pub provider: AuthProviderId,
    pub setup: crate::LifecycleExtensionCredentialSetup,
    pub provider_scopes: Vec<ProviderScope>,
    pub requester_extension: ExtensionId,
}

#[derive(Debug)]
pub struct ExtensionCredentialSubmitRequest {
    pub scope: AuthProductScope,
    pub provider: AuthProviderId,
    pub label: String,
    pub requester_extension: ExtensionId,
    pub existing_account: Option<CredentialAccountUpdateBinding>,
    pub secret: SecretString,
}

#[async_trait]
pub trait ExtensionCredentialSetupService: Send + Sync {
    async fn credential_status(
        &self,
        request: ExtensionCredentialStatusRequest,
    ) -> Result<Option<CredentialAccountProjection>, ProductSurfaceError>;

    async fn submit_manual_token(
        &self,
        request: ExtensionCredentialSubmitRequest,
    ) -> Result<CredentialAccountId, ProductSurfaceError>;
}

pub(super) fn parse_credential_account_id(
    value: &str,
) -> Result<CredentialAccountId, ProductSurfaceFailure> {
    uuid::Uuid::parse_str(value)
        .map(CredentialAccountId::from_uuid)
        .map_err(|_| ProductSurfaceFailure::AuthInteractionRejected {
            kind: AuthInteractionRejectionKind::InvalidCredentialRef,
        })
}
