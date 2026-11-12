//! Helpers `assemble_runtime` calls: channel-connection facade, WebUI auth
//! interaction service, trusted-laptop audit record, the unattended deny-list,
//! and the allow-all capability surface resolver.

use super::*;

/// Build and start a Reborn agent runtime.
///
/// On return, the turn-runner worker is already running in the background and
/// the returned `RebornRuntime` is ready to accept `send_user_message` calls.
///
/// **Currently supported profiles:** `RebornCompositionProfile::Standalone`,
/// `RebornCompositionProfile::StandaloneUnrestricted`,
/// `RebornCompositionProfile::HostedSingleTenant`, and
/// `RebornCompositionProfile::Production` are wired end-to-end here. Production
/// starts only after readiness diagnostics validate that live traffic can be
/// exposed without a partial cutover.
/// Assemble the generic per-user channel-connection facade from its stores.
///
/// Shared by [`RebornRuntime::generic_channel_connection_facade`] (extensions
/// card / product surface) and the communication-context provider wiring in
/// `build_runtime_with_resource_governor` (#7247), so the model-facing
/// "connected channels" truth and the extensions card consult the same
/// connection service assembly.
pub(super) fn build_generic_channel_connection_facade(
    tenant_id: ironclaw_host_api::ids::TenantId,
    extension_management: &Arc<RebornLocalExtensionManagementPort>,
    channel_identity_store: &Arc<ironclaw_extension_host::FilesystemChannelIdentityStore>,
    product_auth: &Arc<RebornProductAuthServices>,
    channel_dm_target_store: &Arc<ironclaw_extension_host::FilesystemChannelDmTargetStore>,
    channel_pairing: Option<Arc<ChannelPairingRegistry>>,
) -> Arc<dyn ironclaw_auth::ChannelConnectionService> {
    let identity_store = Arc::clone(channel_identity_store);
    let installation_store = Some(extension_management.installation_store_handle());
    let credential_cleanup = Some(Arc::clone(product_auth)
        as Arc<dyn ironclaw_extension_host::channel_connection::ChannelCredentialCleanup>);
    let account_status_reader = Some(Arc::clone(product_auth)
        as Arc<dyn ironclaw_extension_host::channel_connection::ChannelAccountStatusReader>);
    Arc::new(
        ironclaw_extension_host::channel_connection::GenericChannelConnectionService::new(
            tenant_id,
            Vec::new(),
            installation_store,
            Arc::clone(&identity_store)
                as Arc<dyn ironclaw_host_api::user_identity::RebornUserIdentityLookup>,
            identity_store
                as Arc<dyn ironclaw_host_api::user_identity::RebornUserIdentityBindingDeleteStore>,
            credential_cleanup,
            account_status_reader,
            Some(Arc::clone(channel_dm_target_store)),
            channel_pairing,
        ),
    )
}

/// Thin wrapper over
pub(super) type NotificationInbox = Arc<dyn ironclaw_notifications::NotificationInboxStorePort>;

/// `build_webui_auth_interaction_service_with_turn_run_source` using
/// `agent_turn_runtime` as the turn-run state source.
pub(super) fn build_webui_auth_interaction_service(
    product_auth: &RebornProductAuthServices,
    process_gate_query_source: Arc<dyn ProcessGateQuerySource<Error = TurnError>>,
    turn_coordinator: Arc<dyn TurnCoordinator>,
    notification_inbox: NotificationInbox,
) -> Arc<dyn AuthInteractionService> {
    build_webui_auth_interaction_service_with_turn_run_source(
        product_auth,
        process_gate_query_source,
        turn_coordinator,
        notification_inbox,
    )
}

/// Identical to [`build_webui_auth_interaction_service`] except
/// the auth read model reads `turn_run_source` instead of a hardcoded
/// concrete row-store type. See
/// `build_approval_interaction_service_with_turn_run_source`'s doc
/// for why this seam exists.
pub(super) fn build_webui_auth_interaction_service_with_turn_run_source(
    product_auth: &RebornProductAuthServices,
    turn_run_source: Arc<dyn ProcessGateQuerySource<Error = TurnError>>,
    turn_coordinator: Arc<dyn TurnCoordinator>,
    notification_inbox: NotificationInbox,
) -> Arc<dyn AuthInteractionService> {
    // `AuthFlowRecordSource` is optional on the product-auth bundle because
    // production may supply a durable read projection that is not the flow
    // manager itself. Standalone can render pending WebUI auth interactions only
    // when the bundle explicitly exposes this scoped projection; otherwise the
    // WebUI surface fails closed with a stable unavailable error.
    let Some(flow_records) = product_auth.flow_record_source() else {
        return Arc::new(auth_interaction::UnavailableAuthInteractionService);
    };
    Arc::new(
        DefaultAuthInteractionService::new(
            Arc::new(auth_interaction::ProcessGateAuthInteractionReadModel::new(
                turn_run_source,
                flow_records,
            )),
            product_auth.flow_manager(),
            turn_coordinator,
        )
        .with_notification_inbox(notification_inbox),
    )
}

