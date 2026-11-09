//! Remote-environment routes, the device-token authenticator, and the two
//! listener wrappers (`docs/internal/plans/2026-11-08-zlo-remote-environments.md`,
//! "Wire contract v1").
//!
//! - `GET /.well-known/ironclaw/environment` (public) — the descriptor a
//!   client reads before pairing.
//! - `POST /api/auth/pair` (public, strict per-peer rate limit) — redeem a
//!   one-time code for a device token; every miss is one `400 invalid_code`.
//! - `POST /api/auth/pair/tailnet` (public, identity listener only) — one
//!   click for an owner's tailnet login; `404` on the main listener.
//! - `POST /api/auth/pairing-codes`, `GET /api/auth/devices`,
//!   `DELETE /api/auth/devices/{device_id}` (bearer, operator only).
//!
//! The records, hashing, expiry and single-use guarantee live behind
//! [`EnvironmentAccessService`] (implemented in `ironclaw_identity`). This
//! module owns HTTP, the operator check, and the tailnet owner decision.
//!
//! Tailscale identity headers are trusted only through
//! [`identity_listener_router`], which reads `Tailscale-User-Login` into a
//! request extension a client cannot forge. [`main_listener_router`] strips
//! every `Tailscale-*` header, and the tailnet route answers `404` when the
//! extension is absent.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Path, Request, State};
use axum::http::{HeaderName, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use ironclaw_host_api::action::NetworkMethod;
use ironclaw_host_api::ids::UserId;
use ironclaw_host_api::ingress::{
    AllowedEffectPath, AuditTraceClass, BodyLimitPolicy, CorsPolicy, IngressAuthPolicy,
    IngressAuthScheme, IngressJustification, IngressPolicy, IngressPolicyParts,
    IngressRouteDescriptor, IngressScopeSource, ListenerClass, RateLimitPolicy, RateLimitScope,
    StreamingMode, WebSocketOriginPolicy,
};
use ironclaw_host_ingress::SplitRouteMount;
use ironclaw_product_contracts::environment_access::{
    DEVICE_TOKEN_PREFIX, DEVICES_PATH, DeviceId, ENVIRONMENT_DESCRIPTOR_PATH,
    ENVIRONMENT_PROTOCOL_VERSION, EnvironmentAccessError, EnvironmentAccessService,
    EnvironmentDescriptor, PAIR_PATH, PAIR_TAILNET_PATH, PAIRING_CODES_PATH, PairRequest,
    PairingErrorCode, PairingMethod, TailnetPairRequest,
};
use serde::Serialize;

use crate::webui_v2::WebUiV2Capabilities;
use crate::{WebuiAuthentication, WebuiAuthenticator};

/// The header `tailscale serve` sets to the caller's login. Read only on the
/// identity listener.
pub const TAILSCALE_USER_LOGIN_HEADER: &str = "tailscale-user-login";
const TAILSCALE_HEADER_PREFIX: &str = "tailscale-";

const DEVICE_PATH: &str = "/api/auth/devices/{device_id}";

const RATE_WINDOW_SECONDS: std::num::NonZeroU32 = std::num::NonZeroU32::new(60).expect("60 != 0"); // safety: const-evaluated, literal non-zero
const DESCRIPTOR_MAX_REQUESTS: std::num::NonZeroU32 =
    std::num::NonZeroU32::new(60).expect("60 != 0"); // safety: const-evaluated, literal non-zero
/// "A handful per minute per peer": a 12-character code has 60 bits, so
/// ten guesses a minute against a five-minute code is hopeless.
const PAIR_MAX_REQUESTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(10).expect("10 != 0"); // safety: const-evaluated, literal non-zero
const OPERATOR_MUTATION_MAX_REQUESTS: std::num::NonZeroU32 =
    std::num::NonZeroU32::new(30).expect("30 != 0"); // safety: const-evaluated, literal non-zero
const OPERATOR_READ_MAX_REQUESTS: std::num::NonZeroU32 =
    std::num::NonZeroU32::new(120).expect("120 != 0"); // safety: const-evaluated, literal non-zero
const PAIR_BODY_LIMIT_BYTES: std::num::NonZeroU64 =
    std::num::NonZeroU64::new(4 * 1024).expect("4096 != 0"); // safety: const-evaluated, literal non-zero
const PAIRING_CODES_BODY_LIMIT_BYTES: std::num::NonZeroU64 =
    std::num::NonZeroU64::new(1024).expect("1024 != 0"); // safety: const-evaluated, literal non-zero

/// One-click tailnet pairing as configured by `[remote]`.
#[derive(Debug, Clone)]
pub struct RemoteAccessPolicy {
    /// The `tailscale serve` HTTPS URL, advertised in the descriptor.
    pub tailnet_url: String,
    /// Tailnet logins allowed to pair in one click (compared ignoring ASCII case).
    pub owner_logins: Vec<String>,
    /// Whether one-click pairing is allowed at all (off on a multi-user host
    /// unless the operator trusts its local users).
    pub one_click: bool,
}

/// Input to [`environment_access_route_mount`].
pub struct EnvironmentAccessRouteConfig {
    pub service: Arc<dyn EnvironmentAccessService>,
    /// The backend build, reported as `server_version`.
    pub server_version: String,
    /// `Some` while `[remote] enabled`.
    pub remote: Option<RemoteAccessPolicy>,
    /// The commit the backend was built from, reported as `build_commit`.
    pub build_commit: Option<String>,
    /// `[update]` is set up, reported as `self_update`.
    pub self_update: bool,
}

struct RouteState {
    service: Arc<dyn EnvironmentAccessService>,
    server_version: String,
    remote: Option<RemoteAccessPolicy>,
    build_commit: Option<String>,
    self_update: bool,
}

/// The device whose token authenticated this request (inserted by the
/// bearer middleware when the authenticator reported one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedDevice(pub DeviceId);

/// Marker inserted only by [`identity_listener_router`]: the request came in
/// through the `tailscale serve` target, with the caller's login if any.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TailnetCaller {
    login: Option<String>,
}

