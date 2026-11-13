//! Built-in first-party package, surface, and dispatch-boundary contracts.

#![deny(clippy::let_underscore_must_use)]
use std::sync::Arc;

use ironclaw_host_api::capability_surface::CapabilitySurfacePolicy;
use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_api::runtime_policy::ProcessBackendKind;
use ironclaw_host_api::{
    capability::{
        CapabilityDescriptor, EffectKind, OriginGatePolicy, PermissionMode,
        UNGATED_LOOP_RUN_CAPABILITIES,
    },
    resource::ResourceEstimate,
    runtime::RuntimeKind,
};
use ironclaw_host_runtime::{
    APPLY_PATCH_CAPABILITY_ID, ECHO_CAPABILITY_ID, GLOB_CAPABILITY_ID, GREP_CAPABILITY_ID,
    HTTP_CAPABILITY_ID, HTTP_SAVE_CAPABILITY_ID, HostRuntime, JSON_CAPABILITY_ID,
    LIST_DIR_CAPABILITY_ID, MEMORY_READ_CAPABILITY_ID, MEMORY_SEARCH_CAPABILITY_ID,
    MEMORY_TREE_CAPABILITY_ID, MEMORY_WRITE_CAPABILITY_ID, OUTBOUND_DELIVER_CAPABILITY_ID,
    READ_FILE_CAPABILITY_ID, RuntimeCapabilityOutcome, SHELL_CAPABILITY_ID,
    SKILL_AUTO_ACTIVATE_SET_CAPABILITY_ID, SKILL_INSTALL_CAPABILITY_ID, SKILL_REMOVE_CAPABILITY_ID,
    SKILL_UPDATE_CAPABILITY_ID, SPAWN_SUBAGENT_CAPABILITY_ID, SurfaceKind,
    TRACE_COMMONS_ACCOUNT_LOGIN_LINK_CAPABILITY_ID, TRACE_COMMONS_ONBOARD_CAPABILITY_ID,
    TRACE_COMMONS_PROFILE_SET_CAPABILITY_ID, TRACE_COMMONS_PROFILE_TOKEN_CAPABILITY_ID,
    TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_LIST_CAPABILITY_ID, TRIGGER_PAUSE_CAPABILITY_ID,
    TRIGGER_REMOVE_CAPABILITY_ID, TRIGGER_RESUME_CAPABILITY_ID, TRIGGER_RUN_CAPABILITY_ID,
    VisibleCapabilityAccess, VisibleCapabilityRequest, WRITE_FILE_CAPABILITY_ID,
    builtin_first_party_handlers, builtin_first_party_handlers_for_process_backend,
    builtin_first_party_package, builtin_first_party_package_for_process_backend,
    native_memory_first_party_package,
};
use ironclaw_triggers::InMemoryTriggerRepository;
use serde_json::{Value, json};

mod first_party_builtin_tools_support;

use first_party_builtin_tools_support::{context::*, fixtures::*, runtimes::*, triggers::*};

