//! `RebornRuntime` service handles: the production accessors callers wire
//! into WebUI, channels, and the CLI, plus `shutdown`.

use super::*;

use super::assembly_support::build_generic_channel_connection_facade;

impl RebornRuntime {
    /// The deployment's authenticated-session channel extension id, when
    /// exactly one is declared. The serve path advertises it to the SPA.
    pub fn session_channel_extension_id(&self) -> Option<&str> {
        self.session_channel_extension_id.as_deref()
    }

    pub fn readiness(&self) -> &RebornReadiness {
        &self.readiness
    }

    /// Build the canonical product surface over this runtime graph.
    ///
    /// The returned surface reuses this runtime's thread service, turn
    /// coordinator, projection stream, product-auth services, lifecycle/admin
    /// ports, and product capability invoker. Consumers should use this handle
    /// instead of assembling product-facing services from runtime internals.
    pub fn product_surface(
        &self,
        event_stream: Option<Arc<dyn ProjectionStream>>,
    ) -> Result<Arc<dyn ProductSurface>, RebornBuildError> {
        let channel_connection = self.generic_channel_connection_facade();
        crate::product_surface::build_product_surface_with_channel_connection(
            self,
            event_stream,
            channel_connection,
            Vec::new(),
        )
    }

    pub(crate) fn ironhub_link_service(
        &self,
    ) -> Option<Arc<dyn ironclaw_product_contracts::ironhub::IronhubLinkService>> {
        self.ironhub_link_service.as_ref().map(Arc::clone)
    }

    /// Build the public registration mount from the same optional service
    /// attached to the product facade. `None` is the default-off gate.
    pub fn ironhub_register_route_mount(
        &self,
    ) -> Result<Option<ironclaw_host_ingress::PublicRouteMount>, RebornBuildError> {
        self.ironhub_link_service()
            .map(|service| {
                crate::ironhub_link_serve::ironhub_register_route_mount(
                    crate::ironhub_link_serve::IronhubRegisterRouteState::new(service),
                )
            })
            .transpose()
    }

    /// The loopback MCP route for CLI agents (orchestrator tools), serving on
    /// `listener`. `None` off libSQL, where there is no engine.
    pub fn cli_bridge_route_mount(
        &self,
        listener: std::net::SocketAddr,
    ) -> Result<Option<ironclaw_host_ingress::PublicRouteMount>, RebornBuildError> {
        let (Some(engine), Some(bridge)) = (&self.orchestrator, &self.cli_bridge) else {
            return Ok(None);
        };
        crate::cli_bridge_serve::cli_bridge_route_mount(
            crate::cli_bridge_serve::CliBridgeRouteState::new(
                Arc::clone(bridge),
                Arc::clone(engine),
            ),
            listener,
        )
        .map(Some)
    }

    pub fn product_auth_services(&self) -> Arc<RebornProductAuthServices> {
        Arc::clone(&self.product_auth)
    }

    pub fn extension_ingress_parts(
        &self,
    ) -> Option<ironclaw_extension_host::extension_ingress::ExtensionIngressParts> {
        self.extension_ingress.clone()
    }

    pub(crate) fn secret_store(&self) -> Arc<dyn SecretStorePort> {
        Arc::clone(&self.secret_store)
    }

    /// The product-side workflow factory the production channel host builds
    /// every per-extension graph through (§12.11 D-A) — `None` when the
    /// composed profile has no channel host at all.
    pub fn channel_workflow_factory(
        &self,
    ) -> Option<Arc<ironclaw_assistant::RebornChannelWorkflowFactory>> {
        self.channel_workflow_factory.clone()
    }

    pub(super) fn read_write_workspace_filesystem(
        &self,
    ) -> Option<Arc<ScopedFilesystem<CompositeRootFilesystem>>> {
        crate::runtime_mounts::read_write_workspace_filesystem(
            &self.extension_filesystem,
            &self.workspace_mount_policy,
        )
    }

