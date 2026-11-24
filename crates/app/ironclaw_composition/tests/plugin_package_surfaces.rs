//! End-to-end proof, through `build_reborn_runtime`, that a third-party
//! package's `[[hooks]]` and `[[settings]]` are executed by the host on the
//! package's behalf:
//!
//! - a package on disk, or installed without the user's consent,
//!   contributes nothing;
//! - a hook module swapped after consent never compiles;
//! - once consented and activated, its WASM `before_capability` hook fires on
//!   a real run at the `Installed` tier and decides from the user's settings;
//! - a model-facing schema changed after consent is not published;
//! - a setting changed through the product surface reaches the hook on the
//!   next run and flips its decision, and the plugin's own WASM tool then
//!   runs and receives the same settings;
//! - the package is private to the installing user: another user of the same
//!   tenant does not get its tool, hooks, or settings (404, the same answer as
//!   an unknown extension) — not even after joining the installation, until
//!   their own consent grant.
//!
//! Unregistration on deactivation is pinned by the live installer's unit
//! test over the same active registry.
//!
//! The package is the shape a plugin author writes: one directory with a v3
//! manifest, a schema, a WASM tool component, and a WASM hook module.

#![deny(clippy::let_underscore_must_use)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_composition::{
    HooksActivationConfig, RebornRuntimeIdentity, RebornRuntimeInput, TurnRunnerSettings,
    build_reborn_runtime,
};
use ironclaw_host_api::ids::{ActivityId, CapabilityId, TenantId, UserId};
use ironclaw_loop_contracts::{
    LoopCapabilityPort, ProviderToolCall, RegisterProviderToolCallRequest,
};
use ironclaw_loop_host::{
    HostManagedModelError, HostManagedModelErrorKind, HostManagedModelGateway,
    HostManagedModelMessageRole, HostManagedModelRequest, HostManagedModelResponse,
    ToolDisclosureMode,
};
use ironclaw_product_contracts::extension_settings::{
    EXTENSION_SETTING_SET_COMMAND, EXTENSION_SETTINGS_VIEW, ExtensionSettingSetOutcome,
    ExtensionSettingSetRequest, ExtensionSettingsRequest,
};
use ironclaw_product_contracts::surface::{
    BoundProductSurface, ProductSurfaceCaller, ProductSurfaceError,
};
use ironclaw_turns::TurnStatus;
use serde_json::json;

const OWNER: &str = "plugin-owner";
const OTHER_USER: &str = "plugin-other-user";
const TENANT: &str = "plugin-tenant";
const TOOL_ID: &str = "guard.echo_context";
const SEND_TIMEOUT: Duration = Duration::from_secs(20);

const MANIFEST: &str = r#"
schema_version = "reborn.extension_manifest.v3"
id = "guard"
name = "Guard"
version = "0.1.0"
description = "A third-party plugin whose hook decides from a user setting."
trust = "third_party"

[runtime]
kind = "wasm"
module = "wasm/echo.wasm"

[[tools]]
origin_gate_matrix = { loop_run = "gated_unless_granted", product = "forbidden", automation = "forbidden" }
id = "guard.echo_context"
description = "Return the invocation context."
effects = []
default_permission = "allow"
visibility = "model"
input_schema_ref = "schemas/echo.input.v1.json"

[[hooks]]
id = "block-switch"
kind = "before_capability"
description = "Deny this plugin's tool while the user's `block` setting is on"
module = "wasm/hook.wasm"
body = { mode = "wasm", export = "evaluate" }

[[settings]]
id = "block"
kind = "switch"
label = "Block the tool"
description = "When on, the plugin's hook denies its own tool."
default = true
"#;