#[tokio::test]
async fn builtin_first_party_package_declares_expected_capabilities() {
    let package = builtin_first_party_package().unwrap();
    assert_eq!(package.id, provider_id());
    assert_eq!(package.manifest.runtime_kind(), RuntimeKind::FirstParty);

    let ids = package
        .capabilities
        .iter()
        .map(|descriptor| descriptor.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, all_builtin_capability_ids());
    for descriptor in &package.capabilities {
        let expected_permission = match descriptor.id.as_str() {
            HTTP_CAPABILITY_ID
            | HTTP_SAVE_CAPABILITY_ID
            | SHELL_CAPABILITY_ID
            | SPAWN_SUBAGENT_CAPABILITY_ID
            | SKILL_INSTALL_CAPABILITY_ID
            | SKILL_UPDATE_CAPABILITY_ID
            | SKILL_AUTO_ACTIVATE_SET_CAPABILITY_ID
            | SKILL_REMOVE_CAPABILITY_ID
            | TRIGGER_CREATE_CAPABILITY_ID
            | TRIGGER_PAUSE_CAPABILITY_ID
            | TRIGGER_REMOVE_CAPABILITY_ID
            | TRIGGER_RESUME_CAPABILITY_ID
            | TRIGGER_RUN_CAPABILITY_ID
            | TRACE_COMMONS_ONBOARD_CAPABILITY_ID
            | TRACE_COMMONS_PROFILE_SET_CAPABILITY_ID
            | TRACE_COMMONS_PROFILE_TOKEN_CAPABILITY_ID
            | TRACE_COMMONS_ACCOUNT_LOGIN_LINK_CAPABILITY_ID => PermissionMode::Ask,
            _ => PermissionMode::Allow,
        };
        assert_eq!(descriptor.default_permission, expected_permission);
    }

    for descriptor in package
        .capabilities
        .iter()
        .filter(|descriptor| is_coding_capability_id(descriptor.id.as_str()))
    {
        assert_coding_manifest_contract(descriptor);
    }
    let json = package
        .capabilities
        .iter()
        .find(|descriptor| descriptor.id.as_str() == JSON_CAPABILITY_ID)
        .expect("JSON manifest");
    assert_eq!(
        json.effects,
        vec![EffectKind::DispatchCapability, EffectKind::ReadFilesystem]
    );
    let skill_install = package
        .capabilities
        .iter()
        .find(|descriptor| descriptor.id.as_str() == SKILL_INSTALL_CAPABILITY_ID)
        .expect("skill_install manifest");
    assert_eq!(
        skill_install.effects,
        vec![
            EffectKind::ReadFilesystem,
            EffectKind::WriteFilesystem,
            EffectKind::DeleteFilesystem,
            EffectKind::Network
        ]
    );
    let http = package
        .capabilities
        .iter()
        .find(|descriptor| descriptor.id.as_str() == HTTP_CAPABILITY_ID)
        .expect("http manifest");
    assert_eq!(
        http.effects,
        vec![EffectKind::DispatchCapability, EffectKind::Network]
    );
    assert!(
        http.description
            .contains("Prefer GitHub extension capabilities"),
        "builtin.http should steer GitHub repository API tasks toward the GitHub extension"
    );
    assert!(
        http.description.contains("structured HTTP APIs")
            && http.description.contains("web_search")
            && http.description.contains("does not render JavaScript"),
        "builtin.http should reserve raw HTTP for APIs and downloads while steering public web \
         content retrieval toward web search"
    );
    let http_save = package
        .capabilities
        .iter()
        .find(|descriptor| descriptor.id.as_str() == HTTP_SAVE_CAPABILITY_ID)
        .expect("http save manifest");
    assert_eq!(
        http_save.effects,
        vec![
            EffectKind::DispatchCapability,
            EffectKind::Network,
            EffectKind::WriteFilesystem
        ]
    );
    assert!(
        http_save
            .description
            .contains("Prefer GitHub extension capabilities"),
        "builtin.http.save should steer GitHub repository API tasks toward the GitHub extension"
    );
    assert!(
        http_save.description.contains("structured HTTP APIs")
            && http_save.description.contains("web_search")
            && http_save.description.contains("does not render JavaScript"),
        "builtin.http.save should reserve raw HTTP for APIs and downloads while steering public \
         web content retrieval toward web search"
    );

    // Memory tools now live in the always-on `ironclaw.memory` package,
    // not the generic builtin package; assert their effects there.
    let native_package = native_memory_first_party_package().unwrap();
    let memory_write = native_package
        .capabilities
        .iter()
        .find(|descriptor| descriptor.id.as_str() == MEMORY_WRITE_CAPABILITY_ID)
        .expect("memory write manifest");
    assert_eq!(
        memory_write.effects,
        vec![
            EffectKind::DispatchCapability,
            EffectKind::ReadFilesystem,
            EffectKind::WriteFilesystem
        ]
    );
    for capability_id in [
        MEMORY_SEARCH_CAPABILITY_ID,
        MEMORY_READ_CAPABILITY_ID,
        MEMORY_TREE_CAPABILITY_ID,
    ] {
        let descriptor = native_package
            .capabilities
            .iter()
            .find(|descriptor| descriptor.id.as_str() == capability_id)
            .expect("memory read-like manifest");
        assert_eq!(
            descriptor.effects,
            vec![EffectKind::DispatchCapability, EffectKind::ReadFilesystem]
        );
    }

    let handlers =
        builtin_first_party_handlers(Arc::new(InMemoryTriggerRepository::default())).unwrap();
    for id in all_builtin_capability_ids() {
        assert!(handlers.contains_handler(&capability_id(id)));
    }
}