    /// Seed a bare `secret_handle` secret for an owner scope so keyed
    /// capabilities (network + `use_secret`) can resolve their
    /// `InjectSecretOnce` obligation. `serve` uses this to write the value of
    /// an `IRONCLAW_REBORN_DEV_SECRET__<handle>` env var into the tenant-shared
    /// admin-managed scope, so one operator-provisioned key serves every user of
    /// the tenant (SSO users included) without per-user provisioning. The secret
    /// store is composition-private, so this is the single narrow write seam.
    pub async fn seed_standalone_secret(
        &self,
        owner: ResourceScope,
        handle: ironclaw_host_api::ids::SecretHandle,
        secret_value: String,
    ) -> Result<(), ironclaw_secrets::SecretStoreError> {
        self.secret_store
            .put(
                owner,
                handle,
                ironclaw_secrets::SecretMaterial::from(secret_value),
                None,
            )
            .await
            .map(|_| ())
    }

    pub(crate) fn webui_tenant_id(&self) -> &TenantId {
        &self.thread_scope.tenant_id
    }

    /// Operator boot config, when the runtime was assembled with one. The
    /// product surface uses it to compose the LLM-config settings service.
    pub(crate) fn webui_boot_config(&self) -> Option<&ironclaw_config::RebornBootConfig> {
        self.boot.as_ref()
    }

    /// The runtime's NEAR AI session manager, when an LLM seam is wired. The
    /// LLM-config service uses it so a completed NEAR AI login applies to the
    /// live provider on reload.
    pub(crate) fn webui_llm_session(&self) -> Option<Arc<ironclaw_llm::SessionManager>> {
        self.llm_reload
            .as_ref()
            .map(|parts| Arc::clone(&parts.session))
    }

    /// Shared NEAR AI login-state store. The authenticated start endpoint
    /// issues states and the public callback consumes them.
    pub(crate) fn webui_nearai_login_states(
        &self,
    ) -> Option<Arc<ironclaw_operator::llm_admin::llm_config_service::NearAiLoginStateStore>> {
        self.llm_reload
            .as_ref()
            .map(|parts| Arc::clone(&parts.nearai_login_states))
    }

    /// Public NEAR AI login callback mount for the host ingress to merge via
    /// `ironclaw_webui::WebuiServeConfig::with_public_route_mount`. Built
    /// from the runtime's private session/reload/boot so those stay internal.
    /// `None` when no LLM seam or boot config was wired.
    pub fn nearai_login_callback_mount(
        &self,
    ) -> Result<Option<ironclaw_host_ingress::PublicRouteMount>, crate::RebornBuildError> {
        let Some(boot) = self.boot.clone() else {
            return Ok(None);
        };
        let Some(session) = self.webui_llm_session() else {
            return Ok(None);
        };
        let Some(reload) = self.webui_llm_reload_trigger() else {
            return Ok(None);
        };
        let Some(states) = self.webui_nearai_login_states() else {
            return Ok(None);
        };
        // Called through operator's crate-root facade: operator now returns the
        // host-owned `PublicRouteMount`, so the composition-side repackaging
        // shim that existed only to convert an operator-local carrier is gone.
        Ok(Some(ironclaw_operator::nearai_login_callback_mount(
            session, reload, boot, states,
        )?))
    }

    /// Live LLM-provider reload trigger for the settings service. Returns the
    /// hot-swap adapter when an LLM provider was wired at boot; otherwise
    /// `None`, in which case config edits persist to disk and apply on the
    /// next restart.
    pub(crate) fn webui_llm_reload_trigger(
        &self,
    ) -> Option<Arc<dyn ironclaw_operator::LlmReloadTrigger>> {
        let boot = self.boot.as_ref()?;
        let parts = self.llm_reload.as_ref()?;
        Some(Arc::new(ironclaw_operator::RebornLlmReloadAdapter::new(
            boot.clone(),
            Arc::clone(&parts.reload_handle),
            Arc::clone(&parts.session),
            ironclaw_operator::LlmKeyStore::new(crate::RuntimeOperatorSecretValueStore::shared(
                self.secret_store(),
            )),
        )))
    }

