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

use crate::reborn_support;

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
    runtime: ironclaw_composition::RebornRuntime,
    _root: TempDir,
}

impl PluginUiRuntime {
    async fn build() -> Self {
        Self::build_with(|_| {}).await
    }

    /// Build after `plant` wrote files under the runtime's storage root (a
    /// package planted at `system/extensions/<id>` loads as a third-party,
    /// `InstalledLocal` package).
    async fn build_with(plant: impl FnOnce(&std::path::Path)) -> Self {
        let root = tempfile::tempdir().expect("runtime tempdir");
        plant(&root.path().join("local-dev"));
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
            runtime,
            _root: root,
        }
    }

    /// Capability ids the model-visible surface publishes (the same owner-
    /// filtered projection a run's tool list is minted from).
    async fn model_visible_tools(&self) -> Vec<String> {
        ironclaw_composition::test_support::extension_management_for_test(&self.runtime)
            .active_model_visible_capabilities()
            .await
            .expect("model-visible capabilities")
            .into_iter()
            .map(|capability| capability.id.as_str().to_string())
            .collect()
    }

    async fn slot_extensions(&self, user: &str) -> Vec<String> {
        let (status, body) = self.get(user, "/api/webchat/v2/plugin-ui/slots").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let mut ids: Vec<String> = body["slots"]
            .as_array()
            .expect("slots array")
            .iter()
            .filter_map(|slot| slot["extension_id"].as_str().map(str::to_string))
            .collect();
        ids.dedup();
        ids
    }

    async fn install(&self, user: &str, extension_id: &str) -> Value {
        let (status, body) = self
            .post(
                user,
                "/api/webchat/v2/extensions/install",
                json!({
                    "package_ref": { "kind": "extension", "id": extension_id },
                    "client_action_id": format!("plugin-ui-install-{extension_id}-{user}"),
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "install response: {body}");
        body
    }

    async fn extension_entry(&self, user: &str, extension_id: &str) -> Value {
        let (status, body) = self.get(user, "/api/webchat/v2/extensions").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body["extensions"]
            .as_array()
            .expect("extensions array")
            .iter()
            .find(|entry| entry["package_ref"]["id"] == extension_id)
            .cloned()
            .unwrap_or_else(|| panic!("{extension_id} listed for {user}: {body}"))
    }

    async fn grant_consent(&self, user: &str, extension_id: &str) {
        let entry = self.extension_entry(user, extension_id).await;
        assert_eq!(entry["installation_state"], "consent_required", "{entry}");
        let digest = entry["consent"]["digest"]
            .as_str()
            .unwrap_or_else(|| panic!("consent digest listed: {entry}"))
            .to_string();
        let (status, body) = self
            .post(
                user,
                &format!("/api/webchat/v2/extensions/{extension_id}/consent"),
                json!({ "digest": digest, "client_action_id": format!("consent-{user}") }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "consent response: {body}");
        assert_eq!(body["installation_state"], "active", "{body}");
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

    // A UI-only plugin activates on its declared slots alone: its render tool
    // is not model-visible, so it is never advertised to the model.
    assert_eq!(
        rt.extension_entry("alice", EXTENSION_ID).await["installation_state"],
        "active"
    );
    assert!(
        !rt.model_visible_tools()
            .await
            .iter()
            .any(|id| id == RENDER_CAPABILITY),
        "the render tool must not reach the model's tool list"
    );
    let entry = rt.extension_entry("alice", EXTENSION_ID).await;
    assert!(
        entry["tools"]
            .as_array()
            .is_none_or(|tools| tools.is_empty()),
        "no model-visible tool is advertised for the plugin: {entry}"
    );

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

const THIRD_PARTY_UI_ID: &str = "acme-panel";

/// A third-party (`InstalledLocal`) UI plugin with a WASM render tool, planted
/// where the standalone host loads local packages.
fn plant_third_party_ui_plugin(storage_root: &std::path::Path) {
    let dir = storage_root.join(format!("system/extensions/{THIRD_PARTY_UI_ID}"));
    std::fs::create_dir_all(dir.join("wasm")).expect("mkdir");
    std::fs::create_dir_all(dir.join("schemas")).expect("mkdir");
    std::fs::write(
        dir.join("manifest.toml"),
        format!(
            r#"schema_version = "reborn.extension_manifest.v3"
id = "{THIRD_PARTY_UI_ID}"
name = "Acme Panel"
version = "0.1.0"
description = "A third-party plugin with one panel."
trust = "third_party"

[runtime]
kind = "wasm"
module = "wasm/panel.wasm"

[[tools]]
origin_gate_matrix = {{ loop_run = "forbidden", product = "consent_sufficient", automation = "forbidden" }}
id = "{THIRD_PARTY_UI_ID}.render"
description = "Render the panel."
effects = []
default_permission = "allow"
visibility = "api"
input_schema_ref = "schemas/render.input.v1.json"

[[ui.slots]]
id = "panel"
slot = "panel.tab"
title = "Acme"
render = "{THIRD_PARTY_UI_ID}.render"
"#
        ),
    )
    .expect("manifest");
    std::fs::write(
        dir.join("schemas/render.input.v1.json"),
        br#"{"type":"object"}"#,
    )
    .expect("schema");
    std::fs::write(dir.join("wasm/panel.wasm"), b"\0asm\x01\0\0\0").expect("module");
}

/// Slots of a third-party plugin are listed (and so renderable) only for a
/// user for whom it is active *and* consented: installing is not enough, one
/// user's grant does not cover another, and a non-member sees nothing.
#[tokio::test]
async fn third_party_plugin_slots_follow_each_users_consent() {
    let rt = PluginUiRuntime::build_with(plant_third_party_ui_plugin).await;
    let render = |user: &'static str| {
        let rt = &rt;
        async move {
            rt.post(
                user,
                "/api/webchat/v2/plugin-ui/render",
                json!({
                    "extension_id": THIRD_PARTY_UI_ID,
                    "slot_id": "panel",
                    "context": { "thread_id": null },
                }),
            )
            .await
            .0
        }
    };

    rt.install("alice", THIRD_PARTY_UI_ID).await;
    assert_eq!(
        rt.extension_entry("alice", THIRD_PARTY_UI_ID).await["installation_state"],
        "consent_required"
    );
    assert!(
        rt.slot_extensions("alice").await.is_empty(),
        "installed is not consented"
    );
    assert_eq!(render("alice").await, StatusCode::NOT_FOUND);

    rt.grant_consent("alice", THIRD_PARTY_UI_ID).await;
    assert_eq!(rt.slot_extensions("alice").await, [THIRD_PARTY_UI_ID]);
    assert!(
        !rt.model_visible_tools()
            .await
            .iter()
            .any(|id| id == &format!("{THIRD_PARTY_UI_ID}.render")),
        "the plugin's render tool is not advertised to the model"
    );

    // Bob is not a member: nothing, and naming the slot is not found.
    assert!(rt.slot_extensions("bob").await.is_empty());
    assert_eq!(render("bob").await, StatusCode::NOT_FOUND);
    // Bob joins: still nothing until his own grant.
    rt.install("bob", THIRD_PARTY_UI_ID).await;
    assert!(
        rt.slot_extensions("bob").await.is_empty(),
        "alice's consent does not cover bob"
    );
    assert_eq!(render("bob").await, StatusCode::NOT_FOUND);
    rt.grant_consent("bob", THIRD_PARTY_UI_ID).await;
    assert_eq!(rt.slot_extensions("bob").await, [THIRD_PARTY_UI_ID]);
}