/// §5.3 S3 (behavior-neutral): every builtin tool-package descriptor carries a
/// declared `origin_gate_matrix`. `LoopRun` mirrors today's effect gate exactly
/// (Ungated iff the id is in the reviewed `UNGATED_LOOP_RUN_CAPABILITIES`
/// allowlist, else GatedUnlessGranted); `Product`/`Automation` are deny-by-
/// default until a reviewed ingress slice declares a producer. This is the
/// load-bearing "allowlist <=> loop_run == Ungated" invariant, checked over the
/// production descriptors the tool package actually emits.
#[tokio::test]
async fn builtin_first_party_package_declares_behavior_neutral_origin_gate_matrix() {
    let package = builtin_first_party_package().unwrap();
    for descriptor in &package.capabilities {
        let matrix = descriptor
            .origin_gate_matrix
            .as_ref()
            .unwrap_or_else(|| panic!("{} must declare an origin_gate_matrix", descriptor.id));
        assert_ne!(
            matrix.loop_run,
            OriginGatePolicy::ConsentSufficient,
            "{}: ConsentSufficient is Product-only; not valid on loop_run",
            descriptor.id
        );
        assert_ne!(
            matrix.automation,
            OriginGatePolicy::ConsentSufficient,
            "{}: ConsentSufficient is Product-only; not valid on automation",
            descriptor.id
        );
        let expected_product =
            if product_surface_builtin_capabilities().contains(&descriptor.id.as_str()) {
                OriginGatePolicy::ConsentSufficient
            } else {
                OriginGatePolicy::Forbidden
            };
        assert_eq!(
            matrix.product, expected_product,
            "{} product origin policy mismatch",
            descriptor.id
        );
        assert_eq!(
            matrix.automation,
            OriginGatePolicy::Forbidden,
            "{} automation must be deny-by-default",
            descriptor.id
        );
        let expected_loop_run = if UNGATED_LOOP_RUN_CAPABILITIES.contains(&descriptor.id.as_str()) {
            OriginGatePolicy::Ungated
        } else {
            OriginGatePolicy::GatedUnlessGranted
        };
        assert_eq!(
            matrix.loop_run, expected_loop_run,
            "{} loop_run must match its allowlist membership",
            descriptor.id
        );
    }

    // Concrete spot checks so a reader sees the intended posture, independent of
    // the allowlist derivation: read-only/dispatch-only caps are Ungated, any
    // write/network/process cap is GatedUnlessGranted.
    let loop_run = |id: &str| {
        package
            .capabilities
            .iter()
            .find(|descriptor| descriptor.id.as_str() == id)
            .and_then(|descriptor| descriptor.origin_gate_matrix.as_ref())
            .map(|matrix| matrix.loop_run)
            .unwrap_or_else(|| panic!("{id} descriptor with matrix"))
    };
    for ungated in [
        ECHO_CAPABILITY_ID,
        JSON_CAPABILITY_ID,
        READ_FILE_CAPABILITY_ID,
        TRIGGER_LIST_CAPABILITY_ID,
    ] {
        assert_eq!(loop_run(ungated), OriginGatePolicy::Ungated, "{ungated}");
    }
    for gated in [
        WRITE_FILE_CAPABILITY_ID,
        HTTP_CAPABILITY_ID,
        SKILL_INSTALL_CAPABILITY_ID,
        TRIGGER_CREATE_CAPABILITY_ID,
        TRIGGER_RUN_CAPABILITY_ID,
        OUTBOUND_DELIVER_CAPABILITY_ID,
    ] {
        assert_eq!(
            loop_run(gated),
            OriginGatePolicy::GatedUnlessGranted,
            "{gated}"
        );
    }
}

