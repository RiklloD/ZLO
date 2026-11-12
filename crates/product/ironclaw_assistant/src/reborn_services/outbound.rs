//! Outbound notification-preference port and its command descriptor
//! (`outbound` charter row).

use super::*;

/// WebUI-facing full-replace notification-channels write. See
/// `outbound_views::RebornServices::set_notification_channels`'s doc comment
/// for why this is a product command (webui-only, no approval gate) rather
/// than a composition-registered capability like the sibling model-callable
/// `builtin.notification_channels_set` tool.
pub const NOTIFICATION_CHANNELS_SET_COMMAND_ID: &str = "outbound.notification_channels_set";

pub const NOTIFICATION_CHANNELS_SET_COMMAND: ProductSurfaceCommandDescriptor<
    RebornSetNotificationChannelsRequest,
    RebornNotificationChannelsResponse,
> = ProductSurfaceCommandDescriptor::new(NOTIFICATION_CHANNELS_SET_COMMAND_ID);

#[async_trait]
pub trait OutboundPreferencesProductService: Send + Sync {
    /// List delivery targets available to the authenticated caller.
    ///
    /// Implementations must scope target inventory by the caller's tenant/user
    /// identity. `RebornServices` installs
    /// `UnsupportedOutboundPreferencesProductService` by default, which keeps
    /// Phase 1 target discovery fail-closed with a non-retryable
    /// service-unavailable response until a real service is wired.
    async fn list_outbound_delivery_targets(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<RebornOutboundDeliveryTargetListResponse, ProductSurfaceError>;

    /// Persist the caller's scoped notification-channel target list — a full
    /// replace of the current set (spec §7). An empty list means notifications
    /// stay in the web app only.
    ///
    /// Implementations must scope writes by the caller's tenant/user identity,
    /// validate each id through the caller-scoped target registry, dedup
    /// preserving order, and cap at `NOTIFICATION_TARGETS_CAP`. Defaults to a
    /// non-retryable service-unavailable response so the pre-existing
    /// implementors of this trait that predate notification channels do not
    /// need to opt in explicitly.
    async fn set_notification_channels(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornSetNotificationChannelsRequest,
    ) -> Result<RebornNotificationChannelsResponse, ProductSurfaceError> {
        let _ = (caller, request);
        Err(outbound_preferences_unavailable())
    }

    /// Return the authenticated caller's scoped notification-channel targets,
    /// resolved to channel options. Stored ids that no longer resolve are
    /// omitted rather than erroring (see
    /// [`RebornNotificationChannelsResponse`]).
    ///
    /// Implementations must scope reads by the caller's tenant/user identity.
    /// Defaults to an empty projection, so "not configured yet" is a stable
    /// read state.
    async fn get_notification_channels(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<RebornNotificationChannelsResponse, ProductSurfaceError> {
        let _ = caller;
        Ok(RebornNotificationChannelsResponse::default())
    }
}

#[derive(Debug)]
pub struct UnsupportedOutboundPreferencesProductService;

impl UnsupportedOutboundPreferencesProductService {
    pub fn new_static() -> Self {
        Self
    }
}

#[async_trait]
impl OutboundPreferencesProductService for UnsupportedOutboundPreferencesProductService {
    async fn list_outbound_delivery_targets(
        &self,
        _caller: ProductSurfaceCaller,
    ) -> Result<RebornOutboundDeliveryTargetListResponse, ProductSurfaceError> {
        Err(outbound_preferences_unavailable())
    }

    async fn set_notification_channels(
        &self,
        _caller: ProductSurfaceCaller,
        _request: RebornSetNotificationChannelsRequest,
    ) -> Result<RebornNotificationChannelsResponse, ProductSurfaceError> {
        Err(outbound_preferences_unavailable())
    }

    async fn get_notification_channels(
        &self,
        _caller: ProductSurfaceCaller,
    ) -> Result<RebornNotificationChannelsResponse, ProductSurfaceError> {
        Ok(RebornNotificationChannelsResponse::default())
    }
}

pub(super) fn outbound_preferences_unavailable() -> ProductSurfaceError {
    ProductSurfaceError::service_unavailable(false)
}
