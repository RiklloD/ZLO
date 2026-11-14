//! `config.toml` hot reload against a fully composed runtime.
//!
//! Boots `build_reborn_runtime` with a real boot config (temp `ZLO_HOME`) whose
//! `[llm.default]` points the `lmstudio` provider (OpenAI-compatible, no key)
//! at a local endpoint. The endpoint records the `model` of every chat
//! request — the seam: it is what the provider actually sent upstream after
//! the full `ironclaw_llm` chain ran. Each test edits the file, calls the same
//! `RebornConfigReloader::reload` that `zlo serve`'s SIGHUP handler calls
//! (built the way serve builds it, over the boot-parsed snapshot), and asserts
//! on the next model request.
//!
//! Tier note: this drives `build_reborn_runtime` rather than
//! `RebornIntegrationHarness`, because the harness assembles its own stack
//! below composition and never wires a boot config or the live LLM reload
//! handle — the two things under test.

use std::sync::{Arc, Mutex};

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use ironclaw_composition::{
    RebornRuntime, RebornRuntimeIdentity, RebornRuntimeInput, build_reborn_runtime,
    local_filesystem_build_input, standalone_runtime_policy,
};
use ironclaw_config::{
    ConfigReloadOutcome, RebornBootConfig, RebornConfigFile, RebornHome, RebornProfile,
};
use serde_json::{Value, json};
use tempfile::TempDir;

#[derive(Clone, Default)]
struct SeenModels(Arc<Mutex<Vec<String>>>);

impl SeenModels {
    fn last(&self) -> Option<String> {
        self.0.lock().expect("lock").last().cloned()
    }
}

async fn chat(State(seen): State<SeenModels>, Json(body): Json<Value>) -> Response {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    seen.0.lock().expect("lock").push(model);
    if body.get("stream").and_then(Value::as_bool) == Some(true) {
        let sse = concat!(
            r#"data: {"id":"c1","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
            "\n\n",
            "data: [DONE]\n\n"
        );
        return ([("content-type", "text/event-stream")], sse).into_response();
    }
    Json(json!({
        "id": "c1",
        "object": "chat.completion",
        "created": 1,
        "model": "m",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }))
    .into_response()
}

async fn start_endpoint() -> (String, SeenModels) {
    let seen = SeenModels::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", get(|| async { Json(json!({"data": []})) }))
        .with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind endpoint");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve endpoint");
    });
    (format!("http://{addr}/v1"), seen)
}

struct Booted {
    _root: TempDir,
    config_path: std::path::PathBuf,
    base_url: String,
    seen: SeenModels,
    runtime: RebornRuntime,
    /// The file as boot parsed it: the reloader's restart-required baseline.
    boot_file: Option<RebornConfigFile>,
}

impl Booted {
    fn llm_section(&self, model: &str) -> String {
        format!(
            "[llm.default]\nprovider_id = \"lmstudio\"\nmodel = \"{model}\"\nbase_url = \"{}\"\n",
            self.base_url
        )
    }

    fn write(&self, text: &str) {
        std::fs::write(&self.config_path, text).expect("write config.toml");
    }

    /// Send one message and return the model the provider asked upstream for.
    async fn model_of_next_turn(&self) -> String {
        let conversation = self.runtime.new_conversation().await.expect("conversation");
        let reply = self
            .runtime
            .send_user_message(&conversation, "hi")
            .await
            .expect("message sends");
        assert!(reply.is_successful_final_reply(), "reply: {reply:?}");
        self.seen.last().expect("the endpoint saw a model call")
    }

    /// A fresh reloader per call on purpose: all reload state lives in the
    /// runtime's shared live-LLM owner, not in the reloader.
    async fn reload(&self) -> ironclaw_config::ConfigReloadReport {
        self.runtime
            .config_reloader(self.boot_file.clone())
            .expect("a boot-config runtime has a reloader")
            .reload()
            .await
    }
}

async fn boot(initial_model: &str, extra: &str) -> Booted {
    let (base_url, seen) = start_endpoint().await;
    let root = tempfile::tempdir().expect("tempdir");
    let home_dir = root.path().join("zlo-home");
    std::fs::create_dir_all(&home_dir).expect("home dir");
    let home = RebornHome::resolve_from_env_parts(Some(home_dir.into_os_string()), None, None)
        .expect("home");
    let config_path = home.config_file_path();
    std::fs::write(
        &config_path,
        format!(
            "[llm.default]\nprovider_id = \"lmstudio\"\nmodel = \"{initial_model}\"\nbase_url = \"{base_url}\"\n{extra}"
        ),
    )
    .expect("write config.toml");
    let boot_file = RebornConfigFile::load(&config_path).expect("boot parse");
    let input = RebornRuntimeInput::from_build_input(
        local_filesystem_build_input("config-reload-owner", root.path().join("storage"))
            .with_runtime_policy(standalone_runtime_policy().expect("standalone policy")),
    )
    .with_boot_config(RebornBootConfig::new(home, RebornProfile::Standalone))
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "config-reload-tenant".to_string(),
        agent_id: "config-reload-agent".to_string(),
        source_binding_id: "config-reload-source".to_string(),
        reply_target_binding_id: "config-reload-reply".to_string(),
    });
    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    Booted {
        _root: root,
        config_path,
        base_url,
        seen,
        runtime,
        boot_file,
    }
}

