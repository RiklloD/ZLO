//! Builder-style port wiring for `RebornServices` (`with_*`). Part of the
//! `dispatch` charter row: these are `RebornServices` inherent methods, so the
//! map charts them with the type, not by file.

use super::*;

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    /// Override the process-local diagnostic store used by the operator
    /// inspector. Capture adapters and this read surface must share one store.
    pub fn with_diagnostic_store(
        mut self,
        diagnostic_store: Arc<dyn crate::inspector_store::DiagnosticStorePort>,
    ) -> Self {
        self.diagnostic_store = diagnostic_store;
        self
    }

    pub fn with_event_stream(mut self, event_stream: Arc<dyn ProjectionStream>) -> Self {
        self.event_stream = Some(event_stream);
        self
    }

    pub fn with_suggestions(
        mut self,
        store: Arc<dyn crate::suggestions_store::SuggestionsStore>,
        unbound: Arc<crate::unbound_turn::UnboundTurnService>,
    ) -> Self {
        self.suggestions = Some(SuggestionsServices { store, unbound });
        self
    }

    pub fn with_input_enqueue(mut self, input_enqueue: Arc<dyn HostInputEnqueuePort>) -> Self {
        self.input_enqueue = input_enqueue;
        self
    }

    /// Wire the port that lands inbound attachment bytes into project storage.
    /// Without it, a send-message carrying attachments is rejected rather than
    /// silently dropping the files.
    pub fn with_inbound_attachments(
        mut self,
        inbound_attachments: Arc<dyn InboundAttachmentLander>,
    ) -> Self {
        self.inbound_attachments = Some(inbound_attachments);
        self
    }

    /// Wire the read-only project-filesystem port backing directory listing and
    /// file download. Without it, the `list_project_dir` / `stat_project_path` /
    /// `read_project_file` methods report the service unavailable.
    pub fn with_project_filesystem_reader(
        mut self,
        project_filesystem: Arc<dyn ProjectFilesystemReader>,
    ) -> Self {
        self.project_filesystem = Some(project_filesystem);
        self
    }

    /// Wire the read-only multi-mount browse port backing the standalone
    /// filesystem viewer (memory / workspace files / skills). Without it,
    /// `list_fs_mounts` reports no mounts and the `browse_fs_dir` /
    /// `stat_fs_path` / `read_fs_file` methods report the service unavailable.
    pub fn with_filesystem_browser(
        mut self,
        filesystem_browser: Arc<dyn FilesystemBrowseReader>,
    ) -> Self {
        self.filesystem_browser = Some(filesystem_browser);
        self
    }

    /// Wire the project management + membership (ACL) port. Without it, the
    /// `list_projects` / `create_project` / … methods report the service
    /// unavailable.
    pub fn with_project_service(mut self, project_service: Arc<dyn ProjectService>) -> Self {
        self.project_service = Some(project_service);
        self
    }

    /// Wire the port that reads landed attachment bytes back for the WebUI bytes
    /// endpoint. Without it, `read_attachment` reports the bytes unavailable
    /// (the timeline still renders the attachment card from its ref).
    pub fn with_inbound_attachment_reader(
        mut self,
        reader: Arc<dyn InboundAttachmentReader>,
    ) -> Self {
        self.inbound_attachment_reader = Some(reader);
        self
    }

    pub fn with_llm_config_service(mut self, llm_config: Arc<dyn LlmConfigService>) -> Self {
        self.llm_config = Some(llm_config);
        self
    }

    pub fn with_ironhub_link_service(mut self, ironhub_link: Arc<dyn IronhubLinkService>) -> Self {
        self.ironhub_link = Some(ironhub_link);
        self
    }

    /// Wire the read-only port exposing the runtime's live active/default model
    /// id. Without it, `get_run_state` cannot price a default-model run (one
    /// submitted without an explicit `model`, so it carries no
    /// `resolved_model_route`): such a run reports token `usage` but no `cost`.
    pub fn with_active_model_reader(
        mut self,
        active_model_reader: Arc<dyn ActiveModelReader>,
    ) -> Self {
        self.active_model_reader = Some(active_model_reader);
        self
    }

    /// Wire the run-scoped external-tool catalog the loop parks native
    /// `ask_user` calls in, so a question gate can be answered.
    pub fn with_external_tool_catalog(
        mut self,
        catalog: Arc<dyn ironclaw_turns::ExternalToolCatalog>,
    ) -> Self {
        self.external_tool_catalog = Some(catalog);
        self
    }

    pub fn with_operator_approval_config(
        mut self,
        overrides: Arc<dyn CapabilityPermissionOverrideStorePort>,
        auto_approve: Arc<dyn AutoApproveSettingStorePort>,
        persistent_policies: Arc<dyn PersistentApprovalPolicyStorePort>,
        tool_catalog: Arc<dyn RebornOperatorToolCatalog>,
    ) -> Self {
        self.operator_approval_config = Some(RebornOperatorApprovalConfig {
            overrides,
            auto_approve,
            persistent_policies,
            tool_catalog,
        });
        self
    }

    pub fn with_lifecycle_product_service(
        mut self,
        lifecycle_service: Arc<dyn LifecycleProductService>,
    ) -> Self {
        self.lifecycle_service = lifecycle_service;
        self
    }

    pub fn with_automation_product_service(
        mut self,
        automation_service: Arc<dyn AutomationProductService>,
    ) -> Self {
        self.automation_service = automation_service;
        self
    }

    pub fn with_skills_product_service(
        mut self,
        skills_service: Arc<dyn SkillsProductService>,
    ) -> Self {
        self.skills_service = skills_service;
        self
    }

    pub fn with_channel_connection_service(
        mut self,
        channel_connection_service: Arc<dyn ChannelConnectionService>,
    ) -> Self {
        self.channel_connection_service = channel_connection_service;
        self
    }

    pub fn with_outbound_preferences_product_service(
        mut self,
        outbound_preferences_service: Arc<dyn OutboundPreferencesProductService>,
    ) -> Self {
        self.outbound_preferences_service = outbound_preferences_service;
        self
    }

    pub fn with_notification_setup_service(
        mut self,
        notification_setup_service: Arc<dyn ChannelNotificationSetupService>,
    ) -> Self {
        self.notification_setup_service = notification_setup_service;
        self
    }

    pub fn with_notification_inbox(
        mut self,
        notification_inbox: Arc<dyn NotificationInboxStorePort>,
    ) -> Self {
        self.notification_inbox = notification_inbox;
        self
    }

    /// Wire the generic channel-config configure port. Without it, the
    /// setup service renders no channel-config fields and rejects
    /// channel-config submissions as unavailable.
    pub fn with_channel_config_product_service(
        mut self,
        channel_config_service: Arc<dyn ChannelConfigProductService>,
    ) -> Self {
        self.channel_config_service = Some(channel_config_service);
        self
    }

    pub fn with_operator_status_service(
        mut self,
        operator_status: Arc<dyn OperatorStatusService>,
    ) -> Self {
        self.operator_status = operator_status;
        self
    }

    pub fn with_operator_logs_service(
        mut self,
        operator_logs: Arc<dyn OperatorLogsService>,
    ) -> Self {
        self.operator_logs = operator_logs;
        self
    }

    pub fn with_operator_service_lifecycle_service(
        mut self,
        operator_service_lifecycle: Arc<dyn OperatorServiceLifecycleService>,
    ) -> Self {
        self.operator_service_lifecycle = operator_service_lifecycle;
        self
    }

    pub fn with_approval_interactions(
        mut self,
        approval_interactions: Arc<dyn ApprovalInteractionService>,
    ) -> Self {
        self.approval_interactions = approval_interactions;
        self
    }

    pub fn with_auth_interactions(
        mut self,
        auth_interactions: Arc<dyn AuthInteractionService>,
    ) -> Self {
        self.auth_interactions = auth_interactions;
        self
    }

    pub fn with_extension_credentials(
        mut self,
        extension_credentials: Arc<dyn ExtensionCredentialSetupService>,
    ) -> Self {
        self.extension_credentials = Some(extension_credentials);
        self
    }

    /// Wire the admin user-management port (user CRUD + per-user secret
    /// provisioning). Without it, every admin service method reports the service
    /// unavailable via the fail-closed [`RejectingAdminUserService`] default.
    pub fn with_admin_user_service(mut self, admin_users: Arc<dyn AdminUserService>) -> Self {
        self.admin_users = admin_users;
        self
    }

    /// Wire the deployment's session-channel directory so channel-
    /// parameterized session submissions can be validated. Without it, a
    /// submission naming an extension fails closed as service-unavailable.
    /// Submissions without an extension identity are always rejected.
    pub fn with_session_channel_directory(
        mut self,
        directory: Arc<dyn ironclaw_product_contracts::session_ingress::SessionChannelDirectory>,
    ) -> Self {
        self.session_channels = Some(directory);
        self
    }

    /// Swap the session-lane inbound idempotency ledger. Production wires the
    /// durable filesystem ledger (`build_session_inbound_ledger`); the default
    /// is process-local, for standalone/tests only.
    pub fn with_session_inbound_ledger(
        mut self,
        ledger: Arc<dyn crate::ledger::IdempotencyLedger>,
    ) -> Self {
        self.session_inbound_ledger = ledger;
        self
    }

    pub fn with_skill_activation_hooks<R, C>(mut self, recorder: R, clearer: C) -> Self
    where
        R: Fn(&TurnScope, &AcceptedMessageRef, &str) -> Result<(), ProductSurfaceError>
            + Send
            + Sync
            + 'static,
        C: Fn(&TurnScope, &AcceptedMessageRef) -> Result<(), ProductSurfaceError>
            + Send
            + Sync
            + 'static,
    {
        self.skill_activation_recorder = Some(Arc::new(recorder));
        self.skill_activation_clearer = Some(Arc::new(clearer));
        self
    }
}