/// §5.3 S3 for the always-on `ironclaw.memory` package: every memory tool
/// descriptor carries a declared `origin_gate_matrix` preserving the gating the
/// tools had as `builtin.memory_*` members of the builtin package (read-like
/// tools Ungated for `LoopRun` via the reviewed allowlist, `memory_write`
/// GatedUnlessGranted, `Product`/`Automation` deny-by-default). A missing
/// matrix is not "no gate" — the S4 authorize fold fails closed to Forbidden
/// for every origin-stamped invocation, denying all model memory dispatch.
#[tokio::test]
async fn native_memory_package_declares_behavior_neutral_origin_gate_matrix() {
    let package = native_memory_first_party_package().unwrap();
    for descriptor in &package.capabilities {
        let matrix = descriptor
            .origin_gate_matrix
            .as_ref()
            .unwrap_or_else(|| panic!("{} must declare an origin_gate_matrix", descriptor.id));
        assert_eq!(
            matrix.product,
            OriginGatePolicy::Forbidden,
            "{} product must be deny-by-default",
            descriptor.id
        );
        assert_eq!(
            matrix.automation,
            OriginGatePolicy::Forbidden,
            "{} automation must be deny-by-default",
            descriptor.id
        );
        let expected_loop_run = if UNGATED_LOOP_RUN_CAPABILITIES.contains(&descriptor.id.as_str()) {
            OriginGatePolicy::Ungated
        } else {
            OriginGatePolicy::GatedUnlessGranted
        };
        assert_eq!(
            matrix.loop_run, expected_loop_run,
            "{} loop_run must match its allowlist membership",
            descriptor.id
        );
    }

    let loop_run = |id: &str| {
        package
            .capabilities
            .iter()
            .find(|descriptor| descriptor.id.as_str() == id)
            .and_then(|descriptor| descriptor.origin_gate_matrix.as_ref())
            .map(|matrix| matrix.loop_run)
            .unwrap_or_else(|| panic!("{id} descriptor with matrix"))
    };
    for ungated in [
        MEMORY_READ_CAPABILITY_ID,
        MEMORY_SEARCH_CAPABILITY_ID,
        MEMORY_TREE_CAPABILITY_ID,
    ] {
        assert_eq!(loop_run(ungated), OriginGatePolicy::Ungated, "{ungated}");
    }
    assert_eq!(
        loop_run(MEMORY_WRITE_CAPABILITY_ID),
        OriginGatePolicy::GatedUnlessGranted,
        "{MEMORY_WRITE_CAPABILITY_ID}"
    );
}

fn product_surface_builtin_capabilities() -> &'static [&'static str] {
    &[
        SKILL_INSTALL_CAPABILITY_ID,
        "builtin.skill_update",
        "builtin.skill_auto_activate_set",
        SKILL_REMOVE_CAPABILITY_ID,
    ]
}

#[tokio::test]
async fn builtin_first_party_processless_package_and_handlers_omit_process_port_backed_shell() {
    let package =
        builtin_first_party_package_for_process_backend(ProcessBackendKind::None).unwrap();
    let ids = package
        .capabilities
        .iter()
        .map(|descriptor| descriptor.id.as_str())
        .collect::<Vec<_>>();
    assert!(!ids.contains(&SHELL_CAPABILITY_ID));
    assert!(ids.contains(&SPAWN_SUBAGENT_CAPABILITY_ID));
    assert!(ids.contains(&ECHO_CAPABILITY_ID));
    assert!(
        !package
            .manifest
            .capabilities
            .iter()
            .any(|capability| capability.id.as_str() == SHELL_CAPABILITY_ID)
    );

    let handlers = builtin_first_party_handlers_for_process_backend(
        Arc::new(InMemoryTriggerRepository::default()),
        ProcessBackendKind::None,
    )
    .unwrap();
    assert!(!handlers.contains_handler(&capability_id(SHELL_CAPABILITY_ID)));
    assert!(handlers.contains_handler(&capability_id(SPAWN_SUBAGENT_CAPABILITY_ID)));
    assert!(handlers.contains_handler(&capability_id(ECHO_CAPABILITY_ID)));
}

#[tokio::test]
async fn builtin_first_party_process_backend_package_and_handlers_keep_shell() {
    let package =
        builtin_first_party_package_for_process_backend(ProcessBackendKind::UserSandbox).unwrap();
    assert!(
        package
            .capabilities
            .iter()
            .any(|descriptor| descriptor.id.as_str() == SHELL_CAPABILITY_ID)
    );
    let shell = package
        .capabilities
        .iter()
        .find(|descriptor| descriptor.id.as_str() == SHELL_CAPABILITY_ID)
        .expect("user-sandbox shell descriptor");
    assert!(shell.description.contains("read-only system filesystem"));
    assert!(shell.description.contains("/workspace/.venv"));
    assert!(shell.description.contains("/workspace/.venv/bin/python"));
    for effect in [
        EffectKind::ReadFilesystem,
        EffectKind::WriteFilesystem,
        EffectKind::Network,
    ] {
        assert!(!shell.effects.contains(&effect));
    }
    let manifest_shell = package
        .manifest
        .capabilities
        .iter()
        .find(|capability| capability.id.as_str() == SHELL_CAPABILITY_ID)
        .expect("user-sandbox shell manifest");
    assert_eq!(manifest_shell.description, shell.description);
    for effect in [
        EffectKind::ReadFilesystem,
        EffectKind::WriteFilesystem,
        EffectKind::Network,
    ] {
        assert!(!manifest_shell.effects.contains(&effect));
    }

    let handlers = builtin_first_party_handlers_for_process_backend(
        Arc::new(InMemoryTriggerRepository::default()),
        ProcessBackendKind::UserSandbox,
    )
    .unwrap();
    assert!(handlers.contains_handler(&capability_id(SHELL_CAPABILITY_ID)));

    let host_package =
        builtin_first_party_package_for_process_backend(ProcessBackendKind::LocalHost).unwrap();
    let host_shell = host_package
        .capabilities
        .iter()
        .find(|descriptor| descriptor.id.as_str() == SHELL_CAPABILITY_ID)
        .expect("local-host shell descriptor");
    assert!(!host_shell.description.contains("/workspace/.venv"));
}