/// Build the split mount: the descriptor and both pair routes public, the
/// pairing-code and device routes behind bearer auth.
pub fn environment_access_route_mount(config: EnvironmentAccessRouteConfig) -> SplitRouteMount {
    let state = Arc::new(RouteState {
        service: config.service,
        server_version: config.server_version,
        remote: config.remote,
        build_commit: config.build_commit,
        self_update: config.self_update,
    });
    let public = Router::new()
        .route(ENVIRONMENT_DESCRIPTOR_PATH, get(get_descriptor))
        .route(PAIR_PATH, post(pair))
        .route(PAIR_TAILNET_PATH, post(pair_tailnet))
        .with_state(Arc::clone(&state));
    let protected = Router::new()
        .route(PAIRING_CODES_PATH, post(issue_pairing_code))
        .route(DEVICES_PATH, get(list_devices))
        .route(DEVICE_PATH, delete(revoke_device))
        .with_state(state);
    SplitRouteMount::new(protected, public, route_descriptors())
}

/// The router a `tailscale serve` target listener serves: `router` plus the
/// identity extension read from `Tailscale-User-Login`. Every `Tailscale-*`
/// header is removed after reading, so nothing downstream sees raw headers.
pub fn identity_listener_router(router: Router) -> Router {
    router.layer(middleware::from_fn(read_tailnet_identity))
}

/// The router the normal listener serves: `router` with every `Tailscale-*`
/// header removed, so a caller cannot smuggle an identity through it.
pub fn main_listener_router(router: Router) -> Router {
    router.layer(middleware::from_fn(strip_tailscale_headers))
}

async fn read_tailnet_identity(mut request: Request, next: Next) -> Response {
    let login = request
        .headers()
        .get(TAILSCALE_USER_LOGIN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|login| !login.is_empty())
        .map(str::to_string);
    remove_tailscale_headers(&mut request);
    request.extensions_mut().insert(TailnetCaller { login });
    next.run(request).await
}