    /// Read-only reader exposing the live active/default model id so the WebUI
    /// facade can price a default-model run (one with no `resolved_model_route`)
    /// against the model that actually ran. Backed by the same hot-swappable
    /// primary provider the model gateway drives, so it tracks operator model
    /// swaps. `None` when no LLM provider was wired at boot.
    pub(crate) fn webui_active_model_reader(&self) -> Option<Arc<dyn ActiveModelReader>> {
        let parts = self.llm_reload.as_ref()?;
        Some(Arc::new(ironclaw_operator::ProviderActiveModelReader::new(
            parts.reload_handle.primary_provider(),
        )))
    }

    /// Configured providers a native thread can switch to (the model
    /// picker's extra groups); `None` when no LLM provider was wired at boot.
    pub(crate) fn native_model_routes(
        &self,
    ) -> Option<Arc<dyn ironclaw_product_contracts::operator_llm::NativeModelRoutes>> {
        let routes = self.llm_reload.as_ref()?.provider_routes.clone()?;
        Some(routes as _)
    }

    /// Diagnostic id for the no-profile run profile selected by this runtime.
    pub fn default_run_profile_id(&self) -> &str {
        &self.default_run_profile_id
    }

    /// Open the SSO/admin identity resolver over the host-owned identity substrate.
    /// A built `RebornRuntime` always carries the canonical filesystem-backed
    /// scoped substrate; callers should not synthesize a second identity store.
    /// See #5013.
    pub async fn open_reborn_identity_resolver(
        &self,
        _tenant_id: &TenantId,
    ) -> Result<
        Arc<dyn ironclaw_identity::RebornIdentityResolver>,
        ironclaw_identity::RebornIdentityError,
    > {
        let store = filesystem_reborn_identity_store(
            Arc::clone(&self.scoped_filesystem),
            self.thread_scope.tenant_id.clone(),
            self.actor_user_id.clone(),
            self.thread_scope.agent_id.clone(),
            self.thread_scope.project_id.clone(),
        );
        Ok(store)
    }

    /// Open the remote-access records (environment id, pairing codes, device
    /// grants) over the host-owned identity substrate, creating the permanent
    /// environment id with `default_label` on first start. Same control-plane
    /// mount as the identity resolver, so revocations survive a restart.
    pub async fn open_environment_access(
        &self,
        default_label: &str,
    ) -> Result<
        Arc<dyn ironclaw_product_contracts::environment_access::EnvironmentAccessService>,
        ironclaw_product_contracts::environment_access::EnvironmentAccessError,
    > {
        let access = ironclaw_identity::environment_access::FilesystemEnvironmentAccess::new(
            Arc::clone(&self.scoped_filesystem),
            self.thread_scope.tenant_id.clone(),
            self.actor_user_id.clone(),
            self.thread_scope.agent_id.clone(),
            self.thread_scope.project_id.clone(),
        );
        access.ensure_environment(default_label).await?;
        Ok(Arc::new(access))
    }

    /// Open the admin user-directory surface over the host-owned identity
    /// substrate. Same store [`open_reborn_identity_resolver`] uses
    /// (`RebornIdentityStore` implements both traits), so admin CRUD
    /// enumerates exactly the users SSO login persists. Synchronous and fold-free
    /// (the legacy fold seeds identity/index records, not `StoredUser` rows the
    /// directory reads), so `runtime.product_surface` can call it directly.
    pub(crate) fn reborn_user_directory(&self) -> Arc<dyn ironclaw_identity::RebornUserDirectory> {
        filesystem_reborn_identity_store(
            Arc::clone(&self.scoped_filesystem),
            self.thread_scope.tenant_id.clone(),
            self.actor_user_id.clone(),
            self.thread_scope.agent_id.clone(),
            self.thread_scope.project_id.clone(),
        )
    }