/// The shell description is the only place the model is told to reduce data
/// *inside* the pipeline. Without it the model reads whole files and datasets
/// into the conversation to count or sort them, which burns context on raw
/// content that the shell could have collapsed to a one-line conclusion. The
/// steering must reach the model on every process backend, so it lives in the
/// base description rather than a backend-conditional suffix like the
/// user-sandbox guidance above.
#[tokio::test]
async fn builtin_shell_description_steers_the_model_to_filter_in_the_pipeline() {
    const STEERING: &str = "Prefer computing the answer in the pipeline";

    for backend in [
        ProcessBackendKind::LocalHost,
        ProcessBackendKind::UserSandbox,
        ProcessBackendKind::Docker,
    ] {
        let package = builtin_first_party_package_for_process_backend(backend)
            .expect("built-in first-party package");
        let descriptor = package
            .capabilities
            .iter()
            .find(|descriptor| descriptor.id.as_str() == SHELL_CAPABILITY_ID)
            .expect("shell descriptor");
        assert!(
            descriptor.description.contains(STEERING),
            "{backend:?} shell descriptor lost the pipeline-filtering steering: {}",
            descriptor.description
        );
        assert!(
            descriptor.description.contains("conclusions, not content"),
            "{backend:?} shell descriptor lost the conclusions-not-content rule: {}",
            descriptor.description
        );

        // The manifest is the copy that reaches the model surface, so it must
        // carry the same steering as the descriptor.
        let manifest = package
            .manifest
            .capabilities
            .iter()
            .find(|capability| capability.id.as_str() == SHELL_CAPABILITY_ID)
            .expect("shell manifest");
        assert_eq!(manifest.description, descriptor.description);
    }
}

fn assert_coding_manifest_contract(descriptor: &CapabilityDescriptor) {
    let expected_effects = match descriptor.id.as_str() {
        WRITE_FILE_CAPABILITY_ID => vec![EffectKind::WriteFilesystem],
        APPLY_PATCH_CAPABILITY_ID => vec![EffectKind::ReadFilesystem, EffectKind::WriteFilesystem],
        _ => vec![EffectKind::ReadFilesystem],
    };
    assert_eq!(descriptor.effects, expected_effects);
    assert_eq!(descriptor.default_permission, PermissionMode::Allow);
}

fn is_coding_capability_id(id: &str) -> bool {
    matches!(
        id,
        READ_FILE_CAPABILITY_ID
            | WRITE_FILE_CAPABILITY_ID
            | LIST_DIR_CAPABILITY_ID
            | GLOB_CAPABILITY_ID
            | GREP_CAPABILITY_ID
            | APPLY_PATCH_CAPABILITY_ID
    )
}

#[tokio::test]
async fn builtin_first_party_package_omits_prompt_doc_refs() {
    let package = builtin_first_party_package().unwrap();

    assert!(
        package
            .manifest
            .capabilities
            .iter()
            .all(|capability| capability.prompt_doc_ref.is_none())
    );
}

#[tokio::test]
async fn builtin_first_party_surface_lists_allowed_tools_in_registry_order() {
    let runtime = runtime();
    let request = VisibleCapabilityRequest::new(
        execution_context(all_builtin_capability_ids()),
        SurfaceKind::new("agent_loop").unwrap(),
    )
    .with_policy(CapabilitySurfacePolicy::allow_all())
    .with_provider_trust(provider_trust());

    let surface = runtime.visible_capabilities(request).await.unwrap();

    let ids = surface
        .capabilities
        .iter()
        .map(|capability| capability.descriptor.id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, all_builtin_capability_ids());
    assert!(
        surface
            .capabilities
            .iter()
            .all(|capability| capability.access == VisibleCapabilityAccess::Available)
    );
    assert!(
        surface
            .capabilities
            .iter()
            .all(|capability| capability.estimated_resources.output_bytes.is_some())
    );
    let shell = surface
        .capabilities
        .iter()
        .find(|capability| capability.descriptor.id.as_str() == SHELL_CAPABILITY_ID)
        .expect("shell capability must be visible");
    assert_eq!(shell.estimated_resources.process_count, Some(1));

    let spawn = surface
        .capabilities
        .iter()
        .find(|capability| capability.descriptor.id.as_str() == SPAWN_SUBAGENT_CAPABILITY_ID)
        .expect("spawn_subagent capability must be visible");
    let properties = spawn
        .descriptor
        .parameters_schema
        .get("properties")
        .and_then(Value::as_object)
        .expect("spawn_subagent schema properties");
    assert!(properties.contains_key("subagent_type"));
    assert!(properties.contains_key("task"));
    assert!(properties.contains_key("handoff"));
    assert!(!properties.contains_key("mode"));
    assert!(!properties.contains_key("run_in_background"));
}