async fn strip_tailscale_headers(mut request: Request, next: Next) -> Response {
    remove_tailscale_headers(&mut request);
    next.run(request).await
}

fn remove_tailscale_headers(request: &mut Request) {
    let names: Vec<HeaderName> = request
        .headers()
        .keys()
        .filter(|name| name.as_str().starts_with(TAILSCALE_HEADER_PREFIX))
        .cloned()
        .collect();
    for name in names {
        request.headers_mut().remove(name);
    }
}

/// `WebuiAuthenticator` for `icd_` device tokens; every other bearer goes to
/// `fallback` (the master token / session chain). A live device token maps
/// to the operator, exactly like the master token (plan decision 1), with
/// its device id attached for audit.
pub struct DeviceTokenAuthenticator {
    service: Arc<dyn EnvironmentAccessService>,
    operator: UserId,
    fallback: Arc<dyn WebuiAuthenticator>,
}

impl DeviceTokenAuthenticator {
    pub fn new(
        service: Arc<dyn EnvironmentAccessService>,
        operator: UserId,
        fallback: Arc<dyn WebuiAuthenticator>,
    ) -> Self {
        Self {
            service,
            operator,
            fallback,
        }
    }
}

#[async_trait]
impl WebuiAuthenticator for DeviceTokenAuthenticator {
    async fn authenticate(&self, token: &str) -> Option<WebuiAuthentication> {
        if !token.starts_with(DEVICE_TOKEN_PREFIX) {
            return self.fallback.authenticate(token).await;
        }
        match self.service.authenticate_device(token).await {
            Ok(Some(device_id)) => {
                Some(WebuiAuthentication::operator(self.operator.clone()).with_device(device_id))
            }
            Ok(None) => None,
            // The implementor logged the cause; an outage fails closed.
            Err(_unavailable) => None,
        }
    }

    fn mounts_operator_webui_config_routes(&self) -> bool {
        self.fallback.mounts_operator_webui_config_routes()
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: &'static str,
}

fn error_response(status: StatusCode, error: &'static str, message: &'static str) -> Response {
    (status, Json(ErrorBody { error, message })).into_response()
}

fn pairing_error(code: PairingErrorCode) -> Response {
    match code {
        PairingErrorCode::InvalidCode => error_response(
            StatusCode::BAD_REQUEST,
            "invalid_code",
            "That pairing code is not valid. Ask for a new one.",
        ),
        PairingErrorCode::NotOwner => error_response(
            StatusCode::FORBIDDEN,
            "not_owner",
            "This tailnet login does not own this machine. Ask the owner for a pairing code.",
        ),
        PairingErrorCode::TailnetPairingDisabled => error_response(
            StatusCode::FORBIDDEN,
            "tailnet_pairing_disabled",
            "One-click pairing is off on this machine. Use a pairing code.",
        ),
    }
}

fn service_error(error: EnvironmentAccessError) -> Response {
    match error {
        EnvironmentAccessError::InvalidCode => pairing_error(PairingErrorCode::InvalidCode),
        EnvironmentAccessError::DeviceNotFound => not_found(),
        EnvironmentAccessError::Unavailable => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "Remote access records are temporarily unavailable.",
        ),
    }
}

fn not_found() -> Response {
    error_response(StatusCode::NOT_FOUND, "not_found", "Not found.")
}

fn operator_required() -> Response {
    error_response(
        StatusCode::FORBIDDEN,
        "operator_required",
        "Operator privileges required.",
    )
}

fn bad_request_body() -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        "The request body is not valid.",
    )
}

fn is_operator(capabilities: Option<Extension<WebUiV2Capabilities>>) -> bool {
    capabilities.is_some_and(|Extension(caps)| caps.operator_webui_config)
}

fn one_click_allowed(remote: Option<&RemoteAccessPolicy>) -> bool {
    remote.is_some_and(|remote| remote.one_click)
}