#[tokio::test]
async fn edited_default_model_applies_to_the_next_turn_without_restart() {
    let b = boot("model-a", "").await;
    assert_eq!(b.model_of_next_turn().await, "model-a");

    b.write(&b.llm_section("model-b"));
    let report = b.reload().await;
    assert_eq!(report.outcome, ConfigReloadOutcome::Applied, "{report}");
    assert_eq!(report.applied, vec!["llm.default"]);
    assert!(report.restart_required.is_empty(), "{report}");

    assert_eq!(b.model_of_next_turn().await, "model-b");
    // Applied once: a second reload of the same file is a no-op.
    assert_eq!(b.reload().await.outcome, ConfigReloadOutcome::Unchanged);
    b.runtime.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn invalid_config_is_rejected_and_the_old_model_keeps_serving() {
    let b = boot("model-a", "").await;

    // Unparseable file: rejected before anything is touched.
    b.write("[llm.default\nmodel = \"model-b\"\n");
    let report = b.reload().await;
    assert!(
        matches!(report.outcome, ConfigReloadOutcome::Rejected { .. }),
        "{report}"
    );
    assert!(report.applied.is_empty());
    assert_eq!(b.model_of_next_turn().await, "model-a");

    // Parses, but the live seam refuses it: the selection does not resolve
    // (unknown provider), so nothing is built or swapped and the old provider
    // stays wired. A selection that resolves but whose chain fails to build
    // leaves the old chain in place too; that is pinned where the build
    // happens, `ironclaw_llm`'s
    // `llm_reload_handle_preserves_old_chain_on_build_failure`.
    b.write("[llm.default]\nprovider_id = \"no-such-provider\"\nmodel = \"model-b\"\n");
    let report = b.reload().await;
    assert!(
        matches!(report.outcome, ConfigReloadOutcome::Rejected { .. }),
        "{report}"
    );
    assert_eq!(b.model_of_next_turn().await, "model-a");

    // Fixing the file applies against the untouched baseline.
    b.write(&b.llm_section("model-b"));
    assert_eq!(b.reload().await.outcome, ConfigReloadOutcome::Applied);
    assert_eq!(b.model_of_next_turn().await, "model-b");
    b.runtime.shutdown().await.expect("shutdown");
}

/// The boot parse, not a re-read at the end of assembly, is the baseline: a
/// boot-time key edited while the server was starting is still reported.
#[tokio::test]
async fn edit_racing_boot_is_reported_restart_required() {
    let b = boot("model-a", "").await;
    // Lands after the boot parse but before any reload.
    b.write(&format!(
        "{}[webui]\nlisten_port = 4300\n",
        b.llm_section("model-a")
    ));
    let report = b.reload().await;
    assert_eq!(
        report.outcome,
        ConfigReloadOutcome::RestartRequired,
        "{report}"
    );
    assert_eq!(report.restart_required, vec!["webui"]);
    assert_eq!(b.model_of_next_turn().await, "model-a");
    b.runtime.shutdown().await.expect("shutdown");
}

#[tokio::test]
async fn boot_time_settings_are_reported_restart_required_and_not_applied() {
    let b = boot("model-a", "[webui]\nlisten_port = 4100\n").await;

    // Only boot-time keys change: listener port and the WebUI origin allowlist.
    b.write(&format!(
        "{}[webui]\nlisten_port = 4200\nallowed_origins = [\"https://evil.example\"]\n[identity]\ntenant = \"other-tenant\"\n",
        b.llm_section("model-a")
    ));
    let report = b.reload().await;
    assert_eq!(
        report.outcome,
        ConfigReloadOutcome::RestartRequired,
        "{report}"
    );
    assert_eq!(report.restart_required, vec!["identity", "webui"]);
    assert!(report.applied.is_empty());

    // A mixed edit applies the live key and still reports the boot-time ones.
    b.write(&format!(
        "{}[webui]\nlisten_port = 4200\n[identity]\ntenant = \"other-tenant\"\n",
        b.llm_section("model-b")
    ));
    let report = b.reload().await;
    assert_eq!(report.outcome, ConfigReloadOutcome::Applied, "{report}");
    assert_eq!(report.applied, vec!["llm.default"]);
    assert_eq!(report.restart_required, vec!["identity", "webui"]);
    assert_eq!(b.model_of_next_turn().await, "model-b");
    b.runtime.shutdown().await.expect("shutdown");
}

/// The Settings → Inference save (`POST /api/webchat/v2/llm/active`) through
/// the real WebUI router over this runtime's production product surface, as
/// an operator. Returns the status and JSON body.
async fn settings_set_active(b: &Booted, body: Value) -> (axum::http::StatusCode, Value) {
    use ironclaw_host_api::ids::{AgentId, TenantId, UserId};
    use ironclaw_product_contracts::surface::ProductSurfaceCaller;
    use ironclaw_webui::webui_v2::{
        DEFAULT_SSE_MAX_CONCURRENT_PER_CALLER, WebUiV2Capabilities, WebUiV2State, webui_v2_router,
    };
    use tower::ServiceExt;

    let surface = b
        .runtime
        .product_surface(None)
        .expect("production product surface builds");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("config-reload-tenant").expect("tenant"),
        UserId::new("config-reload-owner").expect("user"),
        Some(AgentId::new("config-reload-agent").expect("agent")),
        None,
    )
    .with_operator_config(true);
    let router = webui_v2_router(WebUiV2State::new(
        surface,
        DEFAULT_SSE_MAX_CONCURRENT_PER_CALLER,
    ))
    .layer(axum::Extension(caller))
    .layer(axum::Extension(WebUiV2Capabilities {
        operator_webui_config: true,
    }));
    let response = router
        .oneshot(
            axum::http::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/api/webchat/v2/llm/active")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("router answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

fn default_slot(path: &std::path::Path) -> ironclaw_config::LlmSlotSelection {
    RebornConfigFile::load(path)
        .expect("config.toml parses")
        .and_then(|file| file.default_llm_slot().cloned())
        .expect("[llm.default] present")
}

/// A Settings save that keeps the provider and changes only the model must
/// keep the operator's hand-set slot overrides: here `base_url` is the only
/// thing pointing the provider at the endpoint, so dropping it would leave
/// the file unable to serve.
#[tokio::test]
async fn settings_model_change_for_the_same_provider_keeps_hand_set_overrides() {
    let b = boot("model-a", "").await;
    assert_eq!(b.model_of_next_turn().await, "model-a");

    let (status, body) =
        settings_set_active(&b, json!({"provider_id": "lmstudio", "model": "model-b"})).await;
    assert!(status.is_success(), "{status}: {body}");

    let slot = default_slot(&b.config_path);
    assert_eq!(slot.model.as_deref(), Some("model-b"));
    assert_eq!(
        slot.base_url.as_deref(),
        Some(b.base_url.as_str()),
        "the hand-set base_url survives a same-provider save"
    );
    assert_eq!(b.model_of_next_turn().await, "model-b");
    // The file and the live provider agree: a reload has nothing to do.
    assert_eq!(b.reload().await.outcome, ConfigReloadOutcome::Unchanged);
    b.runtime.shutdown().await.expect("shutdown");
}

/// A Settings save whose selection cannot be applied to the running server
/// is refused: non-2xx with a stable reason, `config.toml` is left as it
/// was, and the old model keeps serving — never "200, applies on restart"
/// with the file and the running provider disagreeing.
#[tokio::test]
async fn settings_save_that_cannot_be_applied_live_is_refused_and_rolled_back() {
    let b = boot("model-a", "").await;
    assert_eq!(b.model_of_next_turn().await, "model-a");
    let before = std::fs::read_to_string(&b.config_path).expect("read config");

    // `openai_compatible` requires a base_url; switching provider resets the
    // slot to that provider's catalog defaults, which carry none.
    let (status, body) = settings_set_active(
        &b,
        json!({"provider_id": "openai_compatible", "model": "model-x"}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "conflict", "{body}");
    assert_eq!(body["kind"], "not_applied", "{body}");
    assert_eq!(body["field"], "llm.default", "{body}");

    let slot = default_slot(&b.config_path);
    assert_eq!(slot.provider_id.as_deref(), Some("lmstudio"));
    assert_eq!(slot.model.as_deref(), Some("model-a"));
    assert_eq!(slot.base_url.as_deref(), Some(b.base_url.as_str()));
    assert_eq!(
        std::fs::read_to_string(&b.config_path).expect("read config"),
        before,
        "the refused save leaves config.toml byte-for-byte as it was"
    );
    assert_eq!(b.model_of_next_turn().await, "model-a");
    assert_eq!(b.reload().await.outcome, ConfigReloadOutcome::Unchanged);
    b.runtime.shutdown().await.expect("shutdown");
}