#[tokio::test]
async fn builtin_memory_search_surface_declares_internal_scope_boundary() {
    let runtime = runtime();
    let request = VisibleCapabilityRequest::new(
        execution_context(all_always_on_first_party_capability_ids()),
        SurfaceKind::new("agent_loop").unwrap(),
    )
    .with_policy(CapabilitySurfacePolicy::allow_all())
    .with_provider_trust(provider_trust());

    let surface = runtime.visible_capabilities(request).await.unwrap();

    let memory_search = surface
        .capabilities
        .iter()
        .find(|capability| capability.descriptor.id.as_str() == MEMORY_SEARCH_CAPABILITY_ID)
        .expect("memory_search must appear in surface");
    assert!(
        memory_search
            .descriptor
            .description
            .contains("only Reborn internal persistent memory"),
        "memory_search description should declare the internal-memory boundary: {}",
        memory_search.descriptor.description
    );
    assert!(
        memory_search
            .descriptor
            .description
            .contains("does not search connected app or extension data"),
        "memory_search description should avoid implying it can search external apps: {}",
        memory_search.descriptor.description
    );

    let schema = &memory_search.descriptor.parameters_schema;
    let schema_description = schema
        .get("description")
        .and_then(Value::as_str)
        .expect("memory_search schema should describe its search scope");
    assert!(
        schema_description.contains("only Reborn internal persistent memory"),
        "memory_search schema description should declare scope: {schema_description}"
    );
    assert!(
        schema_description.contains("does not search connected app or extension data"),
        "memory_search schema description should avoid external-app ambiguity: {schema_description}"
    );

    let query_description = schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get("query"))
        .and_then(|query| query.get("description"))
        .and_then(Value::as_str)
        .expect("memory_search query should have a description");
    assert!(
        query_description.contains("Reborn internal persistent memory"),
        "memory_search query description should declare internal memory: {query_description}"
    );
}

