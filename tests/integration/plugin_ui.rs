//! Plugin UI slots (`reborn.plugin_ui.v1`) end to end over the real WebUI v2
//! routes and a real production-composed runtime.
//!
//! The `plugin-ui-demo` fixture (`tests/fixtures/extensions/plugin-ui-demo/`)
//! is injected as a host-bundled first-party package with its render tool's
//! handler registered through the same registrar seam the binary uses. The
//! user installs it through `/extensions/install`, then the slot routes list
//! it, render it by invoking the render tool through the product-origin
//! capability path (authorization, the first-party lane), round-trip an
//! event, and reject trees that break the contract. A new bin because no
//! other scenario wires a fixture first-party render handler.

#[allow(dead_code)]
#[path = "support/mod.rs"]
mod reborn_support;
#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod support;

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use axum::http::StatusCode;
use ironclaw_composition::test_support::BudgetTestGateway;
use ironclaw_composition::{
    RebornRuntimeIdentity, RebornRuntimeInput, build_reborn_runtime, standalone_runtime_policy,
};
use ironclaw_extension_host::{
    FirstPartyCapabilityError, FirstPartyCapabilityHandler, FirstPartyCapabilityRegistry,
    FirstPartyCapabilityRequest, FirstPartyCapabilityResult, FirstPartyHandlerRegistrar,
    FirstPartyPackageAsset, FirstPartyPackageBundle, FirstPartyRegistrarContext,
};
use ironclaw_host_api::{
    capability::EffectKind,
    error::HostApiError,
    ids::{AgentId, CapabilityId, TenantId, UserId},
    resource::ResourceUsage,
};
use ironclaw_product_contracts::surface::{ProductSurface, ProductSurfaceCaller};
use reborn_support::webui_mount::{get_json, mount_webui_v2_router, post_json};
use serde_json::{Value, json};
use tempfile::TempDir;

const EXTENSION_ID: &str = "plugin-ui-demo";
const RENDER_CAPABILITY: &str = "plugin-ui-demo.render";
const TENANT: &str = "plugin-ui-tenant";
const AGENT: &str = "plugin-ui-agent";

fn fixture(path: &str) -> Vec<u8> {
    let full = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/extensions/plugin-ui-demo")
        .join(path);
    std::fs::read(&full).unwrap_or_else(|error| panic!("read {}: {error}", full.display()))
}

fn demo_bundle() -> FirstPartyPackageBundle {
    let manifest = String::from_utf8(fixture("manifest.toml")).expect("utf-8 manifest");
    let assets = [
        "manifest.toml",
        "schemas/plugin-ui-demo/render.input.v1.json",
        "icons/demo.svg",
    ]
    .into_iter()
    .map(|path| FirstPartyPackageAsset {
        path: path.to_string(),
        bytes: Cow::Owned(fixture(path)),
    })
    .collect();
    FirstPartyPackageBundle {
        id: EXTENSION_ID.to_string(),
        display_name: "Plugin UI Demo".to_string(),
        manifest_toml: manifest,
        assets,
        onboarding: None,
        trust_effects: Some(vec![EffectKind::DispatchCapability]),
        search_aliases: Vec::new(),
    }
}

/// The fixture's render tool: a stateless counter panel, a badge, a settings
/// page, a contract-breaking tree, and an oversize tree, keyed by `slot_id`.
struct DemoRenderHandler;

#[async_trait]
impl FirstPartyCapabilityHandler for DemoRenderHandler {
    async fn dispatch(
        &self,
        request: FirstPartyCapabilityRequest,
    ) -> Result<FirstPartyCapabilityResult, FirstPartyCapabilityError> {
        let input = request.input;
        let count = match (
            &input["event"]["action"],
            &input["event"]["payload"]["count"],
        ) {
            (Value::String(action), count) if action == "increment" => {
                count.as_u64().unwrap_or(0) + 1
            }
            _ => 0,
        };
        let thread = input["context"]["thread_id"].as_str().unwrap_or("none");
        let output = match input["slot_id"].as_str() {
            Some("panel") => json!({
                "type": "col",
                "gap": 8,
                "children": [
                    { "type": "text", "text": format!("Count: {count}"), "style": "title" },
                    { "type": "text", "text": format!("thread: {thread}"), "style": "caption" },
                    { "type": "button", "label": "Add one", "action": "increment",
                      "payload": { "count": count }, "style": "primary" },
                ],
            }),
            Some("badge") => json!({ "type": "badge", "text": "3", "tone": "accent" }),
            Some("settings") => json!({
                "type": "col",
                "children": [
                    { "type": "markdown", "source": "## Demo settings" },
                    { "type": "divider" },
                    { "type": "list", "items": [{ "title": "Mode", "subtitle": "demo",
                      "trailing": { "type": "badge", "text": "on", "tone": "success" } }] },
                ],
            }),
            Some("broken") => json!({
                "type": "col",
                "children": [{ "type": "iframe", "src": "https://example.invalid" }],
            }),
            _ => json!({ "type": "markdown", "source": "x".repeat(80 * 1024) }),
        };
        Ok(FirstPartyCapabilityResult::new(
            output,
            ResourceUsage::default(),
        ))
    }
}

