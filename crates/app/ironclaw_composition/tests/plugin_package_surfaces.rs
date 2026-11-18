//! End-to-end proof, through `build_reborn_runtime`, that a third-party
//! package's `[[hooks]]` and `[[settings]]` are executed by the host on the
//! package's behalf:
//!
//! - a package on disk but not activated contributes nothing;
//! - once activated, its WASM `before_capability` hook fires on a real run at
//!   the `Installed` tier and decides from the user's settings;
//! - a setting changed through the product surface reaches the hook on the
//!   next run and flips its decision;
//! - after deactivation the package's hooks and tool are gone.
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
use ironclaw_extension_registry::{
    ExtensionInstallation, ExtensionInstallationId, ExtensionManifest, ExtensionManifestRecord,
    ExtensionManifestRef, ExtensionPackage, HostApiContractRegistry, InstallationOwner,
    ManifestSource,
};
use ironclaw_host_api::ids::{ActivityId, CapabilityId, TenantId, UserId};
use ironclaw_host_api::path::VirtualPath;
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
use ironclaw_product_contracts::surface::{BoundProductSurface, ProductSurfaceCaller};
use ironclaw_turns::TurnStatus;
use serde_json::json;

const OWNER: &str = "plugin-owner";
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

fn package() -> (ExtensionPackage, ExtensionManifestRecord) {
    let record = ExtensionManifestRecord::from_toml(
        MANIFEST,
        ManifestSource::InstalledLocal,
        &ironclaw_host_api::host_port::default_host_port_catalog().unwrap(),
        None,
        &HostApiContractRegistry::new(),
        Some(VirtualPath::new("/system/extensions/guard").unwrap()),
    )
    .unwrap();
    let manifest = ExtensionManifest::try_from(record.manifest().clone()).unwrap();
    let package = ExtensionPackage::from_manifest(
        manifest,
        VirtualPath::new("/system/extensions/guard").unwrap(),
    )
    .unwrap();
    (package, record)
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

#[tokio::test]
async fn activated_plugin_hook_fires_decides_from_settings_and_is_gone_after_deactivation() {
    let root = tempfile::tempdir().unwrap();
    let storage_root = root.path().join("standalone");
    plant_package(&storage_root);

    let input = RebornRuntimeInput::from_build_input(
        ironclaw_composition::local_filesystem_build_input(OWNER, storage_root)
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
    .with_model_gateway_override(Arc::new(PluginGateway::default()))
    .with_runner_settings(
        TurnRunnerSettings::default()
            .set_heartbeat_interval(Duration::from_millis(25))
            .set_poll_interval(Duration::from_millis(10)),
    );
    let runtime = build_reborn_runtime(input).await.unwrap();
    let conversation = runtime.new_conversation().await.unwrap();
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    let send = |text: &'static str| {
        let runtime = &runtime;
        let conversation = &conversation;
        async move {
            let reply =
                tokio::time::timeout(SEND_TIMEOUT, runtime.send_user_message(conversation, text))
                    .await
                    .expect("turn finishes in time")
                    .expect("turn runs");
            assert_eq!(reply.status, TurnStatus::Completed, "reply: {reply:?}");
            reply.text.unwrap_or_default()
        }
    };

    // On disk but not activated: nothing — no tool, no hook.
    assert_eq!(send("use guard").await, "no guard tool");

    // Activate: the package's hook now fires on its own tool at the
    // Installed tier, and the declared default (`block = true`) denies.
    let (package, record) = package();
    // The user's installation record (what the install lifecycle writes) so
    // the plugin's tool joins this user's owner-filtered surface.
    runtime
        .extension_installation_store_for_test()
        .expect("installation store wired")
        .upsert_manifest_and_installation(
            record.clone(),
            ExtensionInstallation::new(
                ExtensionInstallationId::new("guard-install").unwrap(),
                package.id.clone(),
                ExtensionManifestRef::new(package.id.clone(), None),
                Vec::new(),
                chrono::Utc::now(),
                InstallationOwner::user(UserId::new(OWNER).unwrap()),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    runtime
        .publish_bundled_extension_for_test(&package, Some(record.resolved()))
        .await
        .expect("extension management wired")
        .expect("plugin activates");
    let denied = send("use guard").await;
    assert!(
        denied.contains("hook_predicate_denied"),
        "the plugin's hook must deny while `block` is on: {denied}"
    );

    // The settings product face lists the declaration with its default ...
    let caller = ProductSurfaceCaller::new(
        TenantId::new(TENANT).unwrap(),
        UserId::new(OWNER).unwrap(),
        None,
        None,
    );
    let surface = BoundProductSurface::new(runtime.product_surface(None).unwrap(), caller);
    let listed = EXTENSION_SETTINGS_VIEW
        .query_on(
            &surface,
            ExtensionSettingsRequest {
                extension_id: "guard".to_string(),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(listed.settings.len(), 1);
    assert_eq!(listed.settings[0].value, json!(true));
    // ... rejects a value that does not fit it, with a plain reason ...
    let rejected = EXTENSION_SETTING_SET_COMMAND
        .invoke_on(
            &surface,
            ExtensionSettingSetRequest {
                extension_id: "guard".to_string(),
                setting_id: "block".to_string(),
                value: json!("off"),
            },
            ActivityId::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        rejected,
        ExtensionSettingSetOutcome::Rejected {
            reason: "`block`: expected true or false".to_string()
        }
    );
    // ... and stores a valid one.
    let updated = EXTENSION_SETTING_SET_COMMAND
        .invoke_on(
            &surface,
            ExtensionSettingSetRequest {
                extension_id: "guard".to_string(),
                setting_id: "block".to_string(),
                value: json!(false),
            },
            ActivityId::new(),
        )
        .await
        .unwrap();
    let ExtensionSettingSetOutcome::Updated { setting } = updated else {
        panic!("expected an update, got {updated:?}");
    };
    assert_eq!(setting.value, json!(false));

    // Next run: the hook reads `block = false` from the user's settings and
    // passes, so the call proceeds past the hook into the capability
    // pipeline. (Tool-side delivery of the same values through the WASM
    // invocation context is pinned at the host-runtime tier:
    // `host_runtime_services_wasm_tool_context_carries_the_users_effective_settings`.
    // Kernel authorization of a locally installed third-party tool is the
    // install/consent lifecycle's contract, not this one's.)
    let passed = send("use guard").await;
    assert!(
        passed.starts_with("tool said:"),
        "the plugin tool call must still be issued: {passed}"
    );
    assert!(
        !passed.contains("hook_predicate_denied"),
        "the hook must pass once `block` is off: {passed}"
    );

    // Deactivate: the plugin's surfaces disappear from the next run.
    runtime
        .unpublish_bundled_extension_for_test(&package)
        .await
        .expect("plugin deactivates");
    assert_eq!(send("use guard").await, "no guard tool");

    runtime.shutdown().await.unwrap();
}