async fn get_descriptor(State(state): State<Arc<RouteState>>) -> Response {
    let identity = match state.service.environment().await {
        Ok(identity) => identity,
        Err(error) => return service_error(error),
    };
    let mut pairing = vec![PairingMethod::OneTimeCode];
    if one_click_allowed(state.remote.as_ref()) {
        pairing.push(PairingMethod::TailnetIdentity);
    }
    Json(EnvironmentDescriptor {
        environment_id: identity.environment_id.into_inner(),
        label: identity.label,
        server_version: state.server_version.clone(),
        protocol_version: ENVIRONMENT_PROTOCOL_VERSION,
        pairing,
        tailnet_url: state
            .remote
            .as_ref()
            .map(|remote| remote.tailnet_url.clone()),
        build_commit: state.build_commit.clone(),
        self_update: state.self_update,
    })
    .into_response()
}

async fn pair(
    State(state): State<Arc<RouteState>>,
    body: Result<Json<PairRequest>, JsonRejection>,
) -> Response {
    let Ok(Json(request)) = body else {
        return bad_request_body();
    };
    match state
        .service
        .redeem_pairing_code(&request.code, request.client)
        .await
    {
        Ok(paired) => Json(paired).into_response(),
        Err(error) => service_error(error),
    }
}

async fn pair_tailnet(
    State(state): State<Arc<RouteState>>,
    caller: Option<Extension<TailnetCaller>>,
    body: Result<Json<TailnetPairRequest>, JsonRejection>,
) -> Response {
    // Only the identity listener inserts the caller extension.
    let Some(Extension(caller)) = caller else {
        return not_found();
    };
    let Some(remote) = state.remote.as_ref().filter(|remote| remote.one_click) else {
        return pairing_error(PairingErrorCode::TailnetPairingDisabled);
    };
    let Some(login) = caller.login.filter(|login| {
        remote
            .owner_logins
            .iter()
            .any(|owner| owner.eq_ignore_ascii_case(login))
    }) else {
        return pairing_error(PairingErrorCode::NotOwner);
    };
    let Ok(Json(request)) = body else {
        return bad_request_body();
    };
    match state
        .service
        .pair_tailnet_owner(&login, request.client)
        .await
    {
        Ok(paired) => Json(paired).into_response(),
        Err(error) => service_error(error),
    }
}

async fn issue_pairing_code(
    State(state): State<Arc<RouteState>>,
    capabilities: Option<Extension<WebUiV2Capabilities>>,
) -> Response {
    if !is_operator(capabilities) {
        return operator_required();
    }
    match state.service.issue_pairing_code().await {
        Ok(issued) => Json(issued).into_response(),
        Err(error) => service_error(error),
    }
}

async fn list_devices(
    State(state): State<Arc<RouteState>>,
    capabilities: Option<Extension<WebUiV2Capabilities>>,
    device: Option<Extension<AuthenticatedDevice>>,
) -> Response {
    if !is_operator(capabilities) {
        return operator_required();
    }
    let current = device.map(|Extension(AuthenticatedDevice(id))| id);
    match state.service.list_devices(current.as_ref()).await {
        Ok(list) => Json(list).into_response(),
        Err(error) => service_error(error),
    }
}

async fn revoke_device(
    State(state): State<Arc<RouteState>>,
    capabilities: Option<Extension<WebUiV2Capabilities>>,
    Path(device_id): Path<String>,
) -> Response {
    if !is_operator(capabilities) {
        return operator_required();
    }
    let Ok(device_id) = DeviceId::new(device_id) else {
        return not_found();
    };
    match state.service.revoke_device(&device_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => service_error(error),
    }
}