const TRUSTED_LAPTOP_ACCESS_AUDIT_KIND: &str = "standalone_trusted_laptop_access";
const TRUSTED_LAPTOP_ACCESS_AUDIT_TARGET: &str = "filesystem=host_workspace_and_home;process=local_host;network=direct;secrets=inherited_env;host_home_mount=/host";
const TRUSTED_LAPTOP_ACCESS_AUDIT_STATUS: &str = "host_home_mounted_read_write";

pub(super) async fn append_trusted_laptop_access_audit(
    audit_log: &Arc<dyn DurableAuditLog>,
    thread_scope: &ThreadScope,
    actor_user_id: &UserId,
) -> Result<(), RebornRuntimeError> {
    let invocation_id = InvocationId::new();
    audit_log
        .append(AuditEnvelope {
            event_id: AuditEventId::new(),
            correlation_id: CorrelationId::new(),
            stage: AuditStage::After,
            timestamp: Utc::now(),
            tenant_id: thread_scope.tenant_id.clone(),
            user_id: actor_user_id.clone(),
            agent_id: Some(thread_scope.agent_id.clone()),
            project_id: thread_scope.project_id.clone(),
            mission_id: thread_scope.mission_id.clone(),
            thread_id: None,
            invocation_id,
            process_id: None,
            approval_request_id: None,
            extension_id: None,
            action: ActionSummary {
                kind: TRUSTED_LAPTOP_ACCESS_AUDIT_KIND.to_string(),
                target: Some(TRUSTED_LAPTOP_ACCESS_AUDIT_TARGET.to_string()),
                effects: vec![
                    EffectKind::ReadFilesystem,
                    EffectKind::WriteFilesystem,
                    EffectKind::SpawnProcess,
                    EffectKind::Network,
                    EffectKind::UseSecret,
                ],
            },
            decision: DecisionSummary {
                kind: "allowed".to_string(),
                reason: None,
                actor: None,
                feedback: None,
            },
            result: Some(ActionResultSummary {
                success: true,
                status: Some(TRUSTED_LAPTOP_ACCESS_AUDIT_STATUS.to_string()),
                output_bytes: None,
            }),
        })
        .await
        .map(|_| ())
        .map_err(|error| RebornRuntimeError::InvalidArgument {
            reason: format!("could not record trusted laptop access audit event: {error}"),
        })
}

/// Reads a boolean feature flag from the environment. Absent or unrecognized
/// values are treated as off — this gates an opt-in prompt addendum for
/// unattended dataset evaluation (see `default_system_prompt.rs`), not a
/// required config value, so we default closed rather than erroring on a
/// typo'd value.
pub(super) fn bool_env_flag(key: &'static str) -> bool {
    match std::env::var(key) {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes"
        ),
        Err(_) => false,
    }
}

pub(super) fn optional_nonzero_u32_env(
    key: &'static str,
) -> Result<Option<std::num::NonZeroU32>, RebornRuntimeError> {
    match std::env::var(key) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            let parsed =
                trimmed
                    .parse::<u32>()
                    .map_err(|error| RebornRuntimeError::InvalidArgument {
                        reason: format!("{key} must be a positive integer: {error}"),
                    })?;
            if parsed == 0 {
                return Err(RebornRuntimeError::InvalidArgument {
                    reason: format!("{key} must be greater than zero"),
                });
            }
            Ok(std::num::NonZeroU32::new(parsed))
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(RebornRuntimeError::InvalidArgument {
            reason: format!("could not read {key}: {error}"),
        }),
    }
}

/// The mutating synthetic capabilities denied to an unattended run.
///
/// Why the list has to exist, and why it lives here rather than in the loop
/// layer that enforces it: see
/// `DefaultPlannedRuntimeConfig::unattended_denied_capability_ids`.
///
/// Derived by measurement, not inspection — with global auto-approve off these
/// are the capabilities that survive because they bypass the surface policy.
/// `tests/integration/suggestions.rs` pins the resulting set.
pub(super) fn unattended_denied_capability_ids() -> Result<Vec<CapabilityId>, RebornRuntimeError> {
    [
        ironclaw_assistant::PROJECT_CREATE_CAPABILITY_ID,
        ironclaw_assistant::OUTBOUND_NOTIFICATION_CHANNELS_SET_CAPABILITY_ID,
        ironclaw_loop_host::SKILL_ACTIVATE_CAPABILITY_ID,
        ironclaw_memory::PROFILE_SET_CAPABILITY_ID,
        ironclaw_host_runtime::TRACE_COMMONS_ONBOARD_CAPABILITY_ID,
    ]
    .into_iter()
    .map(|id| {
        CapabilityId::new(id).map_err(|reason| RebornRuntimeError::InvalidArgument {
            reason: format!("unattended-denied capability id: {reason}"),
        })
    })
    .collect()
}

pub(super) struct AllowAllCapabilitySurfaceResolver;

#[async_trait::async_trait]
impl CapabilitySurfaceProfileResolver for AllowAllCapabilitySurfaceResolver {
    async fn resolve(
        &self,
        _run_context: &LoopRunContext,
    ) -> Result<CapabilitySurfacePolicy, CapabilityResolveError> {
        Ok(CapabilitySurfacePolicy::allow_all())
    }
}