    /// Admin per-user secret provisioner over the host-owned secret substrate,
    /// scoped to an arbitrary target user (not the runtime owner). See
    /// `admin_secrets.rs`.
    pub(crate) fn reborn_admin_secret_provisioner(
        &self,
    ) -> Arc<dyn ironclaw_assistant::AdminSecretProvisioner> {
        Arc::clone(&self.admin_secret_provisioner)
    }

    /// First-class projects + membership (ACL) facade over the host-owned scoped
    /// substrate, backing the WebUI project surface.
    pub(crate) fn reborn_project_service(
        &self,
    ) -> Arc<dyn ironclaw_product_contracts::project_service::ProjectService> {
        Arc::clone(&self.project_service)
    }

    /// The admin API-token minter supplied via
    /// [`RebornRuntimeInput::with_admin_api_token_minter`], if any.
    pub(crate) fn reborn_admin_token_minter(
        &self,
    ) -> Option<Arc<dyn ironclaw_product_contracts::admin_users::AdminApiTokenMinter>> {
        self.admin_api_token_minter.clone()
    }

    pub(crate) fn product_thread_service(&self) -> Arc<dyn SessionThreadService> {
        self.thread_service.clone()
    }

    pub(crate) fn suggestions_store(&self) -> Arc<dyn ironclaw_assistant::SuggestionsStore> {
        Arc::clone(&self.suggestions_store)
    }

    pub(crate) fn product_default_thread_scope(&self) -> &ThreadScope {
        &self.thread_scope
    }

    pub(crate) fn product_turn_coordinator(&self) -> Arc<dyn TurnCoordinator> {
        self.turn_coordinator.clone()
    }

    pub(crate) fn webui_input_enqueue(&self) -> Arc<dyn ironclaw_loop_host::HostInputEnqueuePort> {
        Arc::clone(&self.subagent_delivery.input_queue)
            as Arc<dyn ironclaw_loop_host::HostInputEnqueuePort>
    }

    /// The generic post-OAuth channel-identity binding config for this
    /// deployment (extension-runtime §5.5): channel extensions bind through
    /// generic discovery over the durable installation store; bindings
    /// persist in the generic channel-identity store; post-bind DM-target
    /// provisioning opens the caller's direct conversation through the
    /// extension's own adapter. `None` when the composed runtime carries no
    /// durable channel-identity storage.
    /// The composed pairing registry for the bearer-authed generic pairing
    /// routes (`WebGeneratedCode` channels), when the composed runtime built
    /// any pairing service. The binary turns it into a route mount through
    /// `ironclaw_webui::channel_pairing_route_mount` — see
    /// `channel_pairing_registry_for_test` for why the mount is not built
    /// here.
    pub fn channel_pairing_registry(&self) -> Option<Arc<ChannelPairingRegistry>> {
        self.channel_pairing.as_ref().map(Arc::clone)
    }

    pub fn channel_identity_binding_config(
        &self,
    ) -> Option<ironclaw_extension_host::channel_identity_binding::ChannelIdentityBindingConfig>
    {
        let identity_store = self.channel_identity_store.clone();
        let installation_store = Some(self.extension_management.installation_store_handle());
        let snapshot_updates = self
            .extension_management
            .generic_host()
            .map(|host| host.snapshot_watch().subscribe());
        let post_bind_factory = match (
            self.channel_delivery_resolver.clone(),
            Some(self.channel_dm_target_store.clone()),
            snapshot_updates,
        ) {
            (Some(delivery), Some(store), Some(snapshot_updates)) => Some(Arc::new(
                ironclaw_extension_host::channel_dm_provisioning::ChannelDmTargetProvisioning::new(
                    delivery,
                    store,
                    snapshot_updates,
                ),
            )
                as Arc<dyn ironclaw_extension_contracts::channel_identity::ChannelIdentityPostBindFactory>),
            _ => None,
        };
        Some(
            ironclaw_extension_host::channel_identity_binding::ChannelIdentityBindingConfig {
                tenant_id: self.thread_scope.tenant_id.clone(),
                installation_store,
                channel_config: Some(self.channel_config_service.clone()),
                binding_store: Arc::clone(&identity_store)
                    as Arc<dyn ironclaw_host_api::user_identity::RebornUserIdentityBindingStore>,
                rollback_store: identity_store
                    as Arc<
                        dyn ironclaw_host_api::user_identity::RebornUserIdentityBindingDeleteStore,
                    >,
                post_bind_factory,
                overrides: Vec::new(),
            },
        )
    }