struct DemoRegistrar;

impl FirstPartyHandlerRegistrar for DemoRegistrar {
    fn register(
        &self,
        registry: &mut FirstPartyCapabilityRegistry,
        _context: &FirstPartyRegistrarContext,
    ) -> Result<(), HostApiError> {
        registry.insert_handler(
            CapabilityId::new(RENDER_CAPABILITY)?,
            Arc::new(DemoRenderHandler),
        );
        Ok(())
    }
}

struct PluginUiRuntime {
    surface: Arc<dyn ProductSurface>,
    _root: TempDir,
}

impl PluginUiRuntime {
    async fn build() -> Self {
        let root = tempfile::tempdir().expect("runtime tempdir");
        let tenant = TenantId::new(TENANT).expect("tenant");
        let agent = AgentId::new(AGENT).expect("agent");
        let base = ironclaw_composition::local_filesystem_build_input(
            "plugin-ui-owner",
            root.path().join("local-dev"),
        )
        .with_local_runtime_identity(tenant, agent)
        .with_runtime_policy(standalone_runtime_policy().expect("policy"))
        .with_bundled_first_party_for_test();
        let mut bundles =
            ironclaw_extension_host::test_support::first_party_bundles_from_inventory();
        bundles.push(demo_bundle());
        let mut registrars = ironclaw_extension_host::test_support::first_party_registrars::bundled_first_party_registrars();
        registrars.push(Arc::new(DemoRegistrar));
        let input = base
            .with_first_party_bundles(bundles)
            .with_first_party_registrars(registrars);
        let runtime = build_reborn_runtime(
            RebornRuntimeInput::from_build_input(input)
                .with_identity(RebornRuntimeIdentity {
                    tenant_id: TENANT.to_string(),
                    agent_id: AGENT.to_string(),
                    source_binding_id: "plugin-ui-source".to_string(),
                    reply_target_binding_id: "plugin-ui-reply".to_string(),
                })
                .with_model_gateway_override(Arc::new(BudgetTestGateway::with_constant(
                    "unused", 0, 0,
                ))),
        )
        .await
        .expect("production runtime builds");
        let surface = runtime
            .product_surface(None)
            .expect("product surface builds");
        Self {
            surface,
            _root: root,
        }
    }

    fn caller(&self, user: &str) -> ProductSurfaceCaller {
        ProductSurfaceCaller::new(
            TenantId::new(TENANT).expect("tenant"),
            UserId::new(user).expect("user"),
            Some(AgentId::new(AGENT).expect("agent")),
            None,
        )
    }

    async fn get(&self, user: &str, path: &str) -> (StatusCode, Value) {
        get_json(
            mount_webui_v2_router(Arc::clone(&self.surface), self.caller(user)),
            path,
        )
        .await
    }

    async fn post(&self, user: &str, path: &str, body: Value) -> (StatusCode, Value) {
        post_json(
            mount_webui_v2_router(Arc::clone(&self.surface), self.caller(user)),
            path,
            body,
        )
        .await
    }

