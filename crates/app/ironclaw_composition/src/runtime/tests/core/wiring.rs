//! Runtime wiring regressions: persistent-approval grantee resolution,
//! deployment channel identity binding, and the IronHub shared-key link
//! service.

use super::*;

fn capability_provider_contracts() -> ironclaw_extension_registry::HostApiContractRegistry {
    let mut contracts = ironclaw_extension_registry::HostApiContractRegistry::new();
    contracts
        .register(std::sync::Arc::new(
            ironclaw_extension_registry::CapabilityProviderHostApiContract::new()
                .expect("capability provider contract"),
        ))
        .expect("register capability provider contract");
    contracts
}

#[async_trait]
impl EventSink for OverloadedEventSink {
    async fn emit(&self, event: RuntimeEvent) -> Result<(), EventError> {
        NonBlockingEventSink::try_emit(self, event)
    }
}

impl NonBlockingEventSink for OverloadedEventSink {
    fn try_emit(&self, event: RuntimeEvent) -> Result<(), EventError> {
        self.attempted_events
            .lock()
            .expect("attempted event recording mutex is not poisoned")
            .push(event);
        Err(EventError::Sink {
            reason: "injected saturated observability queue".to_string(),
        })
    }
}

#[derive(Default)]
struct SlackDmOpenNetworkEgress {
    calls: AtomicUsize,
}

#[async_trait]
impl ironclaw_network::NetworkHttpEgress for SlackDmOpenNetworkEgress {
    async fn execute(
        &self,
        request: ironclaw_network::NetworkHttpRequest,
    ) -> Result<ironclaw_network::NetworkHttpResponse, ironclaw_network::NetworkHttpError> {
        assert!(
            request.url.ends_with("/api/conversations.open"),
            "unexpected Slack request: {}",
            request.url
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body = br#"{"ok":true,"channel":{"id":"D-RUNTIME-RACE"}}"#.to_vec();
        Ok(ironclaw_network::NetworkHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            usage: ironclaw_network::NetworkUsage {
                request_bytes: request.body.len() as u64,
                response_bytes: body.len() as u64,
                resolved_ip: None,
            },
            body,
        })
    }
}

#[test]
fn persistent_grantee_resolver_maps_notification_channels_set_to_synthetic_provider() {
    let registry = Arc::new(ironclaw_extension_registry::ExtensionRegistry::new());
    let resolver = crate::runtime::RegistryPersistentApprovalGranteeResolver::new(registry)
        .expect("resolver builds");
    let capability_id =
        CapabilityId::new(ironclaw_assistant::OUTBOUND_NOTIFICATION_CHANNELS_SET_CAPABILITY_ID)
            .expect("capability id");
    let expected_provider =
        ironclaw_assistant::outbound_delivery_synthetic_provider().expect("synthetic provider id");

    assert_eq!(
        ironclaw_assistant::PersistentApprovalGranteeResolver::persistent_approval_grantee(
            &resolver,
            &capability_id
        ),
        Some(Principal::Extension(expected_provider))
    );
}

#[test]
fn persistent_grantee_resolver_maps_registered_capability_to_provider() {
    let manifest = r#"
schema_version = "reborn.extension_manifest.v2"
id = "approval-provider"
name = "approval-provider"
version = "0.1.0"
description = "approval provider"
trust = "third_party"

[runtime]
kind = "wasm"
module = "wasm/approval-provider.wasm"

[[host_api]]
id = "ironclaw.capability_provider/v1"
section = "capability_provider.tools"

[capability_provider.tools]

[[capability_provider.tools.capabilities]]
id = "approval-provider.write"
description = "write"
effects = ["external_write"]
default_permission = "ask"
visibility = "model"
input_schema_ref = "schemas/write.input.json"
output_schema_ref = "schemas/write.output.json"
"#;
    let manifest = ironclaw_extension_registry::ExtensionManifest::parse(
        manifest,
        ironclaw_extension_registry::ManifestSource::HostBundled,
        &ironclaw_host_api::host_port::HostPortCatalog::empty(),
        &capability_provider_contracts(),
    )
    .expect("manifest parses");
    let package = ironclaw_extension_registry::ExtensionPackage::from_manifest(
        manifest,
        ironclaw_host_api::path::VirtualPath::new("/system/extensions/approval-provider")
            .expect("root"),
    )
    .expect("package builds");
    let mut registry = ironclaw_extension_registry::ExtensionRegistry::new();
    registry.insert(package).expect("package inserts");
    let resolver =
        crate::runtime::RegistryPersistentApprovalGranteeResolver::new(Arc::new(registry))
            .expect("resolver builds");
    let capability_id = CapabilityId::new("approval-provider.write").expect("capability id");
    let expected_provider =
        ironclaw_host_api::ids::ExtensionId::new("approval-provider").expect("extension id");

    assert_eq!(
        ironclaw_assistant::PersistentApprovalGranteeResolver::persistent_approval_grantee(
            &resolver,
            &capability_id,
        ),
        Some(Principal::Extension(expected_provider))
    );
}