    /// The generic per-user channel-connection facade over the same generic
    /// stores (discovery from the installation store; connected = an
    /// identity binding under the extension's installation prefix;
    /// disconnect clears bindings, vendor credentials, and the provisioned
    /// DM target). `None` when the composed runtime carries no durable
    /// channel-identity storage.
    pub(crate) fn generic_channel_connection_facade(
        &self,
    ) -> Option<Arc<dyn ironclaw_auth::ChannelConnectionService>> {
        Some(build_generic_channel_connection_facade(
            self.thread_scope.tenant_id.clone(),
            &self.extension_management,
            &self.channel_identity_store,
            &self.product_auth,
            &self.channel_dm_target_store,
            self.channel_pairing.clone(),
        ))
    }

    pub(crate) fn product_event_stream(&self) -> Arc<dyn ProjectionStream> {
        self.projection_services.product_event_stream()
    }

    pub(crate) fn webui_approval_interaction_service(&self) -> Arc<dyn ApprovalInteractionService> {
        self.approval_interaction_service.clone()
    }

    pub(crate) fn webui_auth_interaction_service(&self) -> Arc<dyn AuthInteractionService> {
        self.auth_interaction_service.clone()
    }

    pub(crate) fn outbound_delivery_target_provider(
        &self,
    ) -> Option<Arc<dyn OutboundDeliveryTargetProvider>> {
        self.outbound_delivery_target_registry
            .as_ref()
            .map(|registry| {
                let registry = Arc::clone(registry);
                let provider: Arc<dyn OutboundDeliveryTargetProvider> = registry;
                provider
            })
    }

    pub(crate) fn webui_skill_activation_source(
        &self,
    ) -> Option<Arc<ComposedSelectableSkillContextSource>> {
        self.skill_activation_source.clone()
    }

    pub(crate) fn webui_tool_mentions(&self) -> Arc<ironclaw_loop_host::TurnToolMentions> {
        Arc::clone(&self.tool_mentions)
    }

    /// Read-write project-scoped workspace filesystem for landing inbound
    /// attachment bytes at paths the agent's file tools can later read back.
    /// `None` when no local runtime is composed.
    ///
    /// This deliberately does NOT reuse `rt.workspace_filesystem`: that handle
    /// is intentionally read-only (it backs setup-marker reads — see
    /// `standalone_setup_marker_workspace_filesystem_is_read_only`), so writing
    /// an attachment through it fails closed with `PermissionDenied`. Delegates
    /// to `RebornRuntimeStores::read_write_workspace_filesystem` — the single owner
    /// of this recipe, shared with the `standalone_attachment_test_support_for_test`
    /// C-ATTACH test seam so the two views can never drift apart.
    pub(crate) fn webui_workspace_filesystem(
        &self,
    ) -> Option<
        Arc<ironclaw_filesystem::ScopedFilesystem<ironclaw_filesystem::CompositeRootFilesystem>>,
    > {
        self.read_write_workspace_filesystem()
    }

