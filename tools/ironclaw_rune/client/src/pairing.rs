//! Pairing flows: a one-time code, one click by tailnet identity, the
//! legacy `#token=` link, and the one-time move of the old `gateway.json`.
//! Each reads the descriptor first (a wrong machine is turned away before
//! a code is spent) and ends with the computer to save and its token; the
//! caller writes the token to the Keychain and the entry to
//! `environments.json`.

use std::time::Duration;

use ironclaw_product_contracts::environment_access::{
    EnvironmentDescriptor, PairClient, PairedDevice, PairingErrorCode,
};

use crate::api::Client;
use crate::connection::{Connection, Token};
use crate::environment_calls::{fetch_descriptor, pair_tailnet, pairing_error, redeem_code};
use crate::environments::{
    DescriptorProblem, LegacyGateway, Route, RouteKind, SavedEnvironment, check_descriptor,
    check_transport, host_of, routes_for,
};
use crate::error::ClientError;

const DESCRIPTOR_TIMEOUT: Duration = Duration::from_secs(8);

/// A computer to save, and the token for the Keychain.
#[derive(Clone)]
pub struct Paired {
    pub environment: SavedEnvironment,
    pub token: String,
}

impl std::fmt::Debug for Paired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Paired")
            .field("environment", &self.environment)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Why pairing did not work, in words for the person.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PairError {
    #[error("Could not reach {0}. Check that it is awake and on your tailnet.")]
    Unreachable(String),
    #[error("No IronClaw answers at {0}, or it needs an update before ZLO can pair with it.")]
    NotIronClaw(String),
    #[error("That code did not work: it may be mistyped, used or expired. Make a new one.")]
    InvalidCode,
    #[error("{0} belongs to someone else on your tailnet. Ask its owner for a pairing code.")]
    NotOwner(String),
    #[error("One-click pairing is off on {0}. Enter a pairing code from it.")]
    TailnetDisabled(String),
    #[error("Too many tries. Wait a minute, then try again.")]
    RateLimited,
    #[error("That computer turned the token down. Make a pairing code there instead.")]
    TokenRefused,
    #[error(
        "{0} has no secure address yet. On it, run `zlo remote enable`, then pair with a code."
    )]
    NoSecureRoute(String),
    #[error(transparent)]
    Descriptor(#[from] DescriptorProblem),
    #[error("{0}")]
    Other(String),
}

impl PairError {
    /// Worth retrying later on its own (the migration's "next launch").
    pub fn is_transient(&self) -> bool {
        matches!(self, PairError::Unreachable(_) | PairError::RateLimited)
    }
}

fn reach_error(origin: &str, e: ClientError) -> PairError {
    match e {
        ClientError::Network(_) => PairError::Unreachable(host_of(origin).to_string()),
        ClientError::Config(m) => PairError::Other(m),
        ClientError::Api { status: 404, .. } | ClientError::Decode(_) => {
            PairError::NotIronClaw(host_of(origin).to_string())
        }
        ClientError::Api { status: 429, .. } => PairError::RateLimited,
        other => PairError::Other(other.to_string()),
    }
}

/// The routes of a freshly paired computer: its tailnet URL first, then the
/// address it was paired at.
fn routes(origin: &str, descriptor: &EnvironmentDescriptor) -> Vec<Route> {
    let mut out = routes_for(origin, descriptor);
    if out.is_empty()
        && let Ok(origin) = check_transport(origin)
    {
        out.push(Route::for_origin(&origin));
    }
    out
}

async fn descriptor(origin: &str) -> Result<EnvironmentDescriptor, PairError> {
    let d = fetch_descriptor(origin, DESCRIPTOR_TIMEOUT)
        .await
        .map_err(|e| reach_error(origin, e))?;
    check_descriptor(&d, None)?;
    Ok(d)
}

fn finish(
    origin: &str,
    d: &EnvironmentDescriptor,
    paired: PairedDevice,
) -> Result<Paired, PairError> {
    if paired.environment_id != d.environment_id {
        return Err(DescriptorProblem::DifferentMachine {
            expected: d.label.clone(),
            found: paired.environment_id,
        }
        .into());
    }
    Ok(Paired {
        environment: SavedEnvironment {
            environment_id: d.environment_id.clone(),
            label: d.label.clone(),
            routes: routes(origin, d),
            legacy_token: false,
        },
        token: paired.token,
    })
}