fn route_descriptors() -> Vec<IngressRouteDescriptor> {
    vec![
        descriptor(
            "webui.environment.descriptor",
            NetworkMethod::Get,
            ENVIRONMENT_DESCRIPTOR_PATH,
            public_policy(BodyLimitPolicy::NoBody, DESCRIPTOR_MAX_REQUESTS),
        ),
        descriptor(
            "webui.environment.pair",
            NetworkMethod::Post,
            PAIR_PATH,
            public_policy(
                BodyLimitPolicy::Limited {
                    max_bytes: PAIR_BODY_LIMIT_BYTES,
                },
                PAIR_MAX_REQUESTS,
            ),
        ),
        descriptor(
            "webui.environment.pair_tailnet",
            NetworkMethod::Post,
            PAIR_TAILNET_PATH,
            public_policy(
                BodyLimitPolicy::Limited {
                    max_bytes: PAIR_BODY_LIMIT_BYTES,
                },
                PAIR_MAX_REQUESTS,
            ),
        ),
        descriptor(
            "webui.environment.pairing_codes",
            NetworkMethod::Post,
            PAIRING_CODES_PATH,
            operator_policy(
                BodyLimitPolicy::Limited {
                    max_bytes: PAIRING_CODES_BODY_LIMIT_BYTES,
                },
                OPERATOR_MUTATION_MAX_REQUESTS,
            ),
        ),
        descriptor(
            "webui.environment.devices_list",
            NetworkMethod::Get,
            DEVICES_PATH,
            operator_policy(BodyLimitPolicy::NoBody, OPERATOR_READ_MAX_REQUESTS),
        ),
        descriptor(
            "webui.environment.device_revoke",
            NetworkMethod::Delete,
            DEVICE_PATH,
            operator_policy(BodyLimitPolicy::NoBody, OPERATOR_MUTATION_MAX_REQUESTS),
        ),
    ]
}

fn descriptor(
    route_id: &str,
    method: NetworkMethod,
    pattern: &str,
    policy: IngressPolicy,
) -> IngressRouteDescriptor {
    IngressRouteDescriptor::new(route_id.to_string(), method, pattern.to_string(), policy)
        .expect("environment access route descriptor must validate at startup") // safety: ids/patterns are crate-local literals; policies come from the sibling helpers below.
}

fn public_policy(body_limit: BodyLimitPolicy, max_requests: std::num::NonZeroU32) -> IngressPolicy {
    IngressPolicy::new(IngressPolicyParts {
        listener_class: ListenerClass::LocalGateway,
        auth: IngressAuthPolicy::Public {
            justification: public_justification(),
        },
        scope_source: IngressScopeSource::PublicRoute,
        body_limit,
        rate_limit: RateLimitPolicy::Limited {
            scope: RateLimitScope::PerIp,
            max_requests,
            window_seconds: RATE_WINDOW_SECONDS,
        },
        cors: CorsPolicy::SameOriginOnly,
        websocket_origin: WebSocketOriginPolicy::NotApplicable,
        streaming: StreamingMode::None,
        audit: AuditTraceClass::PublicCallback,
        effect_path: AllowedEffectPath::NoEffect,
    })
    .expect("environment access public policy must validate") // safety: LocalGateway + Public + NoEffect is permitted; rate-limit window/max are non-zero by construction.
}

fn operator_policy(
    body_limit: BodyLimitPolicy,
    max_requests: std::num::NonZeroU32,
) -> IngressPolicy {
    IngressPolicy::new(IngressPolicyParts {
        listener_class: ListenerClass::LocalGateway,
        auth: IngressAuthPolicy::Required {
            schemes: vec![IngressAuthScheme::BearerToken],
        },
        scope_source: IngressScopeSource::AuthenticatedCaller,
        body_limit,
        rate_limit: RateLimitPolicy::Limited {
            scope: RateLimitScope::PerCaller,
            max_requests,
            window_seconds: RATE_WINDOW_SECONDS,
        },
        cors: CorsPolicy::SameOriginOnly,
        websocket_origin: WebSocketOriginPolicy::NotApplicable,
        streaming: StreamingMode::None,
        audit: AuditTraceClass::UserAction,
        effect_path: AllowedEffectPath::ProductSurface,
    })
    .expect("environment access operator policy must validate") // safety: same authenticated local shape the channel-pairing routes use.
}

fn public_justification() -> IngressJustification {
    IngressJustification::new(
        "webui environment access",
        "a device that is not yet paired has no bearer: the descriptor is \
         read before pairing, and the pair routes verify a one-time code or \
         the identity listener's tailnet login before issuing a token",
    )
    .expect("environment access justification literal must validate") // safety: non-empty, no leading/trailing whitespace.
}