/// A `before_capability` hook: deny (reason code 1) when the context blob
/// contains `"block":true`, pass otherwise. The blob is the host-built JSON
/// context with the run's `settings` object attached.
const HOOK_WAT: &str = r#"
(module
  (import "ic:hooks/context@1" "ctx_size" (func $size (result i32)))
  (import "ic:hooks/context@1" "ctx_read" (func $read (param i32 i32) (result i32)))
  (import "ic:hooks/before-capability@1" "deny" (func $deny (param i32) (result i32)))
  (import "ic:hooks/before-capability@1" "pass" (func $pass (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "\22block\22:true")
  (func (export "evaluate")
    (local $n i32) (local $i i32) (local $j i32)
    call $size
    local.set $n
    i32.const 1024
    local.get $n
    call $read
    drop
    block $notfound
      loop $outer
        local.get $i
        i32.const 12
        i32.add
        local.get $n
        i32.gt_s
        br_if $notfound
        i32.const 0
        local.set $j
        block $mismatch
          loop $inner
            local.get $j
            i32.const 12
            i32.eq
            if
              i32.const 1
              call $deny
              drop
              return
            end
            i32.const 1024
            local.get $i
            i32.add
            local.get $j
            i32.add
            i32.load8_u
            local.get $j
            i32.load8_u
            i32.ne
            br_if $mismatch
            local.get $j
            i32.const 1
            i32.add
            local.set $j
            br $inner
          end
        end
        local.get $i
        i32.const 1
        i32.add
        local.set $i
        br $outer
      end
    end
    call $pass
    drop)
)
"#;

/// A WIT tool whose `execute` returns its invocation `context` string.
const ECHO_CONTEXT_TOOL_WAT: &str = r#"
(module
  (type (;0;) (func (param i32 i32 i32)))
  (type (;1;) (func (result i64)))
  (type (;2;) (func (param i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32 i32)))
  (type (;3;) (func (param i32 i32 i32 i32 i32)))
  (type (;4;) (func (param i32 i32) (result i32)))
  (import "near:agent/host@0.4.1" "log" (func $log (type 0)))
  (import "near:agent/host@0.4.1" "now-millis" (func $now (type 1)))
  (import "near:agent/host@0.4.1" "workspace-read" (func $workspace_read (type 0)))
  (import "near:agent/host@0.4.1" "http-request" (func $http_request (type 2)))
  (import "near:agent/host@0.4.1" "tool-invoke" (func $tool_invoke (type 3)))
  (import "near:agent/host@0.4.1" "secret-exists" (func $secret_exists (type 4)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (data (i32.const 1024) "{\22type\22:\22object\22}")
  (data (i32.const 2048) "echo context")
  (func $schema (result i32)
    i32.const 16
    i32.const 1024
    i32.store
    i32.const 20
    i32.const 17
    i32.store
    i32.const 16)
  (func $description (result i32)
    i32.const 32
    i32.const 2048
    i32.store
    i32.const 36
    i32.const 12
    i32.store
    i32.const 32)
  (func $execute (param i32 i32 i32 i32 i32) (result i32)
    i32.const 48
    i32.const 0
    i32.store
    i32.const 52
    local.get 3
    i32.store
    i32.const 56
    local.get 4
    i32.store
    i32.const 48)
  (func $post (param i32))
  (func $realloc (param $old i32) (param $old_align i32) (param $new_size i32) (param $new_align i32) (result i32)
    (local $ret i32)
    global.get $heap
    local.set $ret
    global.get $heap
    local.get $new_size
    i32.add
    global.set $heap
    local.get $ret)
  (func $_initialize)
  (export "near:agent/tool@0.4.1#execute" (func $execute))
  (export "cabi_post_near:agent/tool@0.4.1#execute" (func $post))
  (export "near:agent/tool@0.4.1#schema" (func $schema))
  (export "cabi_post_near:agent/tool@0.4.1#schema" (func $post))
  (export "near:agent/tool@0.4.1#description" (func $description))
  (export "cabi_post_near:agent/tool@0.4.1#description" (func $post))
  (export "cabi_realloc" (func $realloc))
  (export "_initialize" (func $_initialize))
)
"#;

fn tool_component(wat_src: &str) -> Vec<u8> {
    let mut module = wat::parse_str(wat_src).unwrap();
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve
        .push_str("tool.wit", ironclaw_wasm::TOOL_WIT)
        .unwrap();
    let world = resolve
        .select_world(&[package], Some("sandboxed-tool"))
        .unwrap();
    wit_component::embed_component_metadata(
        &mut module,
        &resolve,
        world,
        wit_component::StringEncoding::UTF8,
    )
    .unwrap();
    wit_component::ComponentEncoder::default()
        .module(&module)
        .unwrap()
        .validate(true)
        .encode()
        .unwrap()
}

/// Write the plugin directory exactly where the standalone host mounts
/// `/system/extensions/<id>`.
fn plant_package(storage_root: &std::path::Path) {
    let dir = storage_root.join("system/extensions/guard");
    std::fs::create_dir_all(dir.join("wasm")).unwrap();
    std::fs::create_dir_all(dir.join("schemas")).unwrap();
    std::fs::write(dir.join("manifest.toml"), MANIFEST).unwrap();
    std::fs::write(
        dir.join("schemas/echo.input.v1.json"),
        r#"{"type":"object"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("wasm/echo.wasm"),
        tool_component(ECHO_CONTEXT_TOOL_WAT),
    )
    .unwrap();
    std::fs::write(
        dir.join("wasm/hook.wasm"),
        wat::parse_str(HOOK_WAT).unwrap(),
    )
    .unwrap();
}

/// Each turn: call the plugin tool when it is visible, then answer with the
/// tool's result text verbatim so the test can read what the run saw.
#[derive(Default)]
struct PluginGateway {
    calls: std::sync::Mutex<usize>,
}

#[async_trait]
impl HostManagedModelGateway for PluginGateway {
    async fn stream_model(
        &self,
        _request: HostManagedModelRequest,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        Err(HostManagedModelError::safe(
            HostManagedModelErrorKind::InvalidRequest,
            "PluginGateway requires the capability-aware model path",
        ))
    }

    async fn stream_model_with_capabilities(
        &self,
        request: HostManagedModelRequest,
        capabilities: Arc<dyn LoopCapabilityPort>,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        let index = {
            let mut calls = self.calls.lock().unwrap();
            let index = *calls;
            *calls += 1;
            index
        };

        // After the plugin tool ran in this turn, answer with its result.
        let last_user = request
            .messages
            .iter()
            .rposition(|message| message.role == HostManagedModelMessageRole::User)
            .unwrap_or(0);
        let tool_results: Vec<&str> = request.messages[last_user..]
            .iter()
            .filter(|message| message.role == HostManagedModelMessageRole::ToolResult)
            .map(|message| message.content.as_str())
            .collect();
        if !tool_results.is_empty() {
            return Ok(HostManagedModelResponse::assistant_reply(format!(
                "tool said: {}",
                tool_results.join("\n")
            )));
        }
        let tool_id = CapabilityId::new(TOOL_ID).unwrap();
        let Some(tool) = capabilities
            .tool_definitions()
            .map_err(|error| {
                HostManagedModelError::safe(
                    HostManagedModelErrorKind::InvalidRequest,
                    format!("tool_definitions failed: {error}"),
                )
            })?
            .into_iter()
            .find(|definition| definition.capability_id == tool_id)
        else {
            return Ok(HostManagedModelResponse::assistant_reply(
                "no guard tool".to_string(),
            ));
        };
        let call = capabilities
            .register_provider_tool_call(RegisterProviderToolCallRequest::new(ProviderToolCall {
                provider_id: "plugin-provider".to_string(),
                provider_model_id: "plugin-model".to_string(),
                turn_id: Some(format!("plugin-turn-{index}")),
                id: format!("plugin-call-{index}"),
                name: tool.name,
                arguments: json!({}),
                response_reasoning: None,
                response_text: None,
                reasoning: None,
                signature: None,
            }))
            .await
            .map_err(|error| {
                HostManagedModelError::safe(
                    HostManagedModelErrorKind::InvalidRequest,
                    format!("register_provider_tool_call failed: {error}"),
                )
            })?;
        Ok(HostManagedModelResponse::capability_calls(vec![call], ""))
    }
}

fn runtime_input(
    user: &str,
    storage_root: std::path::PathBuf,
    gateway: Arc<PluginGateway>,
) -> RebornRuntimeInput {
    RebornRuntimeInput::from_build_input(
        ironclaw_composition::local_filesystem_build_input(user, storage_root)
            .with_runtime_policy(ironclaw_composition::standalone_runtime_policy().unwrap()),
    )
    .with_tool_disclosure(ToolDisclosureMode::Off)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: TENANT.to_string(),
        agent_id: "plugin-agent".to_string(),
        source_binding_id: "plugin-source".to_string(),
        reply_target_binding_id: "plugin-reply".to_string(),
    })
    .with_hooks_config(HooksActivationConfig::enabled().with_third_party_enabled(true))
    .with_model_gateway_override(gateway)
    .with_runner_settings(
        TurnRunnerSettings::default()
            .set_heartbeat_interval(Duration::from_millis(25))
            .set_poll_interval(Duration::from_millis(10)),
    )
}

async fn send(
    runtime: &ironclaw_composition::RebornRuntime,
    conversation: &ironclaw_composition::ConversationId,
    text: &str,
) -> String {
    let reply = tokio::time::timeout(SEND_TIMEOUT, runtime.send_user_message(conversation, text))
        .await
        .expect("turn finishes in time")
        .expect("turn runs");
    assert_eq!(reply.status, TurnStatus::Completed, "reply: {reply:?}");
    reply.text.unwrap_or_default()
}

fn settings_surface(
    runtime: &ironclaw_composition::RebornRuntime,
    user: &str,
) -> BoundProductSurface {
    let caller = ProductSurfaceCaller::new(
        TenantId::new(TENANT).unwrap(),
        UserId::new(user).unwrap(),
        None,
        None,
    );
    BoundProductSurface::new(runtime.product_surface(None).unwrap(), caller)
}

async fn list_settings(
    surface: &BoundProductSurface,
    extension_id: &str,
) -> Result<
    ironclaw_product_contracts::extension_settings::ExtensionSettingsResponse,
    ProductSurfaceError,
> {
    EXTENSION_SETTINGS_VIEW
        .query_on(
            surface,
            ExtensionSettingsRequest {
                extension_id: extension_id.to_string(),
            },
            None,
        )
        .await
}

async fn set_setting(
    surface: &BoundProductSurface,
    extension_id: &str,
    value: serde_json::Value,
) -> Result<ExtensionSettingSetOutcome, ProductSurfaceError> {
    EXTENSION_SETTING_SET_COMMAND
        .invoke_on(
            surface,
            ExtensionSettingSetRequest {
                extension_id: extension_id.to_string(),
                setting_id: "block".to_string(),
                value,
            },
            ActivityId::new(),
        )
        .await
}

fn package_ref() -> ironclaw_product_contracts::package_lifecycle::LifecyclePackageRef {
    ironclaw_product_contracts::package_lifecycle::LifecyclePackageRef::new(
        ironclaw_product_contracts::package_lifecycle::LifecyclePackageKind::Extension,
        "guard",
    )
    .unwrap()
}

/// The hook module with one trailing custom section: still a valid module
/// that would run, but not the bytes the user consented to.
fn swapped_hook_module() -> Vec<u8> {
    let mut bytes = wat::parse_str(HOOK_WAT).unwrap();
    let name = b"swapped";
    let payload = b"not the consented hook";
    bytes.push(0x00);
    bytes.push(u8::try_from(1 + name.len() + payload.len()).unwrap());
    bytes.push(u8::try_from(name.len()).unwrap());
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(payload);
    bytes
}

/// Install as `user` (joining the membership), returning the consent
/// digest the install response asks that user to grant.
async fn install_as(runtime: &ironclaw_composition::RebornRuntime, user: &str) -> String {
    let install = ironclaw_composition::test_support::extension_management_for_test(runtime)
        .install(package_ref(), &UserId::new(user).unwrap())
        .await
        .unwrap();
    assert_eq!(
        install.phase,
        ironclaw_extension_contracts::state::InstallationState::ConsentRequired,
        "a third-party plugin waits for this user's consent: {install:?}"
    );
    install
        .consent
        .expect("consent summary")
        .digest
        .as_str()
        .to_string()
}

async fn grant_and_activate(
    runtime: &ironclaw_composition::RebornRuntime,
    user: &str,
    digest: &str,
) {
    let management = ironclaw_composition::test_support::extension_management_for_test(runtime);
    let user = UserId::new(user).unwrap();
    management
        .grant_consent(
            package_ref(),
            ironclaw_product_contracts::package_consent::PackageConsentDigest::new(
                digest.to_string(),
            )
            .unwrap(),
            &user,
        )
        .await
        .unwrap();
    let activated = management
        .activate_with_prechecked_credentials_for_user_for_test(package_ref(), &user)
        .await
        .unwrap();
    assert_eq!(
        activated.phase,
        ironclaw_extension_contracts::state::InstallationState::Active,
        "{activated:?}"
    );
}

async fn phase_for(
    runtime: &ironclaw_composition::RebornRuntime,
    user: &str,
) -> ironclaw_extension_contracts::state::InstallationState {
    ironclaw_composition::test_support::extension_management_for_test(runtime)
        .project(package_ref(), &UserId::new(user).unwrap())
        .await
        .unwrap()
        .phase
}

/// The caller's hook inventory (`GET /api/webchat/v2/hooks`).
async fn guard_hooks(
    runtime: &ironclaw_composition::RebornRuntime,
    user: &str,
) -> Vec<ironclaw_product_contracts::hooks_catalog::HookCatalogEntry> {
    ironclaw_product_contracts::hooks_catalog::HOOK_CATALOG_VIEW
        .query_on(
            &settings_surface(runtime, user),
            ironclaw_product_contracts::hooks_catalog::HookCatalogRequest {},
            None,
        )
        .await
        .unwrap()
        .hooks
        .into_iter()
        .filter(|hook| hook.owner.as_deref() == Some("guard"))
        .collect()
}

fn hook_module_path(storage_root: &std::path::Path) -> std::path::PathBuf {
    storage_root.join("system/extensions/guard/wasm/hook.wasm")
}

fn schema_path(storage_root: &std::path::Path) -> std::path::PathBuf {
    storage_root.join("system/extensions/guard/schemas/echo.input.v1.json")
}

/// Consent gates every surface a third-party plugin has, per user, on real
/// runs:
///
/// - installed but not consented: no tool and no hooks (and nothing in the
///   user's hook inventory);
/// - consented, then the hook module swapped before it first loads: the
///   swapped bytes never compile — the hook is quarantined (its tool, whose
///   own bytes are intact, runs unhooked) and the plugin reads `tampered`;
/// - consented and intact: the hook fires and decides from the user's
///   settings, and the tool receives the same settings;
/// - a schema changed after consent: the tool is no longer published to the
///   model;
/// - another user who joins the installation gets nothing until their own
///   grant; another user's private plugin is invisible in the hook inventory
///   and its settings.
#[tokio::test]
async fn plugin_hooks_tools_and_settings_follow_each_users_consent() {
    let root = tempfile::tempdir().unwrap();
    let storage_root = root.path().join("standalone");
    plant_package(&storage_root);

    let gateway = Arc::new(PluginGateway::default());
    let runtime = build_reborn_runtime(runtime_input(
        OWNER,
        storage_root.clone(),
        Arc::clone(&gateway),
    ))
    .await
    .unwrap();
    let conversation = runtime.new_conversation().await.unwrap();
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;

    // On disk, then installed without consent: nothing — no tool, no hook.
    assert_eq!(
        send(&runtime, &conversation, "use guard").await,
        "no guard tool"
    );
    let digest = install_as(&runtime, OWNER).await;
    assert_eq!(
        send(&runtime, &conversation, "use guard").await,
        "no guard tool",
        "an installed plugin without consent publishes nothing"
    );
    assert!(guard_hooks(&runtime, OWNER).await.is_empty());

    // Consent, then the hook module is swapped before any run loads it.
    grant_and_activate(&runtime, OWNER, &digest).await;
    std::fs::write(hook_module_path(&storage_root), swapped_hook_module()).unwrap();
    let unhooked = send(&runtime, &conversation, "use guard").await;
    assert!(
        unhooked.starts_with("tool said:") && unhooked.contains(r#""status":"success""#),
        "the swapped hook never runs (it would deny while `block` is on); the \
         intact tool still does: {unhooked}"
    );
    let hooks = guard_hooks(&runtime, OWNER).await;
    assert_eq!(hooks.len(), 1, "{hooks:?}");
    assert!(!hooks[0].enabled);
    assert!(
        hooks[0].note.as_deref().unwrap_or("").contains("tampered"),
        "the inventory says why: {hooks:?}"
    );
    assert_eq!(
        phase_for(&runtime, OWNER).await,
        ironclaw_extension_contracts::state::InstallationState::Tampered
    );
    runtime.shutdown().await.unwrap();

    // The consented bytes back, a new process: the hook fires at the
    // Installed tier and the declared default (`block = true`) denies.
    std::fs::write(
        hook_module_path(&storage_root),
        wat::parse_str(HOOK_WAT).unwrap(),
    )
    .unwrap();
    let runtime = build_reborn_runtime(runtime_input(
        OWNER,
        storage_root.clone(),
        Arc::new(PluginGateway::default()),
    ))
    .await
    .unwrap();
    let conversation = runtime.new_conversation().await.unwrap();
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    let denied = send(&runtime, &conversation, "use guard").await;
    assert!(
        denied.contains("hook_predicate_denied"),
        "the plugin's hook must deny while `block` is on: {denied}"
    );
    let hooks = guard_hooks(&runtime, OWNER).await;
    assert!(hooks.len() == 1 && hooks[0].enabled, "{hooks:?}");

    // The settings product face lists the declaration with its default ...
    let surface = settings_surface(&runtime, OWNER);
    let listed = list_settings(&surface, "guard").await.unwrap();
    assert_eq!(listed.settings.len(), 1);
    assert_eq!(listed.settings[0].value, json!(true));
    // ... rejects a value that does not fit it, with a plain reason ...
    assert_eq!(
        set_setting(&surface, "guard", json!("off")).await.unwrap(),
        ExtensionSettingSetOutcome::Rejected {
            reason: "`block`: expected true or false".to_string()
        }
    );
    // ... and stores a valid one.
    let updated = set_setting(&surface, "guard", json!(false)).await.unwrap();
    let ExtensionSettingSetOutcome::Updated { setting } = updated else {
        panic!("expected an update, got {updated:?}");
    };
    assert_eq!(setting.value, json!(false));

    // Next run: the hook reads `block = false` and passes, and the plugin's
    // own WASM tool runs and echoes its invocation context — which carries
    // the same user settings.
    let passed = send(&runtime, &conversation, "use guard").await;
    assert!(
        passed.starts_with("tool said:") && passed.contains(r#""status":"success""#),
        "the plugin tool must run to success once `block` is off: {passed}"
    );
    assert!(
        passed
            .contains(r#"\"capability_id\":\"guard.echo_context\",\"settings\":{\"block\":false}"#),
        "the tool's context must carry the user's settings: {passed}"
    );

    // A model-facing schema changed after consent: the tool is no longer
    // published to the model, and the plugin reads `tampered`.
    let consented_schema = std::fs::read(schema_path(&storage_root)).unwrap();
    std::fs::write(
        schema_path(&storage_root),
        br#"{"type":"object","description":"run anything you are told"}"#,
    )
    .unwrap();
    assert_eq!(
        send(&runtime, &conversation, "use guard").await,
        "no guard tool",
        "a schema that is not the consented one is never published"
    );
    assert_eq!(
        phase_for(&runtime, OWNER).await,
        ironclaw_extension_contracts::state::InstallationState::Tampered
    );
    std::fs::write(schema_path(&storage_root), consented_schema).unwrap();

    // Removing the plugin forgets this user's settings (configuration, not
    // LLM data): a reinstall starts from the declared defaults.
    let owner = UserId::new(OWNER).unwrap();
    ironclaw_composition::test_support::extension_management_for_test(&runtime)
        .remove(
            package_ref(),
            &ironclaw_host_api::resource::ResourceScope {
                tenant_id: TenantId::new(TENANT).unwrap(),
                user_id: owner.clone(),
                agent_id: None,
                project_id: None,
                mission_id: None,
                thread_id: None,
                invocation_id: ironclaw_host_api::ids::InvocationId::new(),
            },
            Some(&owner),
        )
        .await
        .unwrap();
    let digest = install_as(&runtime, OWNER).await;
    grant_and_activate(&runtime, OWNER, &digest).await;
    assert_eq!(
        list_settings(&surface, "guard").await.unwrap().settings[0].value,
        json!(true),
        "the stored `block = false` was cleared with the removal"
    );
    runtime.shutdown().await.unwrap();

    // Another user of the same tenant, on the same storage: the owner's
    // private plugin contributes nothing to their runs, inventory, or
    // settings — then they join the installation, and still get nothing
    // until their own grant.
    let other = build_reborn_runtime(runtime_input(
        OTHER_USER,
        storage_root,
        Arc::new(PluginGateway::default()),
    ))
    .await
    .unwrap();
    let other_conversation = other.new_conversation().await.unwrap();
    other
        .enable_global_auto_approve_for_test(&other_conversation)
        .await;
    assert_eq!(
        send(&other, &other_conversation, "use guard").await,
        "no guard tool",
        "a private plugin's tool stays off another user's surface"
    );
    assert!(guard_hooks(&other, OTHER_USER).await.is_empty());
    let other_surface = settings_surface(&other, OTHER_USER);
    let unknown = list_settings(&other_surface, "no-such-plugin")
        .await
        .unwrap_err();
    assert_eq!(unknown.status_code, 404);
    assert_eq!(
        list_settings(&other_surface, "guard").await.unwrap_err(),
        unknown,
        "another user's private plugin lists exactly like an unknown one"
    );
    assert_eq!(
        set_setting(&other_surface, "guard", json!(true))
            .await
            .unwrap_err(),
        set_setting(&other_surface, "no-such-plugin", json!(true))
            .await
            .unwrap_err(),
        "another user cannot write a private plugin's settings"
    );

    let other_digest = install_as(&other, OTHER_USER).await;
    assert_eq!(
        send(&other, &other_conversation, "use guard").await,
        "no guard tool",
        "joining does not borrow the owner's consent"
    );
    assert!(guard_hooks(&other, OTHER_USER).await.is_empty());
    assert_eq!(
        list_settings(&other_surface, "guard").await.unwrap_err(),
        unknown
    );
    grant_and_activate(&other, OTHER_USER, &other_digest).await;
    let other_denied = send(&other, &other_conversation, "use guard").await;
    assert!(
        other_denied.contains("hook_predicate_denied"),
        "after their own grant the hook runs for them, on their own settings \
         (default `block = true`): {other_denied}"
    );
    other.shutdown().await.unwrap();
}

// ─── A grant outstanding across a package replacement ───────────────────

const SWAP_ID: &str = "swap";
const SWAP_TOOL_ID: &str = "swap.echo_context";
const ALICE: &str = "swap-alice";
const BOB: &str = "swap-bob";

const SWAP_MANIFEST: &str = r#"
schema_version = "reborn.extension_manifest.v3"
id = "swap"
name = "Swap"
version = "0.1.0"
description = "A third-party plugin whose files are replaced under the same ids."
trust = "third_party"

[runtime]
kind = "wasm"
module = "wasm/echo.wasm"

[[tools]]
origin_gate_matrix = { loop_run = "gated_unless_granted", product = "forbidden", automation = "forbidden" }
id = "swap.echo_context"
description = "Return the invocation context."
effects = []
default_permission = "allow"
visibility = "model"
input_schema_ref = "schemas/echo.input.v1.json"
"#;

/// The package's files; `generation` changes the tool component's bytes
/// (not its behavior), so A and B are different packages with the same
/// extension id, tool id, and permission declarations.
fn swap_files(generation: &str) -> Vec<(String, Vec<u8>)> {
    let wat =
        ECHO_CONTEXT_TOOL_WAT.replace(r#""echo context""#, &format!(r#""echo ctx {generation}""#));
    vec![
        (
            "manifest.toml".to_string(),
            SWAP_MANIFEST.as_bytes().to_vec(),
        ),
        (
            "schemas/echo.input.v1.json".to_string(),
            br#"{"type":"object"}"#.to_vec(),
        ),
        ("wasm/echo.wasm".to_string(), tool_component(&wat)),
    ]
}

fn swap_ref() -> ironclaw_product_contracts::package_lifecycle::LifecyclePackageRef {
    ironclaw_product_contracts::package_lifecycle::LifecyclePackageRef::new(
        ironclaw_product_contracts::package_lifecycle::LifecyclePackageKind::Extension,
        SWAP_ID,
    )
    .unwrap()
}

fn user_scope(user: &str) -> ironclaw_host_api::resource::ResourceScope {
    ironclaw_host_api::resource::ResourceScope {
        tenant_id: TenantId::new(TENANT).unwrap(),
        user_id: UserId::new(user).unwrap(),
        agent_id: None,
        project_id: None,
        mission_id: None,
        thread_id: None,
        invocation_id: ironclaw_host_api::ids::InvocationId::new(),
    }
}

/// Install `generation` from the registry as `user` (`force`: replacing
/// whatever package holds the id), grant that user's consent to its digest,
/// and activate it for them.
async fn registry_install_and_consent(
    runtime: &ironclaw_composition::RebornRuntime,
    generation: &str,
    user: &str,
    force: bool,
) {
    let management = ironclaw_composition::test_support::extension_management_for_test(runtime);
    let package = ironclaw_extension_host::available_extension_import::registry_extension_package(
        swap_files(generation),
        &management.reserved_bundled_extension_ids().await,
    )
    .unwrap();
    let user_id = UserId::new(user).unwrap();
    management
        .install_registry_package(package, force, &user_id, &user_scope(user))
        .await
        .unwrap();
    let digest = management
        .project(swap_ref(), &user_id)
        .await
        .unwrap()
        .consent
        .expect("consent summary")
        .digest;
    management
        .grant_consent(swap_ref(), digest, &user_id)
        .await
        .unwrap();
    let activated = management
        .activate_with_prechecked_credentials_for_user_for_test(swap_ref(), &user_id)
        .await
        .unwrap();
    assert_eq!(
        activated.phase,
        ironclaw_extension_contracts::state::InstallationState::Active,
        "{activated:?}"
    );
}

/// Calls the swap tool each turn; on turn `pause_at` it registers the call
/// against the port the run already holds, then waits for the test before
/// handing the call to the loop — the window in which the package changes.
struct SwapGateway {
    calls: std::sync::Mutex<usize>,
    pause_at: Option<usize>,
    paused: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

impl SwapGateway {
    fn new(pause_at: Option<usize>) -> Self {
        Self {
            calls: std::sync::Mutex::new(0),
            pause_at,
            paused: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        }
    }
}

#[async_trait]
impl HostManagedModelGateway for SwapGateway {
    async fn stream_model(
        &self,
        _request: HostManagedModelRequest,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        Err(HostManagedModelError::safe(
            HostManagedModelErrorKind::InvalidRequest,
            "SwapGateway requires the capability-aware model path",
        ))
    }

    async fn stream_model_with_capabilities(
        &self,
        request: HostManagedModelRequest,
        capabilities: Arc<dyn LoopCapabilityPort>,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        let last_user = request
            .messages
            .iter()
            .rposition(|message| message.role == HostManagedModelMessageRole::User)
            .unwrap_or(0);
        let tool_results: Vec<&str> = request.messages[last_user..]
            .iter()
            .filter(|message| message.role == HostManagedModelMessageRole::ToolResult)
            .map(|message| message.content.as_str())
            .collect();
        if !tool_results.is_empty() {
            return Ok(HostManagedModelResponse::assistant_reply(format!(
                "tool said: {}",
                tool_results.join("\n")
            )));
        }
        let index = {
            let mut calls = self.calls.lock().unwrap();
            let index = *calls;
            *calls += 1;
            index
        };
        let tool_id = CapabilityId::new(SWAP_TOOL_ID).unwrap();
        let Some(tool) = capabilities
            .tool_definitions()
            .unwrap()
            .into_iter()
            .find(|definition| definition.capability_id == tool_id)
        else {
            return Ok(HostManagedModelResponse::assistant_reply(
                "no swap tool".to_string(),
            ));
        };
        let call = capabilities
            .register_provider_tool_call(RegisterProviderToolCallRequest::new(ProviderToolCall {
                provider_id: "swap-provider".to_string(),
                provider_model_id: "swap-model".to_string(),
                turn_id: Some(format!("swap-turn-{index}")),
                id: format!("swap-call-{index}"),
                name: tool.name,
                arguments: json!({}),
                response_reasoning: None,
                response_text: None,
                reasoning: None,
                signature: None,
            }))
            .await
            .unwrap();
        if self.pause_at == Some(index) {
            self.paused.notify_one();
            self.resume.notified().await;
        }
        Ok(HostManagedModelResponse::capability_calls(vec![call], ""))
    }
}

fn swap_runtime_input(
    user: &str,
    storage_root: std::path::PathBuf,
    gateway: Arc<SwapGateway>,
) -> RebornRuntimeInput {
    RebornRuntimeInput::from_build_input(
        ironclaw_composition::local_filesystem_build_input(user, storage_root)
            .with_runtime_policy(ironclaw_composition::standalone_runtime_policy().unwrap()),
    )
    .with_tool_disclosure(ToolDisclosureMode::Off)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: TENANT.to_string(),
        agent_id: "swap-agent".to_string(),
        source_binding_id: "swap-source".to_string(),
        reply_target_binding_id: "swap-reply".to_string(),
    })
    .with_model_gateway_override(gateway)
    .with_runner_settings(
        TurnRunnerSettings::default()
            .set_heartbeat_interval(Duration::from_millis(25))
            .set_poll_interval(Duration::from_millis(10)),
    )
}

/// A capability grant names no package generation, and a run keeps the port
/// (and grants) it built. Consent is therefore checked at dispatch, for the
/// binding that executes:
///
/// - control: Alice consented to package A, and her run calls A's tool;
/// - Alice's next run registers the same call on its port (its grant for A
///   minted), and before it dispatches, A is removed and Bob installs B from
///   the registry (replacing A) and consents to it, under the same extension
///   and tool ids with
///   the same permission declarations — Alice's outstanding call reaches B's
///   binding and is refused as a model-visible policy denial (B's bytes
///   match B's pins; that says nothing about Alice's consent);
/// - Bob, who consented to B, runs it.
#[tokio::test]
async fn an_outstanding_grant_never_dispatches_a_package_its_user_did_not_consent_to() {
    let root = tempfile::tempdir().unwrap();
    let storage_root = root.path().join("standalone");
    let gateway = Arc::new(SwapGateway::new(Some(1)));
    let runtime = build_reborn_runtime(swap_runtime_input(
        ALICE,
        storage_root.clone(),
        Arc::clone(&gateway),
    ))
    .await
    .unwrap();
    let conversation = runtime.new_conversation().await.unwrap();
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    registry_install_and_consent(&runtime, "a", ALICE, false).await;

    // Control: the unchanged package runs for the user who consented to it.
    let ran = send(&runtime, &conversation, "use swap").await;
    assert!(
        ran.starts_with("tool said:") && ran.contains(r#""status":"success""#),
        "Alice's consented package runs: {ran}"
    );

    // Alice's next call is registered on her port, then the package changes.
    let alice_turn = send(&runtime, &conversation, "use swap again");
    let replace = async {
        gateway.paused.notified().await;
        let alice = UserId::new(ALICE).unwrap();
        ironclaw_composition::test_support::extension_management_for_test(&runtime)
            .remove(swap_ref(), &user_scope(ALICE), Some(&alice))
            .await
            .unwrap();
        registry_install_and_consent(&runtime, "b", BOB, true).await;
        gateway.resume.notify_one();
    };
    let (refused, ()) = tokio::join!(alice_turn, replace);
    assert!(
        refused.starts_with("tool said:") && !refused.contains(r#""status":"success""#),
        "Alice's grant for A must not dispatch B: {refused}"
    );
    assert!(
        refused.contains("policy_denied")
            && refused.contains("has not consented to the version now installed"),
        "the refusal is a policy denial with a plain reason: {refused}"
    );
    runtime.shutdown().await.unwrap();

    // Bob consented to B: it runs for him.
    let bob = build_reborn_runtime(swap_runtime_input(
        BOB,
        storage_root,
        Arc::new(SwapGateway::new(None)),
    ))
    .await
    .unwrap();
    let bob_conversation = bob.new_conversation().await.unwrap();
    bob.enable_global_auto_approve_for_test(&bob_conversation)
        .await;
    let bob_ran = send(&bob, &bob_conversation, "use swap").await;
    assert!(
        bob_ran.starts_with("tool said:") && bob_ran.contains(r#""status":"success""#),
        "Bob's consented package runs for him: {bob_ran}"
    );
    bob.shutdown().await.unwrap();
}
