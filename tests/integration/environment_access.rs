//! Remote environments, wire contract v1
//! (`docs/internal/plans/2026-11-08-zlo-remote-environments.md`).
//!
//! Drives the fully composed WebUI app (`webui_v2_app` with the bearer, body
//! and rate-limit middleware) over the real filesystem-backed records from
//! `ironclaw_identity`, opened through composition's test-support factory.
//! Cross-crate on purpose: the crate tier of `ironclaw_webui` cannot reach
//! the identity store, and the identity crate cannot reach HTTP, so the
//! "a revoked device gets 401, including after a restart" claim only exists
//! here. A restart is modelled by opening the store and the app again over
//! the same in-memory host filesystem.
//!
//! Flat suite, no harness mounts: this is an HTTP wire contract, not an
//! agent turn.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use ironclaw_assistant::rejecting_product_surface_error;
use ironclaw_composition::open_environment_access_for_test;
use ironclaw_filesystem::InMemoryBackend;
use ironclaw_host_api::ids::{AgentId, TenantId, UserId};
use ironclaw_product_contracts::environment_access::{
    DeviceList, EnvironmentAccessService, EnvironmentDescriptor, PairedDevice, PairingCodeIssued,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use ironclaw_webui::{
    DeviceTokenAuthenticator, EnvBearerAuthenticator, EnvironmentAccessRouteConfig,
    RemoteAccessPolicy, WebuiServeConfig, environment_access_route_mount, identity_listener_router,
    main_listener_router, webui_v2_app,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use tower::ServiceExt;

const MASTER: &str = "master-token-0123456789abcdef0123456789";
const OWNER: &str = "owner@example.com";

struct NoProductSurface;

#[async_trait]
impl ProductSurface for NoProductSurface {
    async fn invoke(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        Err(rejecting_product_surface_error())
    }

    async fn query(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        Err(rejecting_product_surface_error())
    }

    async fn stream_events(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        Err(rejecting_product_surface_error())
    }
}

/// The main and identity listeners exactly as `zlo serve` builds them.
struct Listeners {
    main: Router,
    identity: Router,
}

async fn listeners(
    root: &Arc<InMemoryBackend>,
    remote: Option<RemoteAccessPolicy>,
) -> (Listeners, Arc<dyn EnvironmentAccessService>) {
    let service = open_environment_access_for_test(Arc::clone(root), "Studio")
        .await
        .expect("environment access opens");
    let operator = UserId::new("operator").expect("user");
    let master = Arc::new(
        EnvBearerAuthenticator::new(SecretString::from(MASTER.to_string()), operator.clone())
            .expect("master authenticator"),
    );
    let authenticator = Arc::new(DeviceTokenAuthenticator::new(
        Arc::clone(&service),
        operator,
        master,
    ));
    let config = WebuiServeConfig::new(
        TenantId::new("tenant-env").expect("tenant"),
        authenticator,
        Vec::new(),
    )
    .with_default_agent_id(AgentId::new("agent-env").expect("agent"))
    .with_split_route_mount(environment_access_route_mount(
        EnvironmentAccessRouteConfig {
            service: Arc::clone(&service),
            server_version: "0.0.0-test".to_string(),
            remote,
            build_commit: Some("0123abcd".to_string()),
            self_update: false,
        },
    ));
    let router = webui_v2_app(Arc::new(NoProductSurface), config).expect("app composes");
    (
        Listeners {
            main: main_listener_router(router.clone()),
            identity: identity_listener_router(router),
        },
        service,
    )
}

fn one_click(one_click: bool) -> Option<RemoteAccessPolicy> {
    Some(RemoteAccessPolicy {
        tailnet_url: "https://studio.tail9915dd.ts.net".to_string(),
        owner_logins: vec![OWNER.to_string()],
        one_click,
    })
}

fn request(
    method: Method,
    uri: &str,
    bearer: Option<&str>,
    body: Option<Value>,
    peer: [u8; 4],
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = match body {
        Some(value) => {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let mut request = builder.body(body).expect("request");
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from((peer, 4000))));
    request
}

async fn send(router: &Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("body");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

fn client() -> Value {
    json!({ "label": "Air", "os": "macOS", "app_version": "0.1.0" })
}

async fn mint_code(router: &Router, bearer: &str) -> PairingCodeIssued {
    let (status, body) = send(
        router,
        request(
            Method::POST,
            "/api/auth/pairing-codes",
            Some(bearer),
            None,
            [127, 0, 0, 1],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    serde_json::from_value(body).expect("pairing code body")
}

async fn redeem(router: &Router, code: &str, peer: [u8; 4]) -> (StatusCode, Value) {
    send(
        router,
        request(
            Method::POST,
            "/api/auth/pair",
            None,
            Some(json!({ "code": code, "client": client() })),
            peer,
        ),
    )
    .await
}

#[tokio::test]
async fn descriptor_is_public_and_carries_no_secrets() {
    let root = Arc::new(InMemoryBackend::default());
    let (off, _) = listeners(&root, None).await;
    let (status, body) = send(
        &off.main,
        request(
            Method::GET,
            "/.well-known/ironclaw/environment",
            None,
            None,
            [10, 0, 0, 1],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let descriptor: EnvironmentDescriptor = serde_json::from_value(body.clone()).expect("shape");
    assert_eq!(descriptor.label, "Studio");
    assert_eq!(descriptor.protocol_version, 1);
    assert_eq!(body["pairing"], json!(["one_time_code"]));
    assert_eq!(body["tailnet_url"], Value::Null);
    assert_eq!(body["build_commit"], json!("0123abcd"));
    assert_eq!(body["self_update"], json!(false));
    let keys: Vec<&str> = body
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys.len(),
        8,
        "only the eight contract fields are public: {keys:?}"
    );
    assert!(!body.to_string().contains(MASTER));

    // Same installation, remote on: one-click advertised, id unchanged.
    let (on, _) = listeners(&root, one_click(true)).await;
    let (_, remote_body) = send(
        &on.main,
        request(
            Method::GET,
            "/.well-known/ironclaw/environment",
            None,
            None,
            [10, 0, 0, 1],
        ),
    )
    .await;
    assert_eq!(
        remote_body["pairing"],
        json!(["one_time_code", "tailnet_identity"])
    );
    assert_eq!(
        remote_body["tailnet_url"],
        "https://studio.tail9915dd.ts.net"
    );
    assert_eq!(remote_body["environment_id"], body["environment_id"]);

    let (one_click_off, _) = listeners(&root, one_click(false)).await;
    let (_, off_body) = send(
        &one_click_off.main,
        request(
            Method::GET,
            "/.well-known/ironclaw/environment",
            None,
            None,
            [10, 0, 0, 1],
        ),
    )
    .await;
    assert_eq!(off_body["pairing"], json!(["one_time_code"]));
}

#[tokio::test]
async fn pairing_codes_require_an_operator_bearer() {
    let root = Arc::new(InMemoryBackend::default());
    let (app, _) = listeners(&root, None).await;
    let (status, _) = send(
        &app.main,
        request(
            Method::POST,
            "/api/auth/pairing-codes",
            None,
            None,
            [127, 0, 0, 1],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let issued = mint_code(&app.main, MASTER).await;
    assert_eq!(issued.code.len(), 14, "XXXX-XXXX-XXXX");
}

#[tokio::test]
async fn redeem_then_every_miss_is_one_uniform_invalid_code() {
    let root = Arc::new(InMemoryBackend::default());
    let (app, _) = listeners(&root, None).await;
    let issued = mint_code(&app.main, MASTER).await;

    let lower = issued.code.to_ascii_lowercase().replace('-', "");
    let (status, body) = redeem(&app.main, &lower, [10, 0, 0, 2]).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let paired: PairedDevice = serde_json::from_value(body).expect("paired");
    assert!(
        paired
            .token
            .starts_with(&format!("icd_{}_", paired.device_id))
    );

    // Used, unknown and malformed codes: same status, same body.
    let mut bodies = Vec::new();
    for (code, peer) in [
        (issued.code.as_str(), [10, 0, 0, 3]),
        ("ZZZZ-ZZZZ-ZZZZ", [10, 0, 0, 4]),
        ("nope", [10, 0, 0, 5]),
    ] {
        let (status, body) = redeem(&app.main, code, peer).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{code}");
        bodies.push(body);
    }
    assert_eq!(bodies[0]["error"], "invalid_code");
    assert!(bodies.iter().all(|body| body == &bodies[0]), "{bodies:?}");
}

#[tokio::test]
async fn concurrent_redemptions_through_the_route_have_one_winner() {
    let root = Arc::new(InMemoryBackend::default());
    let (app, _) = listeners(&root, None).await;
    let issued = mint_code(&app.main, MASTER).await;
    let mut tasks = Vec::new();
    for n in 0..6u8 {
        let router = app.main.clone();
        let code = issued.code.clone();
        tasks.push(tokio::spawn(async move {
            redeem(&router, &code, [10, 1, 0, n]).await.0
        }));
    }
    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.expect("join"));
    }
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .all(|s| *s == StatusCode::OK || *s == StatusCode::BAD_REQUEST)
    );
}

#[tokio::test]
async fn pair_route_is_strictly_rate_limited_per_peer() {
    let root = Arc::new(InMemoryBackend::default());
    let (app, _) = listeners(&root, None).await;
    let mut last = StatusCode::OK;
    for _ in 0..11 {
        last = redeem(&app.main, "ZZZZ-ZZZZ-ZZZZ", [10, 2, 0, 1]).await.0;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
    // Another peer is unaffected.
    assert_eq!(
        redeem(&app.main, "ZZZZ-ZZZZ-ZZZZ", [10, 2, 0, 2]).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn device_token_is_operator_until_revoked_including_after_restart() {
    let root = Arc::new(InMemoryBackend::default());
    let (app, _) = listeners(&root, None).await;
    let issued = mint_code(&app.main, MASTER).await;
    let (_, body) = redeem(&app.main, &issued.code, [10, 3, 0, 1]).await;
    let paired: PairedDevice = serde_json::from_value(body).expect("paired");

    // A device token carries operator authority: it can mint codes and
    // list devices, and its own row is `current`.
    mint_code(&app.main, &paired.token).await;
    let (status, body) = send(
        &app.main,
        request(
            Method::GET,
            "/api/auth/devices",
            Some(&paired.token),
            None,
            [10, 3, 0, 1],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let list: DeviceList = serde_json::from_value(body).expect("list");
    assert_eq!(list.devices.len(), 1);
    assert!(list.devices[0].current);
    assert!(list.devices[0].last_seen_at.is_some());

    let revoke_uri = format!("/api/auth/devices/{}", paired.device_id);
    for _ in 0..2 {
        let (status, _) = send(
            &app.main,
            request(
                Method::DELETE,
                &revoke_uri,
                Some(MASTER),
                None,
                [127, 0, 0, 1],
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "revoke is idempotent");
    }
    let (status, _) = send(
        &app.main,
        request(
            Method::GET,
            "/api/auth/devices",
            Some(&paired.token),
            None,
            [10, 3, 0, 1],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Restart: a fresh store and app over the same host filesystem.
    let (restarted, _) = listeners(&root, None).await;
    let (status, _) = send(
        &restarted.main,
        request(
            Method::GET,
            "/api/auth/devices",
            Some(&paired.token),
            None,
            [10, 3, 0, 1],
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "revocation survives a restart"
    );
    let (status, body) = send(
        &restarted.main,
        request(
            Method::GET,
            "/api/auth/devices",
            Some(MASTER),
            None,
            [127, 0, 0, 1],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list: DeviceList = serde_json::from_value(body).expect("list");
    assert!(
        list.devices[0].revoked_at.is_some(),
        "revoked rows stay listed"
    );
    assert!(!list.devices[0].current);
}

#[tokio::test]
async fn tailnet_pairing_only_for_owners_on_the_identity_listener() {
    let root = Arc::new(InMemoryBackend::default());
    let (app, _) = listeners(&root, one_click(true)).await;
    let tailnet = |login: Option<&str>| {
        let mut req = request(
            Method::POST,
            "/api/auth/pair/tailnet",
            None,
            Some(json!({ "client": client() })),
            [127, 0, 0, 1],
        );
        if let Some(login) = login {
            req.headers_mut()
                .insert("tailscale-user-login", login.parse().expect("header"));
        }
        req
    };

    let (status, body) = send(&app.identity, tailnet(Some("OWNER@example.com"))).await;
    assert_eq!(status, StatusCode::OK, "owner pairs in one click: {body}");
    let paired: PairedDevice = serde_json::from_value(body).expect("paired");
    let (_, list) = send(
        &app.main,
        request(
            Method::GET,
            "/api/auth/devices",
            Some(&paired.token),
            None,
            [127, 0, 0, 1],
        ),
    )
    .await;
    assert_eq!(list["devices"][0]["paired_by"], "tailnet_identity");

    let (status, body) = send(&app.identity, tailnet(Some("someone@else.com"))).await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::FORBIDDEN, json!("not_owner"))
    );
    let (status, body) = send(&app.identity, tailnet(None)).await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::FORBIDDEN, json!("not_owner"))
    );

    // The main listener ignores Tailscale headers entirely: even the
    // owner's login gets 404, never a token.
    let (status, _) = send(&app.main, tailnet(Some(OWNER))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (disabled, _) = listeners(&root, one_click(false)).await;
    let (status, body) = send(&disabled.identity, tailnet(Some(OWNER))).await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::FORBIDDEN, json!("tailnet_pairing_disabled"))
    );
    let (remote_off, _) = listeners(&root, None).await;
    let (status, body) = send(&remote_off.identity, tailnet(Some(OWNER))).await;
    assert_eq!(
        (status, body["error"].clone()),
        (StatusCode::FORBIDDEN, json!("tailnet_pairing_disabled"))
    );
}