fn refusal(label: &str, origin: &str, e: ClientError) -> PairError {
    match pairing_error(&e) {
        Some(PairingErrorCode::InvalidCode) => PairError::InvalidCode,
        Some(PairingErrorCode::NotOwner) => PairError::NotOwner(label.to_string()),
        Some(PairingErrorCode::TailnetPairingDisabled) => {
            PairError::TailnetDisabled(label.to_string())
        }
        None => match e {
            // The normal listener answers 404 for the tailnet route.
            ClientError::Api { status: 404, .. } => PairError::TailnetDisabled(label.to_string()),
            ClientError::Api { status: 400, .. } => PairError::InvalidCode,
            e => reach_error(origin, e),
        },
    }
}

/// Redeem a one-time code at `origin`.
pub async fn with_code(origin: &str, code: &str, me: PairClient) -> Result<Paired, PairError> {
    let d = descriptor(origin).await?;
    let paired = redeem_code(origin, code, me)
        .await
        .map_err(|e| refusal(&d.label, origin, e))?;
    finish(origin, &d, paired)
}

/// Redeem a code made over SSH, through the tunnel at `tunnel_origin`
/// (`http://127.0.0.1:<local port>`). The loopback address is never saved:
/// the computer gets its tailnet URL (if it has one) and `ssh_route`.
pub async fn over_ssh(
    tunnel_origin: &str,
    code: &str,
    ssh_route: Route,
    me: PairClient,
) -> Result<Paired, PairError> {
    let mut paired = with_code(tunnel_origin, code, me).await?;
    paired.environment.routes = ssh_routes(&paired.environment.routes, tunnel_origin, ssh_route);
    Ok(paired)
}

/// A freshly paired computer's routes minus the tunnel's own address,
/// with the SSH route last.
fn ssh_routes(routes: &[Route], tunnel_origin: &str, ssh_route: Route) -> Vec<Route> {
    let mut out: Vec<Route> = routes
        .iter()
        .filter(|r| r.url != tunnel_origin && r.kind != RouteKind::Ssh)
        .cloned()
        .collect();
    out.push(ssh_route);
    out
}

/// One click: the backend sees this Mac's tailnet login.
pub async fn by_tailnet(origin: &str, me: PairClient) -> Result<Paired, PairError> {
    let d = descriptor(origin).await?;
    let paired = pair_tailnet(origin, me)
        .await
        .map_err(|e| refusal(&d.label, origin, e))?;
    finish(origin, &d, paired)
}

/// The legacy `#token=` link (one release): the master token is saved
/// as-is, flagged for a re-pair.
pub async fn with_legacy_token(origin: &str, token: &str) -> Result<Paired, PairError> {
    let d = descriptor(origin).await?;
    verify_token(origin, token).await?;
    let routes = routes(origin, &d);
    Ok(Paired {
        environment: SavedEnvironment {
            environment_id: d.environment_id.clone(),
            label: d.label.clone(),
            routes,
            legacy_token: true,
        },
        token: token.to_string(),
    })
}

async fn verify_token(origin: &str, token: &str) -> Result<(), PairError> {
    let client = Client::new(Connection {
        base_url: origin.to_string(),
        token: Token::new(token).map_err(|e| PairError::Other(e.to_string()))?,
        token_source: "pairing link".into(),
    })
    .map_err(|e| PairError::Other(e.to_string()))?;
    match client.session().await {
        Ok(_) => Ok(()),
        Err(e) if e.is_unauthorized() => Err(PairError::TokenRefused),
        Err(e) => Err(reach_error(origin, e)),
    }
}

/// Move the old `gateway.json` computer, keeping its token as a legacy one.
///
/// Only an old address that passes the transport rule (https, or this
/// Mac) is trusted to name the computer's routes: a descriptor read over
/// plain http could be swapped in transit to advertise someone else's
/// https address, and the master token would then go there. Such a
/// computer has to be paired again with a code.
pub async fn migrate(legacy: &LegacyGateway) -> Result<Paired, PairError> {
    let old = legacy.url.trim().trim_end_matches('/');
    if check_transport(old).is_err() {
        return Err(PairError::NoSecureRoute(host_of(old).to_string()));
    }
    let d = crate::environment_calls::fetch_public_descriptor(old, DESCRIPTOR_TIMEOUT)
        .await
        .map_err(|e| reach_error(old, e))?;
    check_descriptor(&d, None)?;
    let routes = routes_for(old, &d);
    if routes.is_empty() {
        return Err(PairError::NoSecureRoute(d.label.clone()));
    }
    Ok(Paired {
        environment: SavedEnvironment {
            environment_id: d.environment_id.clone(),
            label: d.label.clone(),
            routes,
            legacy_token: true,
        },
        token: legacy.token.clone(),
    })
}