#[tokio::test]
async fn builtin_trigger_create_input_schema_declares_schedule_one_of() {
    let runtime = runtime_with_trigger_repository(Arc::new(InMemoryTriggerRepository::default()));
    let request = VisibleCapabilityRequest::new(
        execution_context(all_builtin_capability_ids()),
        SurfaceKind::new("agent_loop").unwrap(),
    )
    .with_policy(CapabilitySurfacePolicy::allow_all())
    .with_provider_trust(provider_trust());

    let surface = runtime.visible_capabilities(request).await.unwrap();

    let trigger_create = surface
        .capabilities
        .iter()
        .find(|capability| capability.descriptor.id.as_str() == TRIGGER_CREATE_CAPABILITY_ID)
        .expect("trigger_create must appear in surface");

    let schema = &trigger_create.descriptor.parameters_schema;

    // `schedule` must be listed in the `required` array.
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .expect("trigger_create schema must have a required array");
    let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        required_names.contains(&"schedule"),
        "schedule must be listed in required; got {required_names:?}"
    );
    assert_eq!(
        schema["properties"]["execution_contract"]["required"],
        json!([
            "version",
            "goal",
            "success_criteria",
            "output_instructions",
            "no_result_text",
            "policy"
        ])
    );
    assert_eq!(
        schema["properties"]["execution_contract"]["properties"]["policy"]["required"],
        json!(["result_delivery"])
    );
    assert!(
        schema["properties"]["execution_contract"]["properties"]["policy"]["properties"]
            .get("allowed_capability_ids")
            .is_none(),
        "the production-visible creation schema must leave future capability discovery dynamic"
    );
    assert!(
        !required_names.contains(&"completion_policy"),
        "completion_policy must NOT be in required; got {required_names:?}"
    );
    let root_description = schema
        .get("description")
        .and_then(Value::as_str)
        .expect("trigger_create schema must describe the top-level input shape");
    assert!(
        root_description.contains("new triggers must use `execution_contract`")
            && !schema["properties"]
                .as_object()
                .is_some_and(|properties| properties.contains_key("prompt")),
        "trigger_create schema should require structured new writes; got {root_description:?}"
    );

    // The `schedule` property must have a `oneOf`.
    let schedule_schema = schema
        .get("properties")
        .and_then(|p| p.get("schedule"))
        .expect("trigger_create schema must declare schedule property");
    let schedule_description = schedule_schema
        .get("description")
        .and_then(Value::as_str)
        .expect("trigger_create schedule schema must describe expected schedule object shape");
    assert!(
        schedule_description.contains("Do not pass {\"operation\":\"parse\",\"data\":...}"),
        "trigger_create schedule description should reject parse/data wrappers; got {schedule_description:?}"
    );
    let one_of = schedule_schema
        .get("oneOf")
        .and_then(Value::as_array)
        .expect("trigger_create schema schedule must have a oneOf array");
    assert_eq!(
        one_of.len(),
        2,
        "schedule oneOf must have exactly 2 variants; got {}",
        one_of.len()
    );

    // Confirm the two kinds are "cron" and "once".
    let kinds: Vec<&str> = one_of
        .iter()
        .filter_map(|v| {
            v.get("properties")
                .and_then(|p| p.get("kind"))
                .and_then(|k| k.get("const"))
                .and_then(Value::as_str)
        })
        .collect();
    assert!(
        kinds.contains(&"cron"),
        "schedule oneOf must have a cron variant; got {kinds:?}"
    );
    assert!(
        kinds.contains(&"once"),
        "schedule oneOf must have an once variant; got {kinds:?}"
    );
    for variant in one_of {
        assert_eq!(
            variant.get("type").and_then(Value::as_str),
            Some("object"),
            "schedule variants must declare type=object so provider argument normalization can decode stringified nested schedules"
        );
    }

    let validator = jsonschema::validator_for(schema).expect("trigger_create schema must compile");
    let input = json!({
        "name": "Tuesday reminder",
        "execution_contract": trigger_execution_contract("Send the Tuesday reminder"),
        "schedule": {
            "kind": "cron",
            "expression": "0 14 * * 2",
            "timezone": "America/Los_Angeles"
        }
    });
    validator
        .validate(&input)
        .expect("resolved trigger_create schema must accept weekly Tuesday cron input");

    let once_input = json!({
        "name": "Dog walking reminder",
        "execution_contract": trigger_execution_contract("Walk the dog"),
        "schedule": {
            "kind": "once",
            "at": "2026-06-23T14:00:00",
            "timezone": "America/Los_Angeles"
        }
    });
    validator
        .validate(&once_input)
        .expect("resolved trigger_create schema must accept one-time tomorrow input");

    let parse_wrapper_input = json!({
        "operation": "parse",
        "data": {
            "kind": "cron",
            "expression": "0 14 * * 2",
            "timezone": "America/Los_Angeles"
        }
    });
    assert!(
        validator.validate(&parse_wrapper_input).is_err(),
        "trigger_create schema must reject parser-style operation/data wrappers"
    );
}