    async fn install_demo(&self, user: &str) {
        let (status, body) = self
            .post(
                user,
                "/api/webchat/v2/extensions/install",
                json!({
                    "package_ref": { "kind": "extension", "id": EXTENSION_ID },
                    "client_action_id": format!("plugin-ui-install-{user}"),
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "install response: {body}");
        assert_eq!(body["success"], true, "install response: {body}");
    }

    async fn render(&self, user: &str, slot_id: &str) -> (StatusCode, Value) {
        self.post(
            user,
            "/api/webchat/v2/plugin-ui/render",
            json!({
                "extension_id": EXTENSION_ID,
                "slot_id": slot_id,
                "context": { "thread_id": null },
            }),
        )
        .await
    }
}

#[tokio::test]
async fn plugin_ui_slots_list_render_and_round_trip_events_through_the_capability_path() {
    let rt = PluginUiRuntime::build().await;

    // Nothing is listed before the plugin is installed and active.
    let (status, body) = rt.get("alice", "/api/webchat/v2/plugin-ui/slots").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "slots": [] }));
    let (status, _) = rt.render("alice", "panel").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    rt.install_demo("alice").await;

    let (status, body) = rt.get("alice", "/api/webchat/v2/plugin-ui/slots").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let slots = body["slots"].as_array().expect("slots array");
    let listed: Vec<(&str, &str)> = slots
        .iter()
        .map(|slot| {
            (
                slot["slot_id"].as_str().unwrap_or_default(),
                slot["slot"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        listed,
        vec![
            ("panel", "panel.tab"),
            ("badge", "sidebar.session_row.badge"),
            ("settings", "settings.page"),
            ("broken", "panel.tab"),
            ("oversize", "panel.tab"),
        ]
    );
    assert_eq!(
        slots[0],
        json!({
            "extension_id": EXTENSION_ID,
            "slot_id": "panel",
            "slot": "panel.tab",
            "title": "Demo",
            "icon": "icons/demo.svg",
        })
    );

    // A real render: the tool ran through the lane and its tree came back.
    let (status, body) = rt
        .post(
            "alice",
            "/api/webchat/v2/plugin-ui/render",
            json!({
                "extension_id": EXTENSION_ID,
                "slot_id": "panel",
                "context": { "thread_id": "thread-123" },
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tree"]["children"][0]["text"], "Count: 0", "{body}");
    assert_eq!(
        body["tree"]["children"][1]["text"], "thread: thread-123",
        "{body}"
    );
    let button = &body["tree"]["children"][2];
    assert_eq!(button["action"], "increment", "{body}");

    // Event round-trip: the button's action + payload re-render the slot.
    let (status, body) = rt
        .post(
            "alice",
            "/api/webchat/v2/plugin-ui/event",
            json!({
                "extension_id": EXTENSION_ID,
                "slot_id": "panel",
                "context": { "thread_id": null },
                "action": button["action"],
                "payload": button["payload"],
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["tree"]["children"][0]["text"], "Count: 1", "{body}");
    assert_eq!(
        body["tree"]["children"][2]["payload"],
        json!({ "count": 1 })
    );

    // The badge slot and the settings page render their kinds.
    let (_, body) = rt.render("alice", "badge").await;
    assert_eq!(
        body,
        json!({ "tree": { "type": "badge", "text": "3", "tone": "accent" } })
    );
    let (_, body) = rt.render("alice", "settings").await;
    assert_eq!(
        body["tree"]["children"][1],
        json!({ "type": "divider" }),
        "{body}"
    );

    // A tree that breaks the contract is a plain, user-visible error — never
    // a crash and never a tree.
    let (status, body) = rt.render("alice", "broken").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "error": "the plugin's UI is invalid at $.children[0]: unknown element type `iframe`" })
    );
    let (status, body) = rt.render("alice", "oversize").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let error = body["error"].as_str().expect("oversize is an error");
    assert!(error.contains("more than the 65536-byte limit"), "{error}");
    assert!(body.get("tree").is_none());
}

#[tokio::test]
async fn plugin_ui_slots_are_scoped_to_the_installing_user_and_requests_are_bounded() {
    let rt = PluginUiRuntime::build().await;
    rt.install_demo("alice").await;

    // Bob never installed the (private) plugin: no slots, and naming one is
    // not found rather than rendered.
    let (status, body) = rt.get("bob", "/api/webchat/v2/plugin-ui/slots").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "slots": [] }));
    let (status, _) = rt.render("bob", "panel").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Unknown slots and oversize event payloads are rejected before any
    // render tool runs (malformed bodies are axum's 422; the action and
    // field grammar is pinned by the contracts suite).
    let (status, _) = rt.render("alice", "nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let event = |action: Value, payload: Value| {
        json!({
            "extension_id": EXTENSION_ID, "slot_id": "panel",
            "action": action, "payload": payload,
        })
    };
    let (status, _) = rt
        .post(
            "alice",
            "/api/webchat/v2/plugin-ui/event",
            event(json!("increment"), json!({ "blob": "x".repeat(20 * 1024) })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "oversize event payload");
}
