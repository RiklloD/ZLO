//! `RebornRuntime` accessors gated behind `test` / `test-support`: the
//! substrate seams integration harnesses drive, plus the gate-aware
//! `send_user_message_until_gate` drive helper (zero bytes in production).

use super::*;

impl RebornRuntime {
    #[cfg(any(test, feature = "test-support"))]
    pub fn secret_store_for_test(&self) -> Arc<dyn SecretStorePort> {
        self.secret_store()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn host_runtime_for_test(&self) -> Option<Arc<dyn HostRuntime>> {
        Some(Arc::clone(&self.host_runtime))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn product_auth_for_test(&self) -> Arc<RebornProductAuthServices> {
        self.product_auth_services()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn turn_coordinator_for_test(&self) -> Arc<dyn TurnCoordinator> {
        Arc::clone(&self.turn_coordinator)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn standalone_auto_approve_settings_for_test(
        &self,
    ) -> Option<Arc<dyn ironclaw_approvals::AutoApproveSettingStorePort>> {
        Some(self.auto_approve_settings.clone()
            as Arc<dyn ironclaw_approvals::AutoApproveSettingStorePort>)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn extension_installation_store_for_test(
        &self,
    ) -> Option<Arc<dyn ironclaw_extension_registry::ExtensionInstallationStorePort>> {
        Some(self.extension_management.installation_store_for_test())
    }

    /// Test-only caller for the production lifecycle install path used by the
    /// WebUI/product facade. This keeps whole-runtime channel tests on the real
    /// catalog, installation store, and generic-host publication path.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn install_extension_for_test(
        &self,
        package_ref: ironclaw_assistant::LifecyclePackageRef,
    ) -> Result<
        ironclaw_assistant::LifecycleProductResponse,
        ironclaw_assistant::ProductSurfaceFailure,
    > {
        self.extension_management
            .install(package_ref, &self.actor_user_id)
            .await
            .map_err(ironclaw_assistant::ProductSurfaceFailure::from)
    }

    /// Test-only caller for the production static activation path with the
    /// existing prechecked-credential gate. Whole-runtime channel tests use it
    /// when the user-tool credential account is outside the scenario under
    /// test; channel configuration and activation still run through their real
    /// stores and generic-host publication.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn activate_extension_for_test(
        &self,
        package_ref: ironclaw_assistant::LifecyclePackageRef,
    ) -> Result<
        ironclaw_assistant::LifecycleProductResponse,
        ironclaw_assistant::ProductSurfaceFailure,
    > {
        self.extension_management
            .activate_with_prechecked_credentials_for_test(package_ref)
            .await
            .map_err(ironclaw_assistant::ProductSurfaceFailure::from)
    }

    /// Test-support handles onto the approval/lease/gate stores the integration
    /// harness drives approve/deny flows through. All four stores are
    /// reconstructed fresh over the runtime's own composite root
    /// (`crate::wrap_scoped(self.extension_filesystem)`), not read off a stored
    /// field: every one is a stateless filesystem-backed CAS store, so a fresh
    /// instance over the same root reads/writes the exact durable rows the
    /// runtime's turns produce (the same reopen-over-same-root equivalence
    /// `outbound_store_durability` relies on). This mirrors what production
    /// composition builds at `factory.rs` (`ApprovalRequestStore` /
    /// `CapabilityLeaseStore` over `scoped_filesystem`). Test-support
    /// only; zero bytes in production builds.
    #[cfg(any(test, feature = "test-support"))]
    pub fn standalone_approval_test_parts(&self) -> Option<crate::RebornApprovalTestParts> {
        let capability_store_filesystem =
            crate::wrap_scoped(Arc::clone(&self.extension_filesystem));
        let approval_requests: Arc<dyn ironclaw_approvals::ApprovalRequestStorePort> = Arc::new(
            ironclaw_approvals::ApprovalRequestStore::new(Arc::clone(&capability_store_filesystem)),
        );
        let capability_leases: Arc<dyn ironclaw_authorization::CapabilityLeaseStorePort> =
            Arc::new(ironclaw_authorization::CapabilityLeaseStore::new(
                Arc::clone(&capability_store_filesystem),
            ));
        let gate_record_store: Arc<dyn ironclaw_approvals::GateRecordStorePort> = Arc::new(
            ironclaw_approvals::GateRecordStore::new(Arc::clone(&capability_store_filesystem)),
        );
        let replay_payload_store: Arc<dyn ironclaw_capabilities::ReplayPayloadStorePort> = Arc::new(
            ironclaw_capabilities::ReplayPayloadStore::new(capability_store_filesystem),
        );
        Some(crate::RebornApprovalTestParts {
            approval_requests,
            capability_leases,
            gate_record_store,
            replay_payload_store,
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn standalone_profile_filesystem_for_test(
        &self,
    ) -> Option<Arc<dyn ironclaw_filesystem::RootFilesystem>> {
        Some(Arc::clone(&self.extension_filesystem) as Arc<dyn ironclaw_filesystem::RootFilesystem>)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn standalone_project_service_for_test(
        &self,
    ) -> Option<Arc<dyn ironclaw_product_contracts::project_service::ProjectService>> {
        Some(Arc::clone(&self.project_service))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn standalone_thread_service_for_test(
        &self,
    ) -> Option<Arc<dyn ironclaw_threads::SessionThreadService>> {
        Some(Arc::clone(&self.thread_service))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn standalone_outbound_preferences_for_test(
        &self,
    ) -> Option<Arc<dyn CommunicationPreferenceRepository>> {
        Some(Arc::clone(&self.outbound_preferences))
    }

    #[cfg(any(test, feature = "test-support"))]
    #[allow(clippy::type_complexity)]
    pub fn outbound_delivery_stores_for_test(
        &self,
    ) -> Option<(
        Arc<dyn ironclaw_outbound::OutboundStateStorePort>,
        Arc<dyn ironclaw_outbound::DeliveredGateRouteStore>,
        Arc<dyn ironclaw_outbound::CommunicationPreferenceRepository>,
        Arc<dyn ironclaw_outbound::ReplyAttachmentIntentPort>,
        Arc<dyn OutboundDeliveryTargetProvider>,
    )> {
        Some((
            Arc::clone(&self.outbound_state.state),
            Arc::clone(&self.delivered_gate_routes),
            Arc::clone(&self.outbound_preferences),
            Arc::clone(&self.outbound_state.reply_attachment_intents),
            self.outbound_delivery_target_provider()?,
        ))
    }

    /// Test-only accessor for the same durable triggered-delivery outcome store
    /// the composition-owned post-submit hook records into. Whole-path tests use
    /// it to await detached delivery completion without observing task timing.
    #[cfg(any(test, feature = "test-support"))]
    pub fn triggered_run_delivery_store_for_test(
        &self,
    ) -> Option<Arc<dyn ironclaw_outbound::TriggeredRunDeliveryStore>> {
        Some(Arc::clone(&self.triggered_run_delivery))
    }

    /// Test-only readiness projection for the production generic channel-host
    /// assembly. Activation publishes snapshots asynchronously; whole-runtime
    /// delivery tests wait until the owning preference codec is routable before
    /// firing a trigger.
    #[cfg(any(test, feature = "test-support"))]
    pub fn active_channel_preference_codec_ids_for_test(&self) -> Vec<String> {
        self._channel_host_assembly
            .as_ref()
            .map(|assembly| {
                assembly
                    .active_preference_codecs()
                    .into_iter()
                    .map(|(extension_id, _)| extension_id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The runtime's delivery coordinator. Tests only: an integration harness
    /// that wires its own run-delivery observer shares it so one publication
    /// owns each run's answer; production wiring receives the coordinator
    /// through the factory, never through this accessor.
    #[cfg(any(test, feature = "test-support"))]
    pub fn delivery_coordinator(&self) -> Option<Arc<ironclaw_assistant::DeliveryCoordinator>> {
        self.delivery_coordinator.clone()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn start_channel_host_assembly_for_test(
        &self,
        wiring: crate::ChannelHostAssemblyTestWiring,
    ) -> Option<Arc<ironclaw_extension_host::channel_host::GenericChannelHostAssembly>> {
        let crate::ChannelHostAssemblyTestWiring {
            thread_service,
            turn_coordinator,
            identity,
            run_delivery_settings,
            reply_projection,
        } = wiring;
        let attachment_filesystem = self.read_write_workspace_filesystem()?;
        let inbound_attachments: Arc<dyn ironclaw_attachments::InboundAttachmentLander> =
            Arc::new(ironclaw_attachments::ProjectScopedAttachmentLander::new(
                Arc::clone(&attachment_filesystem),
            ));
        let project_filesystem: Arc<dyn ironclaw_assistant::ProjectFilesystemReader> = Arc::new(
            ironclaw_assistant::ProjectScopedFilesystemReader::with_max_read_bytes(
                attachment_filesystem,
                ironclaw_attachments::DEFAULT_ATTACHMENT_BUDGETS.max_file_bytes as u64,
            ),
        );
        let source = crate::extension_host_assembly::ChannelHostAssemblySource {
            generic_host: self.extension_management.generic_host()?,
            ingress_registry: Arc::clone(&self.extension_ingress.as_ref()?.registry),
            workflow_filesystem: self.extension_filesystem.clone(),
            inbound_attachments,
            project_filesystem,
            delivery_coordinator: self.delivery_coordinator.clone(),
            outbound_state: Arc::clone(&self.outbound_state.state),
            delivered_gate_routes: Arc::clone(&self.delivered_gate_routes),
            outbound_preferences: Arc::clone(&self.outbound_preferences),
            notification_inbox: Arc::clone(&self.notification_inbox),
            triggered_delivery_store: Arc::clone(&self.triggered_run_delivery),
            outbound_delivery_targets: Arc::clone(self.outbound_delivery_target_registry.as_ref()?)
                as Arc<dyn ironclaw_outbound::OutboundDeliveryTargetProvider>,
            identity_lookup: Arc::clone(&self.channel_identity_store)
                as Arc<dyn ironclaw_host_api::user_identity::RebornUserIdentityLookup>,
            dm_targets: Arc::clone(&self.channel_dm_target_store),
            deployment_channels: Arc::clone(&self.deployment_channels),
            channel_config: Arc::clone(&self.channel_config_service),
            channel_pairing: self.channel_pairing.clone(),
        };
        let admin_users: Arc<dyn AdminUserService> =
            Arc::new(ironclaw_assistant::RebornAdminUserDirectory::new(
                self.reborn_user_directory(),
                self.reborn_admin_secret_provisioner(),
                Arc::new(ironclaw_assistant::RejectingAdminApiTokenMinter),
            ));
        Some(
            crate::extension_host_assembly::start_channel_host(
                &source,
                crate::extension_host_assembly::ChannelHostAssemblyWiring {
                    thread_service,
                    turn_coordinator,
                    input_enqueue: self.webui_input_enqueue(),
                    llm_config: self.llm_config_service.clone(),
                    approval_interaction: None,
                    auth_interaction: None,
                    identity,
                    approval_context: None,
                    blocked_auth_prompts: None,
                    auth_flow_cancel: None,
                    run_delivery_settings,
                    admin_users,
                    reply_projection,
                    session_reply_channel: self
                        .session_channel_extension_id
                        .clone()
                        .map(ironclaw_host_api::ids::ExtensionId::from_trusted),
                    start_reply_publication: true,
                },
            )
            .assembly,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn pairing_mint_for_test(
        &self,
        extension_id: &str,
        user_id: &ironclaw_host_api::ids::UserId,
    ) -> Option<String> {
        let service = self.channel_pairing.as_ref()?.get(extension_id)?;
        service
            .issue_or_rotate(user_id)
            .await
            .ok()
            .map(|issue| issue.code.as_str().to_string())
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn pairing_issue_for_test(
        &self,
        extension_id: &str,
        user_id: &ironclaw_host_api::ids::UserId,
    ) -> Option<(String, Option<String>, chrono::DateTime<chrono::Utc>)> {
        let service = self.channel_pairing.as_ref()?.get(extension_id)?;
        service.issue_or_rotate(user_id).await.ok().map(|issue| {
            (
                issue.code.as_str().to_string(),
                issue.deep_link,
                issue.expires_at,
            )
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn pairing_consume_for_test(
        &self,
        extension_id: &str,
        authenticated_installation_id: &str,
        raw_code: &str,
        actor: (&str, &str, Option<&str>, &str),
        turn_world: (
            Arc<dyn ironclaw_turns::TurnCoordinator>,
            Arc<dyn ProcessGateQuerySource<Error = TurnError>>,
            ironclaw_host_api::ids::TenantId,
        ),
    ) -> Result<Option<ironclaw_host_api::ids::UserId>, String> {
        let (actor_kind, external_actor_id, conversation_space_id, conversation_id) = actor;
        let Some(service) = self
            .channel_pairing
            .as_ref()
            .and_then(|registry| registry.get(extension_id))
        else {
            return Ok(None);
        };
        let installation_id = ironclaw_host_api::product_adapter::AdapterInstallationId::new(
            authenticated_installation_id,
        )
        .map_err(|error| error.to_string())?;
        let outcome = service
            .consume(
                &installation_id,
                raw_code,
                actor_kind,
                external_actor_id,
                conversation_space_id,
                conversation_id,
            )
            .await
            .map_err(|error| error.to_string())?;
        let paired_user = match outcome {
            ChannelPairingConsumeOutcome::Paired { user_id }
            | ChannelPairingConsumeOutcome::AlreadyPairedSameUser { user_id } => Some(user_id),
            ChannelPairingConsumeOutcome::AlreadyBoundToOtherUser
            | ChannelPairingConsumeOutcome::ExpiredOrUnknown => None,
        };
        if let Some(user_id) = paired_user.as_ref() {
            let (turn_coordinator, turn_state, tenant_id) = turn_world;
            let continuation = crate::factory::auth_continuation_dispatcher(
                turn_coordinator,
                Some(turn_state),
                Some(Arc::clone(&self.notification_inbox)),
            );
            service
                .dispatch_pairing_completion_with_for_test(user_id, tenant_id, continuation)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(paired_user)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn pairing_connected_for_test(
        &self,
        extension_id: &str,
        user_id: &ironclaw_host_api::ids::UserId,
    ) -> Option<bool> {
        let service = self.channel_pairing.as_ref()?.get(extension_id)?;
        service
            .status_for(user_id)
            .await
            .ok()
            .map(|status| status.connected)
    }

    /// The composed pairing registry, for building the protected pairing
    /// route mount (`pairing/{mint,status,unpair}`). Tests only.
    ///
    /// Composition hands out the *registry*, not the mount: the routes live in
    /// `ironclaw_webui` (PROPOSAL §6.9.4) and this crate does not depend on
    /// the transport — `ironclaw_webui` is a dev-dependency here, not a
    /// normal one, and building the mount from production code would make it
    /// a normal one.
    #[cfg(any(test, feature = "test-support"))]
    pub fn channel_pairing_registry_for_test(&self) -> Option<Arc<ChannelPairingRegistry>> {
        self.channel_pairing.as_ref().map(Arc::clone)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn channel_config_service(&self) -> Option<Arc<dyn ChannelConfigProductService>> {
        Some(Arc::new(
            ironclaw_extension_manager::RebornChannelConfigProductService::new(Arc::clone(
                &self.channel_config_service,
            )),
        ))
    }

    #[cfg(feature = "test-support")]
    pub fn register_static_channel_egress_credentials_for_test(
        &self,
        entries: Vec<(String, String, ironclaw_secrets::SecretMaterial)>,
    ) -> bool {
        let Some(bridges) = &self.channel_egress_credential_bridges else {
            return false;
        };
        bridges.register(Arc::new(
            ironclaw_extension_host::channel_egress::StaticChannelEgressCredentials::new(entries),
        ));
        true
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn standalone_tool_permission_overrides_for_test(
        &self,
    ) -> Option<Arc<dyn ironclaw_approvals::CapabilityPermissionOverrideStorePort>> {
        Some(self.tool_permission_overrides.clone()
            as Arc<dyn ironclaw_approvals::CapabilityPermissionOverrideStorePort>)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn standalone_persistent_approval_policies_for_test(
        &self,
    ) -> Option<Arc<dyn ironclaw_approvals::PersistentApprovalPolicyStorePort>> {
        Some(self.persistent_approval_policies.clone()
            as Arc<dyn ironclaw_approvals::PersistentApprovalPolicyStorePort>)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn standalone_shared_trigger_repository_for_test(
        &self,
    ) -> Option<Arc<dyn ironclaw_triggers::TriggerRepository>> {
        Some(Arc::clone(&self.trigger_repository))
    }

    // Single owner of the `ProjectScopedAttachmentReader` construction recipe
    // for tests. Sources the workspace view from
    // [`Self::read_write_workspace_filesystem`] (the same read-write handle the
    // production `webui_workspace_filesystem` seam uses) rather than a dedicated
    // stored handle: the reader only reads, so a read-write view is
    // behaviorally identical for read paths, and this keeps the accessor off a
    // separately-stored runtime field. Test-support only; zero bytes shipped in
    // production builds.
    #[cfg(feature = "test-support")]
    pub(super) fn standalone_workspace_attachment_reader_for_test(
        &self,
    ) -> Option<Arc<ironclaw_assistant::ProjectScopedAttachmentReader<CompositeRootFilesystem>>>
    {
        Some(Arc::new(
            ironclaw_assistant::ProjectScopedAttachmentReader::new(
                self.read_write_workspace_filesystem()?,
            ),
        ))
    }

    #[cfg(feature = "test-support")]
    pub fn standalone_inbound_attachment_reader_for_test(
        &self,
    ) -> Option<Arc<dyn ironclaw_attachments::InboundAttachmentReader>> {
        Some(self.standalone_workspace_attachment_reader_for_test()?
            as Arc<dyn ironclaw_attachments::InboundAttachmentReader>)
    }

    #[cfg(feature = "test-support")]
    pub fn standalone_attachment_test_support_for_test(
        &self,
    ) -> Option<crate::factory::AttachmentTestSupport> {
        let read_port = self.standalone_workspace_attachment_reader_for_test()?
            as Arc<dyn ironclaw_loop_host::LoopAttachmentReadPort>;
        let read_write_workspace_filesystem = self.read_write_workspace_filesystem()?;
        Some(crate::factory::AttachmentTestSupport {
            read_port,
            lander: Arc::new(ironclaw_attachments::ProjectScopedAttachmentLander::new(
                read_write_workspace_filesystem,
            )),
        })
    }

    #[cfg(feature = "test-support")]
    pub async fn publish_bundled_extension_for_test(
        &self,
        package: &ironclaw_extension_registry::ExtensionPackage,
        resolved: Option<&ironclaw_extension_registry::ResolvedExtensionManifest>,
    ) -> Option<Result<(), ironclaw_assistant::ProductSurfaceFailure>> {
        Some(
            self.extension_management
                .publish_bundled_package_for_test(package, resolved)
                .await
                .map_err(ironclaw_assistant::ProductSurfaceFailure::from),
        )
    }

    #[cfg(feature = "test-support")]
    pub async fn standalone_active_extension_authority_for_test(
        &self,
        grantee: &ExtensionId,
    ) -> Option<
        Result<
            crate::factory::ActiveExtensionAuthorityForTest,
            ironclaw_assistant::ProductSurfaceFailure,
        >,
    > {
        Some(
            crate::factory::active_extension_authority_for_test(
                &self.extension_management,
                grantee,
            )
            .await,
        )
    }

    /// Test-only accessor for the composition-owned trigger repository so
    /// integration tests can seed `TriggerRecord` rows that the spawned
    /// trigger poller will observe. Gated behind `test-support` so the
    /// substrate handle never leaks into production builds. Mirrors the read
    /// path exercised by the spawned trigger poller worker, which calls
    /// `TriggerRepository::list_due_triggers` on every tick and the
    /// per-trigger `claim_due_fire` / `mark_fire_*` mutation methods.
    #[cfg(any(test, feature = "test-support"))]
    pub fn trigger_repository(&self) -> Arc<dyn ironclaw_triggers::TriggerRepository> {
        Arc::clone(&self.trigger_repository)
    }

    /// Test-only accessor for the SAME `ConversationActorPairingService`
    /// instance the spawned trigger poller's
    /// [`ConversationContentRefMaterializer`] consults. Integration tests
    /// use this to call the production `pair_external_actor` API and seed
    /// the trigger creator's actor pairing — without it, the materializer
    /// fails closed with `BindingRequired` (by design: trigger fires never
    /// auto-pair unknown actors). Returns `None` when the trigger poller
    /// wasn't built for this runtime (poller disabled). Gated behind
    /// `test-support` so the conversation handle never leaks into
    /// production builds.
    #[cfg(any(test, feature = "test-support"))]
    pub fn trigger_conversation_pairing(
        &self,
    ) -> Option<Arc<dyn ironclaw_conversations::ConversationActorPairingService>> {
        self.trigger_conversation_pairing.as_ref().map(Arc::clone)
    }

    /// Test-only accessor for the admin user directory the product surface wires.
    /// Mirrors the production call `runtime.product_surface` makes to
    /// [`Self::reborn_user_directory`] (`pub(crate)`), which integration tests
    /// in a separate crate cannot reach. Gated behind `test-support` so the
    /// substrate handle never leaks into production builds. For tests only.
    #[cfg(any(test, feature = "test-support"))]
    pub fn reborn_user_directory_for_tests(
        &self,
    ) -> Arc<dyn ironclaw_identity::RebornUserDirectory> {
        self.reborn_user_directory()
    }

    /// Test-only accessor for the session thread service shared by the trigger
    /// poller, REPL, and WebUI paths. Integration tests use this to enumerate
    /// threads stored by `record_trigger_prompt` without going through the WebUI
    /// `/api/webchat/v2/threads` endpoint (which filters automation threads out
    /// of the list response). The returned handle is the same `Arc` the
    /// production code uses; writes made through it are visible to all paths.
    #[cfg(any(test, feature = "test-support"))]
    pub fn session_thread_service(&self) -> Arc<dyn ironclaw_threads::SessionThreadService> {
        Arc::clone(&self.thread_service)
    }

    /// The runtime's turn coordinator — the same `Arc` production wiring hands
    /// to the product surface and the channel hosts
    /// ([`RebornRuntime::product_turn_coordinator`]) — so downstream integration
    /// tests can poll `GetRunStateRequest` for runs submitted through the
    /// composed surfaces (e.g. waiting on a `BlockedAuth` park and its resume).
    /// For tests only — ships zero bytes in production builds.
    #[cfg(any(test, feature = "test-support"))]
    pub fn product_turn_coordinator_for_test(&self) -> Arc<dyn TurnCoordinator> {
        self.product_turn_coordinator()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn register_outbound_delivery_target_provider(
        &self,
        provider_key: impl Into<String>,
        provider: Arc<dyn OutboundDeliveryTargetProvider>,
    ) -> Result<OutboundDeliveryTargetRegistrationOutcome, RebornRuntimeError> {
        let Some(registry) = self.outbound_delivery_target_registry.as_ref() else {
            return Err(RebornRuntimeError::InvalidArgument {
                reason: "outbound delivery target registry unavailable for this runtime"
                    .to_string(),
            });
        };
        registry
            .register_provider(provider_key, provider)
            .map_err(|error| RebornRuntimeError::InvalidArgument {
                reason: format!("outbound delivery target provider registration failed: {error}"),
            })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn register_static_outbound_delivery_target_for_test(
        &self,
        provider_key: impl Into<String>,
        target_id: RebornOutboundDeliveryTargetId,
        channel: &str,
        display_name: &str,
        description: Option<&str>,
        reply_target_binding_ref: ReplyTargetBindingRef,
    ) -> Result<(), RebornRuntimeError> {
        let target_id = OutboundDeliveryTargetId::new(target_id.as_str()).map_err(|error| {
            RebornRuntimeError::InvalidArgument {
                reason: format!("invalid outbound delivery target id: {error}"),
            }
        })?;
        let summary = OutboundDeliveryTargetSummary::new(
            target_id,
            channel,
            display_name,
            description.map(ToOwned::to_owned),
        )
        .map_err(|error| RebornRuntimeError::InvalidArgument {
            reason: format!("invalid outbound delivery target summary: {error}"),
        })?;
        self.register_outbound_delivery_target_provider(
            provider_key,
            Arc::new(StaticOutboundDeliveryTargetProvider {
                summary,
                capabilities: DeliveryTargetCapabilities {
                    final_replies: true,
                    progress: false,
                    gate_prompts: false,
                    auth_prompts: false,
                    // A registered channel DM is both a final-reply target and a
                    // notification channel, mirroring the generic channel
                    // provider's `full_capabilities`.
                    notifications: true,
                    modalities: Vec::new(),
                },
                reply_target_binding_ref,
            }),
        )
        .map(|_| ())
    }

    /// Test-only: enable the global auto-approve switch for this runtime's
    /// actor scope so a scripted turn exercises the dispatch path instead of
    /// blocking on the per-tool approval gate. The Tools-settings switch is
    /// authoritative for first-party tool dispatch; turning it on here
    /// mirrors what an operator would do before letting the agent run tools.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn enable_global_auto_approve_for_test(&self, conversation: &ConversationId) {
        let store = Arc::clone(&self.auto_approve_settings)
            as Arc<dyn ironclaw_approvals::AutoApproveSettingStorePort>;
        let scope = self.turn_scope_for(&conversation.0).to_resource_scope();
        store
            .set(ironclaw_approvals::AutoApproveSettingInput {
                updated_by: ironclaw_host_api::scope::Principal::User(scope.user_id.clone()),
                scope,
                enabled: true,
            })
            .await
            .expect("enabling global auto-approve should succeed");
    }

    /// Like [`Self::wait_for_terminal`], but also returns when the run parks on
    /// a user-/client-resolvable gate (auth/approval/resource/external-tool)
    /// instead of polling until those non-terminal states either resolve or hit
    /// `RunTimeout`.
    /// `BlockedDependentRun` is deliberately excluded — it is an internal wait
    /// on a child run, not facade-resolvable, so it keeps polling. The returned
    /// state carries the `Blocked*` status and
    /// `gate_ref`; the caller decides whether to resolve (through the WebUI
    /// facade) or stop. Test/recording-support only.
    #[cfg(any(test, feature = "test-support"))]
    async fn wait_for_terminal_or_gate(
        &self,
        scope: &TurnScope,
        run_id: TurnRunId,
        cancellation: &CancellationToken,
    ) -> Result<TurnRunState, RebornRuntimeError> {
        let start = std::time::Instant::now();
        loop {
            if self.turn_scheduler.is_stopped() {
                return Err(RebornRuntimeError::WorkerStopped);
            }
            let state = self
                .turn_coordinator
                .get_run_state(GetRunStateRequest {
                    scope: scope.clone(),
                    run_id,
                })
                .await?;
            // Exhaustive on purpose: a new `TurnStatus` variant must force a
            // compile error here rather than silently defaulting to "not a
            // gate". Only the user-/client-resolvable gates
            // (auth/approval/resource/external-tool) short-circuit recording.
            // `BlockedDependentRun` is an internal wait on a child run (the
            // upstream contract names it `AwaitDependentRun`) — it is not
            // resolvable through the gate facade, so it keeps polling like
            // `Queued`/`Running` until the dependent run completes or the poll
            // budget expires.
            let blocked_on_gate = match state.status {
                TurnStatus::BlockedApproval
                | TurnStatus::BlockedAuth
                // External-tool gates are resolved by the API client submitting
                // tool output, not by the runtime — short-circuit the wait and
                // return the parked state instead of polling forever.
                | TurnStatus::BlockedExternalTool
                | TurnStatus::BlockedResource => true,
                TurnStatus::BlockedDependentRun
                | TurnStatus::Queued
                | TurnStatus::Running
                | TurnStatus::CancelRequested
                | TurnStatus::Cancelled
                | TurnStatus::Completed
                | TurnStatus::Failed
                | TurnStatus::RecoveryRequired => false,
            };
            if state.status.is_terminal() || blocked_on_gate {
                return Ok(state);
            }
            if start.elapsed() > self.poll_settings.max_total {
                // Surface the primary `RunTimeout`; a failure of the secondary
                // cancel is logged with a sanitized id only and must not mask
                // it (see error-handling.md). `debug!` not `warn!` per the
                // logging rule — this runtime is REPL/TUI-reachable.
                if self
                    .cancel_run(
                        scope,
                        run_id,
                        SanitizedCancelReason::Timeout,
                        "timeout-cancel",
                    )
                    .await
                    .is_err()
                {
                    tracing::debug!(run_id = %run_id, "failed to cancel run after recorder timeout");
                }
                return Err(RebornRuntimeError::RunTimeout {
                    timeout: self.poll_settings.max_total,
                });
            }
            tokio::select! {
                _ = cancellation.cancelled() => {
                    if self
                        .cancel_run(
                            scope,
                            run_id,
                            SanitizedCancelReason::UserRequested,
                            "caller-cancel",
                        )
                        .await
                        .is_err()
                    {
                        tracing::debug!(run_id = %run_id, "failed to cancel run after caller cancellation");
                    }
                    return Err(RebornRuntimeError::OperationCancelled);
                }
                _ = tokio::time::sleep(self.poll_settings.interval) => {}
            }
        }
    }

    /// Test/recording-support sibling of [`Self::send_user_message`] that
    /// returns when the run first reaches a terminal status *or* parks on a
    /// `Blocked*` gate, rather than waiting only for a terminal status.
    ///
    /// The QA-trace recorder (`tests/support/reborn/qa_trace.rs`) uses this so
    /// an OAuth/approval-gated phrase records the agent's decisions up to the
    /// gate and reports the pause, instead of sitting in the non-terminal
    /// `BlockedAuth` state until `RunTimeout` (a real recorder hang this method
    /// exists to eliminate). This method only *observes* where the run paused;
    /// gate *resolution* stays on the WebUI `ProductSurface` facade
    /// (`resolve_gate`) per the #3094 seam — do not add a resolution path here.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn send_user_message_until_gate(
        &self,
        conversation: &ConversationId,
        text: &str,
    ) -> Result<RebornTurnDriveOutcome, RebornRuntimeError> {
        let cancellation = CancellationToken::new();
        let submitted = self
            .submit_user_turn(conversation, text, &cancellation, false)
            .await?;

        let outcome = async {
            let state = self
                .wait_for_terminal_or_gate(&submitted.scope, submitted.run_id, &cancellation)
                .await?;
            let assistant_text = self
                .read_latest_assistant_text(&conversation.0, submitted.run_id)
                .await?;

            if state.status.is_terminal() {
                Ok(RebornTurnDriveOutcome::Terminal(AssistantReply {
                    conversation: conversation.clone(),
                    run_id: submitted.run_id,
                    status: state.status,
                    failure_category: state
                        .failure
                        .as_ref()
                        .map(|failure| failure.category().to_string()),
                    text: assistant_text,
                }))
            } else {
                // `wait_for_terminal_or_gate` only returns terminal or a
                // user-resolvable gate (auth/approval/resource). The
                // blocked-reason contract guarantees a `gate_ref` for those, so
                // a missing one is an invariant violation — surface it as an
                // error rather than letting it look like a valid outcome.
                let gate_ref = state.gate_ref.clone().ok_or_else(|| {
                    RebornRuntimeError::TurnSubmission(format!(
                        "run parked on {:?} without a gate ref",
                        state.status
                    ))
                })?;
                Ok(RebornTurnDriveOutcome::BlockedOnGate {
                    run_id: submitted.run_id,
                    status: state.status,
                    gate_ref,
                    partial_text: assistant_text,
                })
            }
        }
        .await;

        // Clearing the accepted message is safe even on the `BlockedOnGate`
        // path, where the run is still live and resumable: the inbound message
        // is already consumed during the first prompt build (the skill-context
        // source `take`s it), so this is idempotent cleanup of an
        // already-taken entry, and a later gate-resume rebuilds from the active
        // plan candidates rather than this entry. The QA recorder also discards
        // the runtime immediately after, so nothing resumes here in practice.
        if let Some(skill_activation_source) = &self.skill_activation_source
            && let Err(clear_error) = skill_activation_source
                .clear_accepted_message(&submitted.scope, &submitted.accepted_message_ref)
        {
            if outcome.is_ok() {
                // Primary turn succeeded, so the cleanup failure is the only
                // error to surface.
                return Err(RebornRuntimeError::TurnSubmission(clear_error.to_string()));
            }
            // Primary turn already failed: don't mask it with the cleanup
            // error — log the secondary (sanitized id only) and return the
            // primary. See error-handling.md.
            tracing::debug!(
                accepted_message_ref = submitted.accepted_message_ref.as_str(),
                "failed to clear accepted message after primary turn failure"
            );
        }

        outcome
    }
}