/// Regression guard for issue #7246: the agent fabricated automation status
/// ("your BTC digest is running and sending to Telegram") while the Automations
/// page showed none. `builtin.trigger_list` IS the model's read path for that
/// state, so its model-visible description must (a) bridge the user vocabulary
/// ("automation", "routine") to the trigger capability id and (b) carry the
/// grounding rule: call it before asserting routine/automation existence or
/// status, never answer from memory. Driven through the production
/// `visible_capabilities` surface assembly, not the constant.
#[tokio::test]
async fn builtin_trigger_list_surface_grounds_automation_status_claims() {
    let runtime = runtime_with_trigger_repository(Arc::new(InMemoryTriggerRepository::default()));
    let request = VisibleCapabilityRequest::new(
        execution_context(all_builtin_capability_ids()),
        SurfaceKind::new("agent_loop").unwrap(),
    )
    .with_policy(CapabilitySurfacePolicy::allow_all())
    .with_provider_trust(provider_trust());

    let surface = runtime.visible_capabilities(request).await.unwrap();

    let trigger_list = surface
        .capabilities
        .iter()
        .find(|capability| capability.descriptor.id.as_str() == TRIGGER_LIST_CAPABILITY_ID)
        .expect("trigger_list must appear in surface");

    let description = &trigger_list.descriptor.description;
    // Vocabulary bridge: users ask about "automations" (the Automations page)
    // and "routines"; the capability id says "trigger". Without all three in
    // the description the model does not map status questions to this tool.
    assert!(
        description.contains("routine"),
        "trigger_list description must use the routine vocabulary: {description}"
    );
    assert!(
        description.contains("automation"),
        "trigger_list description must use the automation vocabulary: {description}"
    );
    // Grounding rule tied to the specific fabrication (issue #7246), mirroring
    // the outbound targets-list pattern: state the positive check-before-assert
    // rule, not generic caution.
    assert!(
        description.contains("Call this before"),
        "trigger_list description must instruct calling it before status claims: {description}"
    );
    assert!(
        description.contains("never report routine or automation status from"),
        "trigger_list description must forbid answering status from memory: {description}"
    );
    // Empty-state grounding: no rows means no automations exist — say so.
    assert!(
        description.contains("empty"),
        "trigger_list description must ground the empty result as 'no routines': {description}"
    );

    // The same grounding must survive into the model-visible input schema root
    // description (the trigger_create schema already carries steering there).
    let schema = &trigger_list.descriptor.parameters_schema;
    let schema_description = schema
        .get("description")
        .and_then(Value::as_str)
        .expect("trigger_list schema must describe the listing as authoritative state");
    assert!(
        schema_description.contains("authoritative"),
        "trigger_list schema description must declare the response authoritative: {schema_description}"
    );
    assert!(
        schema_description.contains("routine") && schema_description.contains("automation"),
        "trigger_list schema description must bridge routine/automation vocabulary: {schema_description}"
    );
}

#[tokio::test]
async fn builtin_first_party_surface_hides_runtime_policy_impossible_tools() {
    let runtime = runtime_with_policy(network_denied_policy());
    let request = VisibleCapabilityRequest::new(
        execution_context(all_builtin_capability_ids()),
        SurfaceKind::new("agent_loop").unwrap(),
    )
    .with_policy(CapabilitySurfacePolicy::allow_all())
    .with_provider_trust(provider_trust());

    let surface = runtime.visible_capabilities(request).await.unwrap();

    let ids = surface
        .capabilities
        .iter()
        .map(|capability| capability.descriptor.id.as_str())
        .collect::<Vec<_>>();
    assert!(!ids.contains(&HTTP_CAPABILITY_ID));
    assert!(!ids.contains(&HTTP_SAVE_CAPABILITY_ID));
    assert!(ids.contains(&ECHO_CAPABILITY_ID));
}

#[tokio::test]
async fn builtin_rejects_oversized_inputs_before_dispatch() {
    let outcome = runtime()
        .invoke_capability((
            execution_context([JSON_CAPABILITY_ID]),
            capability_id(JSON_CAPABILITY_ID),
            ResourceEstimate::default(),
            json!({"operation": "validate", "data": "x".repeat(1_048_577)}),
        ))
        .await
        .unwrap();

    let RuntimeCapabilityOutcome::Failed(failure) = outcome else {
        panic!("expected resource failure, got {outcome:?}");
    };
    assert_eq!(failure.kind, FailureKind::Resource);
}

#[tokio::test]
async fn builtin_rejects_oversized_outputs_before_return() {
    let outcome = runtime()
        .invoke_capability((
            execution_context([JSON_CAPABILITY_ID]),
            capability_id(JSON_CAPABILITY_ID),
            ResourceEstimate::default(),
            json!({"operation": "stringify", "data": {"items": vec!["xxxxxxxx"; 80_000]}}),
        ))
        .await
        .unwrap();

    let RuntimeCapabilityOutcome::Failed(failure) = outcome else {
        panic!("expected output-too-large failure, got {outcome:?}");
    };
    assert_eq!(failure.kind, FailureKind::OutputTooLarge);
}

#[tokio::test]
async fn builtin_missing_grant_denies_before_handler_dispatch() {
    let outcome = runtime()
        .invoke_capability((
            execution_context(std::iter::empty::<&str>()),
            capability_id(ECHO_CAPABILITY_ID),
            ResourceEstimate::default(),
            json!({"message":"must not run"}),
        ))
        .await
        .unwrap();

    let RuntimeCapabilityOutcome::Failed(failure) = outcome else {
        panic!("expected authorization failure, got {outcome:?}");
    };
    assert_eq!(failure.kind, FailureKind::Authorization);
}
