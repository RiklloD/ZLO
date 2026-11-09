//! Remote-environment wiring for `zlo serve`: the environment-access
//! routes, the device-token authenticator, and — while `[remote]` is
//! enabled — the loopback identity listener `tailscale serve` targets.
//!
//! `serve.rs` calls these in order; the routes and authenticator are
//! always mounted (one-time-code pairing needs no Tailscale), the identity
//! listener only when `[remote] enabled = true`. Both listeners are
//! loopback: the identity listener always binds `127.0.0.1`, and the main
//! listener keeps `serve`'s own loopback rule unchanged.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::Context as _;
use ironclaw_composition::host_api::UserId;
use ironclaw_config::{RebornConfigFile, RemoteSettings};
use ironclaw_product_contracts::environment_access::EnvironmentAccessService;
use ironclaw_webui::{
    BoundWebuiListener, DeviceTokenAuthenticator, EnvironmentAccessRouteConfig,
    RebornWebuiServeError, RemoteAccessPolicy, SplitRouteMount, WebuiAuthenticator, bind_webui_v2,
    environment_access_route_mount,
};

/// `[remote]` resolved for this start, with the local-account check done.
pub(crate) struct RemoteServePlan {
    settings: RemoteSettings,
    one_click: bool,
}

impl RemoteServePlan {
    /// `None` unless `[remote] enabled = true`. Other OS login accounts are
    /// checked on every start (one created after `remote enable` still turns
    /// one-click pairing off); when the check itself fails, one-click stays
    /// off rather than trusting an unknown machine.
    pub(crate) fn from_config(config_file: Option<&RebornConfigFile>) -> Option<Self> {
        let settings = config_file
            .and_then(|file| file.remote.as_ref())
            .and_then(|remote| remote.resolve())?;
        let other_accounts = match crate::host_info::detect_other_login_accounts() {
            Ok(accounts) => !accounts.is_empty(),
            Err(error) => {
                tracing::warn!(
                    target: "ironclaw::reborn::cli::serve",
                    %error,
                    "could not check for other local login accounts; one-click tailnet pairing stays off"
                );
                true
            }
        };
        Some(Self {
            one_click: settings.one_click(other_accounts),
            settings,
        })
    }

    fn policy(&self) -> RemoteAccessPolicy {
        RemoteAccessPolicy {
            tailnet_url: self.settings.tailnet_url.clone(),
            owner_logins: self.settings.owner_logins.clone(),
            one_click: self.one_click,
        }
    }

    fn identity_addr(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.settings.identity_port))
    }
}

/// The device-token authenticator in front of `fallback` (the master token
/// and session chain). A live `icd_` token is the operator, like the master
/// token.
pub(crate) fn device_token_authenticator(
    service: Arc<dyn EnvironmentAccessService>,
    operator: UserId,
    fallback: Arc<dyn WebuiAuthenticator>,
) -> Arc<dyn WebuiAuthenticator> {
    Arc::new(DeviceTokenAuthenticator::new(service, operator, fallback))
}

/// The descriptor, pairing and device routes. The descriptor also says
/// which commit this build is and whether `[update]` is set up.
pub(crate) fn route_mount(
    service: Arc<dyn EnvironmentAccessService>,
    plan: Option<&RemoteServePlan>,
    self_update: &crate::commands::update::SourceSelfUpdate,
) -> SplitRouteMount {
    environment_access_route_mount(EnvironmentAccessRouteConfig {
        service,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        remote: plan.map(RemoteServePlan::policy),
        build_commit: crate::commands::update::current_build().commit,
        self_update: self_update.configured(),
    })
}

/// Bind `127.0.0.1:<identity_port>`. Fails the start when the port is
/// taken: the tailnet URL would otherwise reach whatever holds it.
pub(crate) async fn bind_identity_listener(
    plan: &RemoteServePlan,
) -> anyhow::Result<BoundWebuiListener> {
    let addr = plan.identity_addr();
    bind_webui_v2(addr).await.with_context(|| {
        format!(
            "failed to bind the remote identity listener on {addr} ([remote].identity_port); \
             free the port or run `zlo remote disable`"
        )
    })
}

/// Wait for the identity listener after the main one stopped. When the main
/// listener failed, nothing will signal the identity listener's shutdown,
/// so it is aborted instead.
pub(crate) async fn finish_identity_listener(
    task: tokio::task::JoinHandle<Result<(), RebornWebuiServeError>>,
    main_failed: bool,
) -> anyhow::Result<()> {
    if main_failed {
        task.abort();
    }
    match task.await {
        Ok(result) => result.context("remote identity listener failed"),
        Err(join) if join.is_cancelled() => Ok(()),
        Err(join) => Err(anyhow::anyhow!(
            "remote identity listener task panicked: {join}"
        )),
    }
}
