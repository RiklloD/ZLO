//! "Triage an issue" (`docs/using/triage.mdx`): `POST /api/operator/triage`
//! asks the backend to start an in-app triage session and answers the
//! composer text that links its playbook prompt.
//!
//! Bearer-authenticated and operator-only: the session files name this
//! machine's paths and the agent that reads them investigates the install.
//! The body is a small JSON [`PrepareTriageRequest`] (bounded by the route
//! descriptor before the handler); this module trims each client field to
//! [`MAX_TRIAGE_CLIENT_FIELD_CHARS`] characters and drops control
//! characters, because the text lands in a Markdown file an agent reads. No
//! field selects a path or a command. The binary implements
//! [`TriageService`] (session writer, source clone); this module owns HTTP
//! and the operator check.

use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use ironclaw_host_api::action::NetworkMethod;
use ironclaw_host_api::ingress::{
    AllowedEffectPath, AuditTraceClass, BodyLimitPolicy, CorsPolicy, IngressAuthPolicy,
    IngressAuthScheme, IngressPolicy, IngressPolicyParts, IngressRouteDescriptor,
    IngressScopeSource, ListenerClass, RateLimitPolicy, RateLimitScope, StreamingMode,
    WebSocketOriginPolicy,
};
use ironclaw_host_ingress::ProtectedRouteMount;
use ironclaw_product_contracts::triage::{
    MAX_TRIAGE_CLIENT_FIELD_CHARS, OPERATOR_TRIAGE_PATH, PrepareTriageRequest,
    PrepareTriageResponse, TriageClient,
};
use serde::Serialize;

use crate::webui_v2::WebUiV2Capabilities;

const RATE_WINDOW_SECONDS: std::num::NonZeroU32 = std::num::NonZeroU32::new(60).expect("60 != 0"); // safety: const-evaluated, literal non-zero
/// Each call writes a session folder and may start a clone.
const PREPARE_MAX_REQUESTS: std::num::NonZeroU32 = std::num::NonZeroU32::new(10).expect("10 != 0"); // safety: const-evaluated, literal non-zero
/// Three short fields of JSON.
const MAX_BODY_BYTES: std::num::NonZeroU64 = std::num::NonZeroU64::new(4096).expect("4096 != 0"); // safety: const-evaluated, literal non-zero

/// Why a session could not start. The cause was logged by the implementor.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TriageError {
    /// The session files could not be written.
    #[error("triage session could not be written: {0}")]
    SessionUnwritable(String),
}

/// Starts in-app triage sessions. Declared here, where its only caller is;
/// implemented by the `ironclaw` binary, which writes the same files
/// `zlo triage` does and fetches the source of its own build.
#[async_trait]
pub trait TriageService: Send + Sync {
    async fn prepare(&self, client: TriageClient) -> Result<PrepareTriageResponse, TriageError>;
}

/// Bearer-authed mount of the triage route over `service`.
pub fn triage_route_mount(service: Arc<dyn TriageService>) -> ProtectedRouteMount {
    let router = Router::new()
        .route(OPERATOR_TRIAGE_PATH, post(prepare_triage))
        .with_state(service);
    ProtectedRouteMount::new(router, vec![descriptor()])
}

#[derive(Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

fn error_response(status: StatusCode, error: &'static str, message: String) -> Response {
    (status, Json(ErrorBody { error, message })).into_response()
}

async fn prepare_triage(
    State(service): State<Arc<dyn TriageService>>,
    capabilities: Option<Extension<WebUiV2Capabilities>>,
    body: Result<Json<PrepareTriageRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if !capabilities.is_some_and(|Extension(caps)| caps.operator_webui_config) {
        return error_response(
            StatusCode::FORBIDDEN,
            "operator_required",
            "Operator privileges required.".into(),
        );
    }
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                rejection.body_text(),
            );
        }
    };
    match service.prepare(bounded_client(request.client)).await {
        Ok(prepared) => Json(prepared).into_response(),
        Err(error) => {
            tracing::debug!(%error, "triage prepare failed");
            error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "triage_unavailable",
                "The backend could not write the triage session. Run `zlo triage` in a terminal \
                 on that machine instead."
                    .into(),
            )
        }
    }
}

/// Each field trimmed, one line, at most [`MAX_TRIAGE_CLIENT_FIELD_CHARS`]
/// characters; an empty field is dropped.
fn bounded_client(client: TriageClient) -> TriageClient {
    let bound = |value: Option<String>| {
        value
            .map(|text| {
                text.chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect::<String>()
                    .trim()
                    .chars()
                    .take(MAX_TRIAGE_CLIENT_FIELD_CHARS)
                    .collect::<String>()
            })
            .filter(|text| !text.is_empty())
    };
    TriageClient {
        label: bound(client.label),
        app_version: bound(client.app_version),
        os: bound(client.os),
    }
}

fn descriptor() -> IngressRouteDescriptor {
    IngressRouteDescriptor::new(
        "webui.operator.triage_prepare".to_string(),
        NetworkMethod::Post,
        OPERATOR_TRIAGE_PATH.to_string(),
        IngressPolicy::new(IngressPolicyParts {
            listener_class: ListenerClass::LocalGateway,
            auth: IngressAuthPolicy::Required {
                schemes: vec![IngressAuthScheme::BearerToken],
            },
            scope_source: IngressScopeSource::AuthenticatedCaller,
            body_limit: BodyLimitPolicy::Limited {
                max_bytes: MAX_BODY_BYTES,
            },
            rate_limit: RateLimitPolicy::Limited {
                scope: RateLimitScope::PerCaller,
                max_requests: PREPARE_MAX_REQUESTS,
                window_seconds: RATE_WINDOW_SECONDS,
            },
            cors: CorsPolicy::SameOriginOnly,
            websocket_origin: WebSocketOriginPolicy::NotApplicable,
            streaming: StreamingMode::None,
            audit: AuditTraceClass::UserAction,
            effect_path: AllowedEffectPath::ProductSurface,
        })
        .expect("triage operator policy must validate"), // safety: same authenticated local shape the self-update operator routes use, with a fixed body bound.
    )
    .expect("triage route descriptor must validate at startup") // safety: id/pattern are crate-local literals; the policy is built above.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_fields_are_one_bounded_line() {
        let long = "x".repeat(MAX_TRIAGE_CLIENT_FIELD_CHARS + 20);
        let bounded = bounded_client(TriageClient {
            label: Some("  ZLO\ndesktop\u{7} ".into()),
            app_version: Some(long),
            os: Some("   ".into()),
        });
        assert_eq!(bounded.label.as_deref(), Some("ZLO desktop"));
        assert_eq!(
            bounded.app_version.map(|v| v.chars().count()),
            Some(MAX_TRIAGE_CLIENT_FIELD_CHARS)
        );
        assert_eq!(bounded.os, None);
    }
}