#[tokio::test]
async fn runtime_channel_identity_bind_uses_deployment_channel_before_user_activation() {
    let root = tempfile::tempdir().expect("tempdir");
    let network_egress = Arc::new(SlackDmOpenNetworkEgress::default());
    let build_input = crate::deployment::local_filesystem_build_input(
        "runtime-channel-bind-race-owner",
        root.path().join("standalone"),
    )
    .with_runtime_policy(standalone_runtime_policy())
    .with_network_http_egress_for_test(network_egress.clone())
    .with_channel_extension_bindings(vec![crate::input::ChannelExtensionBinding {
        extension_id: ironclaw_host_api::ids::ExtensionId::from_trusted("slack".to_string()),
        surfaces: {
            let adapter = Arc::new(ironclaw_slack_extension::SlackChannelAdapter);
            ironclaw_extension_contracts::channel_adapter::ChannelSurfaces::default()
                .with_ingress(adapter.clone())
                .with_reply(adapter.clone())
                .with_delivery(adapter)
        },
        preference_target_codec: None,
        outbound_target_provider: None,
        first_party_initializer: None,
        registration_document_path: None,
    }]);
    let input =
        RebornRuntimeInput::from_build_input(build_input).with_identity(RebornRuntimeIdentity {
            tenant_id: "runtime-channel-bind-race-tenant".to_string(),
            agent_id: "runtime-channel-bind-race-agent".to_string(),
            source_binding_id: "runtime-channel-bind-race-source".to_string(),
            reply_target_binding_id: "runtime-channel-bind-race-reply".to_string(),
        });
    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    assert!(
        runtime
            .ironhub_register_route_mount()
            .expect("default-off register route composes")
            .is_none(),
        "the public register route must remain absent without a shared key"
    );
    let extension_management = &runtime.extension_management;
    let operator = extension_management
        .tenant_operator_user_id_for_test()
        .clone();
    let slack_ref = LifecyclePackageRef::new(LifecyclePackageKind::Extension, "slack")
        .expect("valid Slack ref");
    extension_management
        .install(slack_ref.clone(), &operator)
        .await
        .expect("install Slack before OAuth callback");

    let slack_id = ironclaw_host_api::ids::ExtensionId::new("slack").expect("Slack extension id");
    runtime
        .channel_config_service
        .save(
            &slack_id,
            vec![
                ("slack_bot_token".to_string(), "xoxb-test".to_string()),
                (
                    "slack_signing_secret".to_string(),
                    "signing-test".to_string(),
                ),
                ("slack_team_id".to_string(), "T-RUNTIME".to_string()),
                ("slack_api_app_id".to_string(), "A-RUNTIME".to_string()),
                ("slack_installation_id".to_string(), "I-RUNTIME".to_string()),
                ("slack_bot_user_id".to_string(), "U-BOT-RUNTIME".to_string()),
                (
                    "slack_oauth_client_id".to_string(),
                    "runtime-slack-client".to_string(),
                ),
                (
                    "slack_oauth_client_secret".to_string(),
                    "runtime-slack-client-secret".to_string(),
                ),
            ],
        )
        .await
        .expect("configure Slack channel deployment values");

    let binding_config = runtime
        .channel_identity_binding_config()
        .expect("production runtime channel identity binding config");
    let mut resource = ResourceScope::local_default(operator.clone(), InvocationId::new())
        .expect("callback resource scope");
    resource.tenant_id = runtime.thread_scope.tenant_id.clone();
    let callback_scope =
        ironclaw_auth::AuthProductScope::new(resource, ironclaw_auth::AuthSurface::Callback);
    let identity = ironclaw_auth::OAuthProviderIdentity::new(
        "U-RUNTIME",
        Some("T-RUNTIME".to_string()),
        None,
        Some("A-RUNTIME".to_string()),
    )
    .expect("proven Slack identity");
    let transaction =
        ironclaw_extension_host::channel_identity_binding::bind_channel_identities_for_callback(
            &binding_config,
            "slack",
            &callback_scope,
            Some(&identity),
        )
        .await
        .expect("bind Slack identity before activation")
        .expect("Slack callback maps to the installed channel extension");
    transaction.commit().await;

    let dm_targets = &runtime.channel_dm_target_store;

    let record = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(record) = dm_targets
                .load("slack", &operator)
                .await
                .expect("load deployment-owned DM target")
            {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("deployment channel should provision before user activation");
    assert_eq!(record.target["conversation_id"], "D-RUNTIME-RACE");
    assert_eq!(network_egress.calls.load(Ordering::SeqCst), 1);

    extension_management
        .activate_with_prechecked_credentials_for_test(slack_ref)
        .await
        .expect("activate Slack and publish the generic host snapshot");

    // Deployment registration wins over the compatibility activation snapshot,
    // so activation must not create a second delivery binding or provider call.
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(network_egress.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn runtime_with_ironhub_shared_key_builds_link_service_and_public_register_mount() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "unused IronHub runtime test reply".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-ironhub-link-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_ironhub_agent_shared_key(
        ironclaw_extension_manager::ironhub::IronhubSharedKey::new(
            "ihub_sk_RuntimeLinkTestKey000000000000000000000000000",
        )
        .expect("shared key"),
    )
    .with_model_gateway_override(gateway)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-ironhub-link-tenant".to_string(),
        agent_id: "runtime-ironhub-link-agent".to_string(),
        source_binding_id: "runtime-ironhub-link-source".to_string(),
        reply_target_binding_id: "runtime-ironhub-link-reply".to_string(),
    });

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    use ironclaw_extension_manager::ironhub::RebornIronHubRuntime;
    assert!(runtime.ironhub_runtime_http_egress().is_some());
    drop(runtime.ironhub_skill_management());
    drop(runtime.ironhub_extension_management());
    drop(runtime.ironhub_link_state());
    assert_eq!(
        runtime.ironhub_manifest_url().as_str(),
        ironclaw_extension_manager::ironhub::IronhubManifestUrl::default().as_str()
    );
    let product_surface = runtime
        .product_surface(None)
        .expect("IronHub link reaches product surface");
    let mount = runtime
        .ironhub_register_route_mount()
        .expect("register route composes")
        .expect("shared key enables register route");
    assert_eq!(mount.descriptors.len(), 1);
    assert_eq!(
        mount.descriptors[0].route_pattern().as_str(),
        crate::IRONHUB_REGISTER_PATH
    );
    drop(product_surface);
    drop(mount);

    drop(runtime);
}