/// Whether a route is the tailnet one (for the row's caption).
pub fn is_tailnet(route: &Route) -> bool {
    route.kind == RouteKind::Tailnet
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::serve_routes;

    const DESCRIPTOR: &str = r#"{"environment_id":"0f6c2d0e-3b0a-4a51-9b7e-6b1c8f1f2a10","label":"Studio","server_version":"0.41.0","protocol_version":1,"pairing":["one_time_code","tailnet_identity"],"tailnet_url":"https://studio.tail9915dd.ts.net"}"#;
    const PAIRED: &str = r#"{"environment_id":"0f6c2d0e-3b0a-4a51-9b7e-6b1c8f1f2a10","device_id":"0123456789abcdef0123456789abcdef","token":"icd_0123456789abcdef0123456789abcdef_s3cr3t"}"#;
    const OTHER: &str = r#"{"environment_id":"11111111-3b0a-4a51-9b7e-6b1c8f1f2a10","device_id":"0123456789abcdef0123456789abcdef","token":"icd_x_y"}"#;

    fn me() -> PairClient {
        PairClient {
            label: "Air".into(),
            os: "macOS".into(),
            app_version: "0.1.0".into(),
        }
    }

    #[tokio::test]
    async fn a_code_pairs_and_the_tailnet_url_comes_first() {
        let (url, seen) = serve_routes(vec![
            ("/.well-known/ironclaw/environment", 200, DESCRIPTOR),
            ("/api/auth/pair", 200, PAIRED),
        ])
        .await;
        let paired = with_code(&url, "7KQ2-M9XD-4TRA", me()).await.unwrap();
        assert_eq!(paired.environment.label, "Studio");
        assert!(!paired.environment.legacy_token);
        assert_eq!(
            paired.environment.routes[0].url,
            "https://studio.tail9915dd.ts.net"
        );
        assert_eq!(paired.environment.routes[1].url, url);
        assert!(paired.token.starts_with("icd_"));
        let seen = seen.lock().unwrap();
        assert!(seen[0].line.contains("/.well-known/ironclaw/environment"));
        assert!(seen[1].line.starts_with("POST /api/auth/pair "));
        assert!(!format!("{paired:?}").contains("s3cr3t"));
    }

    #[tokio::test]
    async fn a_code_over_ssh_saves_the_ssh_route_never_the_tunnel() {
        let (url, _) = serve_routes(vec![
            ("/.well-known/ironclaw/environment", 200, DESCRIPTOR),
            ("/api/auth/pair", 200, PAIRED),
        ])
        .await;
        let target = crate::ssh::parse_target("me@studio.local").unwrap();
        let paired = over_ssh(&url, "7KQ2-M9XD-4TRA", Route::ssh(&target, 3000), me())
            .await
            .unwrap();
        let routes = &paired.environment.routes;
        assert_eq!(routes.len(), 2, "{routes:?}");
        assert_eq!(routes[0].kind, RouteKind::Tailnet);
        assert_eq!(routes[1].kind, RouteKind::Ssh);
        assert_eq!(routes[1].url, "ssh://me@studio.local:22");
        assert!(routes.iter().all(|r| r.url != url));
    }

    #[tokio::test]
    async fn a_token_from_another_machine_is_refused() {
        let (url, _) = serve_routes(vec![
            ("/.well-known/ironclaw/environment", 200, DESCRIPTOR),
            ("/api/auth/pair", 200, OTHER),
        ])
        .await;
        let err = with_code(&url, "7KQ2-M9XD-4TRA", me()).await.unwrap_err();
        assert!(matches!(
            err,
            PairError::Descriptor(DescriptorProblem::DifferentMachine { .. })
        ));
    }

    #[tokio::test]
    async fn a_newer_protocol_is_blocked_before_the_code_is_spent() {
        let (url, seen) = serve_routes(vec![(
            "/.well-known/ironclaw/environment",
            200,
            r#"{"environment_id":"e","label":"Studio","server_version":"9","protocol_version":2,"pairing":[],"tailnet_url":null}"#,
        )])
        .await;
        let err = with_code(&url, "7KQ2-M9XD-4TRA", me()).await.unwrap_err();
        assert!(err.to_string().contains("Update ZLO"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1, "the code was not sent");
    }

    #[tokio::test]
    async fn tailnet_refusals_become_the_code_fallback_lines() {
        let (url, _) = serve_routes(vec![
            ("/.well-known/ironclaw/environment", 200, DESCRIPTOR),
            (
                "/api/auth/pair/tailnet",
                403,
                r#"{"error":"not_owner","message":"x"}"#,
            ),
        ])
        .await;
        assert_eq!(
            by_tailnet(&url, me()).await.unwrap_err(),
            PairError::NotOwner("Studio".into())
        );
        let (url, _) = serve_routes(vec![
            ("/.well-known/ironclaw/environment", 200, DESCRIPTOR),
            (
                "/api/auth/pair/tailnet",
                403,
                r#"{"error":"tailnet_pairing_disabled","message":"x"}"#,
            ),
        ])
        .await;
        assert_eq!(
            by_tailnet(&url, me()).await.unwrap_err(),
            PairError::TailnetDisabled("Studio".into())
        );
        let (url, _) = serve_routes(vec![
            ("/.well-known/ironclaw/environment", 200, DESCRIPTOR),
            ("/api/auth/pair/tailnet", 200, PAIRED),
        ])
        .await;
        assert!(by_tailnet(&url, me()).await.is_ok());
    }

    #[tokio::test]
    async fn a_legacy_link_checks_its_token_and_is_flagged() {
        let (url, _) = serve_routes(vec![
            ("/.well-known/ironclaw/environment", 200, DESCRIPTOR),
            (
                "/api/webchat/v2/session",
                200,
                r#"{"tenant_id":"t","user_id":"u","features":{}}"#,
            ),
        ])
        .await;
        let paired = with_legacy_token(&url, "master").await.unwrap();
        assert!(paired.environment.legacy_token);
        assert_eq!(paired.token, "master");

        let (url, _) = serve_routes(vec![
            ("/.well-known/ironclaw/environment", 200, DESCRIPTOR),
            (
                "/api/webchat/v2/session",
                401,
                r#"{"error":"unauthorized"}"#,
            ),
        ])
        .await;
        assert_eq!(
            with_legacy_token(&url, "bad").await.unwrap_err(),
            PairError::TokenRefused
        );
    }

    #[tokio::test]
    async fn migration_keeps_the_token_and_needs_a_secure_route() {
        let (url, _) =
            serve_routes(vec![("/.well-known/ironclaw/environment", 200, DESCRIPTOR)]).await;
        let legacy = LegacyGateway {
            url: url.clone(),
            token: "master".into(),
        };
        let paired = migrate(&legacy).await.unwrap();
        assert!(paired.environment.legacy_token);
        assert_eq!(paired.token, "master");
        assert_eq!(
            paired.environment.routes[0].url,
            "https://studio.tail9915dd.ts.net"
        );

        // A backend without the descriptor yet: transient? No, it needs an
        // update; the migration stays pending either way.
        let (url, _) = serve_routes(vec![]).await;
        let err = migrate(&LegacyGateway {
            url,
            token: "m".into(),
        })
        .await
        .unwrap_err();
        assert!(matches!(err, PairError::NotIronClaw(_)), "{err:?}");

        // Unreachable: retried next launch.
        let err = migrate(&LegacyGateway {
            url: "http://127.0.0.1:9".into(),
            token: "m".into(),
        })
        .await
        .unwrap_err();
        assert!(err.is_transient(), "{err:?}");

        // An old plain-http tailnet link: its descriptor could be swapped in
        // transit, so the master token is not moved; pair again instead.
        let err = migrate(&LegacyGateway {
            url: "http://100.101.2.3:3000".into(),
            token: "m".into(),
        })
        .await
        .unwrap_err();
        assert!(matches!(err, PairError::NoSecureRoute(_)), "{err:?}");
        assert!(!err.is_transient());
    }
}