    /// Read-only scoped filesystem spanning every mount the standalone WebUI
    /// filesystem viewer can browse (workspace files + persistent memory), over
    /// the same composite root the agent's tools resolve through. `None` only
    /// when no local runtime is composed; scope-specific mount resolution errors
    /// surface during browse operations.
    ///
    /// Distinct from [`Self::webui_workspace_filesystem`]: that handle is the
    /// read-write workspace-only view used to land attachments, whereas this is
    /// a strictly read-only, multi-mount navigation view.
    pub(crate) fn webui_browse_filesystem(
        &self,
    ) -> Option<
        Arc<ironclaw_filesystem::ScopedFilesystem<ironclaw_filesystem::CompositeRootFilesystem>>,
    > {
        let extension_filesystem = &self.extension_filesystem;
        Some(Arc::new(ironclaw_filesystem::ScopedFilesystem::new(
            Arc::clone(extension_filesystem),
            crate::runtime_mounts::scoped_browse_mount_view,
        )))
    }

    /// Broadcast sink that fans every emitted `BudgetEvent` to any
    /// subscriber. The runtime always spawns its own subscriber — the
    /// [`crate::observability::budget_events::BudgetEventProjection`] task wired by
    /// `build_reborn_runtime` and shut down via [`Self::shutdown`] —
    /// so this sink is never a no-op even when the caller does not
    /// install a custom observer (review feedback Thermo-Nuclear #3
    /// / follow-up A2). Callers that need a richer projection
    /// (multi-channel fan-out, telemetry exporters) should pass an
    /// observer through
    /// [`crate::RebornRuntimeInput::with_budget_event_observer`]
    /// rather than re-subscribing here; spawning a second long-lived
    /// receiver risks one of them lagging while the other drains.
    pub fn broadcast_budget_event_sink(
        &self,
    ) -> Option<Arc<ironclaw_resources::BroadcastBudgetEventSink>> {
        Some(Arc::clone(&self.broadcast_budget_event_sink))
    }

    /// Stop the turn-runner worker and the budget-event projection.
    /// Awaits both tasks before returning so background state is fully
    /// drained when the runtime drops.
    pub async fn shutdown(self) -> Result<(), RebornRuntimeError> {
        // Before turns are interrupted: a clean quit holds running threads
        // as interrupted so reopening never auto-continues them.
        if let Some(engine) = &self.orchestrator
            && let Err(error) = engine.prepare_for_shutdown().await
        {
            tracing::warn!(%error, "orchestrator shutdown bookkeeping failed");
        }
        if let Some(trigger_poller) = self.trigger_poller_handle {
            trigger_poller
                .shutdown(TRIGGER_POLLER_SHUTDOWN_TIMEOUT)
                .await;
        }
        if let Some(credential_refresh_worker) = self.credential_refresh_worker_handle {
            credential_refresh_worker
                .shutdown(ironclaw_auth::KEEPALIVE_SWEEP_SHUTDOWN_TIMEOUT)
                .await;
        }
        self.trace_flush_worker.shutdown().await;
        if let Some(skill_learning_extraction_tasks) = self.skill_learning_extraction_tasks {
            skill_learning_extraction_tasks.shutdown().await;
        }
        self.turn_scheduler.shutdown().await;
        // Stop the boot-recovery sweep before its leases are released below.
        if let Some(recovery) = self.reply_publication_recovery {
            recovery.abort();
            let _ = recovery.await; // silent-ok: an aborted join is Cancelled.
        }
        // Hand open reply publications back: their leases are released so
        // a publisher on the next process (or another node) resumes them
        // immediately instead of waiting for the lease to lapse.
        if let Some(coordinator) = &self.delivery_coordinator {
            coordinator.shutdown_reply_publication().await;
        }
        if let Some(projection) = self.budget_event_projection {
            projection.shutdown().await;
        }
        if let Some(process_port) = self.user_sandbox_process_port {
            process_port
                .shutdown()
                .await
                .map_err(RebornRuntimeError::UserSandboxShutdown)?;
        }
        self.runtime_event_sink
            .flush()
            .await
            .map_err(RebornRuntimeError::RuntimeEventFlush)?;
        Ok(())
    }
}
