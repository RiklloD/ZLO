//! Shared product-service doubles (lifecycle, automations, projections, operator, LLM config, admin users).

use crate::*;

pub(crate) struct RecordingLifecycleService {
    pub(crate) package_refs: Mutex<Vec<LifecyclePackageRef>>,
    pub(crate) actions: Mutex<Vec<LifecycleProductAction>>,
    pub(crate) imported_bundles: Mutex<Vec<Vec<u8>>>,
    pub(crate) credential_requirements: Vec<LifecycleExtensionCredentialRequirement>,
    pub(crate) onboarding: Option<LifecycleExtensionOnboarding>,
}

impl RecordingLifecycleService {
    pub(crate) fn new() -> Self {
        Self {
            package_refs: Mutex::new(Vec::new()),
            actions: Mutex::new(Vec::new()),
            imported_bundles: Mutex::new(Vec::new()),
            credential_requirements: Vec::new(),
            onboarding: None,
        }
    }

    pub(crate) fn with_credential_requirements(
        credential_requirements: Vec<LifecycleExtensionCredentialRequirement>,
    ) -> Self {
        Self {
            package_refs: Mutex::new(Vec::new()),
            actions: Mutex::new(Vec::new()),
            imported_bundles: Mutex::new(Vec::new()),
            credential_requirements,
            onboarding: None,
        }
    }

    pub(crate) fn with_credential_requirements_and_onboarding(
        credential_requirements: Vec<LifecycleExtensionCredentialRequirement>,
        onboarding: LifecycleExtensionOnboarding,
    ) -> Self {
        Self {
            package_refs: Mutex::new(Vec::new()),
            actions: Mutex::new(Vec::new()),
            imported_bundles: Mutex::new(Vec::new()),
            credential_requirements,
            onboarding: Some(onboarding),
        }
    }

    pub(crate) fn package_refs(&self) -> Vec<LifecyclePackageRef> {
        self.package_refs.lock().expect("lock").clone()
    }

    pub(crate) fn imported_bundles(&self) -> Vec<Vec<u8>> {
        self.imported_bundles.lock().expect("lock").clone()
    }

    pub(crate) fn actions(&self) -> Vec<LifecycleProductAction> {
        self.actions.lock().expect("lock").clone()
    }

    pub(crate) fn extension_list_payload(
        &self,
        package_ref: &LifecyclePackageRef,
    ) -> Option<LifecycleProductPayload> {
        if self.credential_requirements.is_empty() {
            return None;
        }
        let summary = LifecycleExtensionSummary {
            package_ref: package_ref.clone(),
            name: package_ref.id.as_str().to_string(),
            version: "1.0.0".to_string(),
            description: "test extension".to_string(),
            source: LifecycleExtensionSource::HostBundled,
            runtime_kind: LifecycleExtensionRuntimeKind::FirstParty,
            surface_kinds: Vec::new(),
            channel_directions: None,
            channel_connection: None,
            channel_presentation: None,
            visible_capability_ids: Vec::new(),
            visible_read_only_capability_ids: Vec::new(),
            credential_requirements: self.credential_requirements.clone(),
            onboarding: self.onboarding.clone(),
        };
        Some(LifecycleProductPayload::ExtensionList {
            extensions: vec![LifecycleInstalledExtensionSummary {
                summary,
                phase: InstallationState::Configured,
                install_scope: None,
            }],
            count: 1,
        })
    }
}

#[async_trait]
impl LifecycleProductService for RecordingLifecycleService {
    async fn execute(
        &self,
        _context: LifecycleProductContext,
        action: ironclaw_assistant::LifecycleProductAction,
    ) -> Result<LifecycleProductResponse, ProductSurfaceError> {
        self.actions.lock().expect("lock").push(action.clone());
        match action {
            LifecycleProductAction::ExtensionActivate { package_ref } => {
                Ok(LifecycleProductResponse {
                    package_ref: Some(package_ref),
                    phase: InstallationState::Active,
                    blockers: Vec::new(),
                    message: Some("activated".to_string()),
                    payload: Some(LifecycleProductPayload::ExtensionActivate {
                        activated: true,
                        visible_capability_ids: Vec::new(),
                        connection_required: None,
                    }),
                })
            }
            LifecycleProductAction::ExtensionSelectHostedMcpAuth { package_ref, .. } => {
                Ok(LifecycleProductResponse {
                    package_ref: Some(package_ref),
                    phase: InstallationState::Installed,
                    blockers: Vec::new(),
                    message: Some("hosted MCP authentication selected".to_string()),
                    payload: Some(LifecycleProductPayload::ExtensionActivate {
                        activated: false,
                        visible_capability_ids: Vec::new(),
                        connection_required: None,
                    }),
                })
            }
            LifecycleProductAction::ExtensionRegisterHostedMcp { .. } => {
                Ok(LifecycleProductResponse {
                    package_ref: None,
                    phase: InstallationState::Installed,
                    blockers: vec![LifecycleReadinessBlocker::Setup {
                        ref_id: Some(
                            ironclaw_extension_contracts::lifecycle_id::LifecycleBlockerRef::new(
                                ironclaw_product_contracts::package_lifecycle::HOSTED_MCP_AUTH_SELECTION_BLOCKER_REF,
                            )
                            .expect("valid hosted MCP auth-selection blocker ref"),
                        ),
                    }],
                    message: Some("internal lifecycle diagnostic".to_string()),
                    payload: None,
                })
            }
            other => panic!("unexpected lifecycle action in setup test service: {other:?}"),
        }
    }

    async fn project_package(
        &self,
        _context: LifecycleProductContext,
        package_ref: LifecyclePackageRef,
    ) -> Result<LifecycleProductResponse, ProductSurfaceError> {
        self.package_refs
            .lock()
            .expect("lock")
            .push(package_ref.clone());
        let phase = if self.credential_requirements.is_empty() {
            InstallationState::Unsupported
        } else {
            InstallationState::Configured
        };
        let mut response = LifecycleProductResponse::projection(
            Some(package_ref),
            phase,
            vec![
                LifecycleReadinessBlocker::runtime(Some(
                    "extension_lifecycle_store_unwired".to_string(),
                ))
                .map_err(ProductSurfaceError::internal_from)?,
            ],
        );
        response.payload = self.extension_list_payload(response.package_ref.as_ref().expect("ref"));
        Ok(response)
    }

    async fn import_extension_bundle(
        &self,
        _context: LifecycleProductContext,
        bundle: Vec<u8>,
    ) -> Result<LifecycleProductResponse, ProductSurfaceError> {
        self.imported_bundles.lock().expect("lock").push(bundle);
        Ok(LifecycleProductResponse {
            package_ref: None,
            phase: InstallationState::Installed,
            blockers: Vec::new(),
            message: Some("imported".to_string()),
            payload: None,
        })
    }
}

pub(crate) struct ListingLifecycleService {
    pub(crate) extension: LifecycleInstalledExtensionSummary,
}

#[async_trait]
impl LifecycleProductService for ListingLifecycleService {
    async fn execute(
        &self,
        _context: LifecycleProductContext,
        action: LifecycleProductAction,
    ) -> Result<LifecycleProductResponse, ProductSurfaceError> {
        assert!(matches!(action, LifecycleProductAction::ExtensionList));
        Ok(LifecycleProductResponse {
            package_ref: None,
            phase: self.extension.phase,
            blockers: Vec::new(),
            message: None,
            payload: Some(LifecycleProductPayload::ExtensionList {
                extensions: vec![self.extension.clone()],
                count: 1,
            }),
        })
    }

    async fn project_package(
        &self,
        _context: LifecycleProductContext,
        _package_ref: LifecyclePackageRef,
    ) -> Result<LifecycleProductResponse, ProductSurfaceError> {
        panic!("list_extensions should execute the list action, not project one package")
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ListAutomationCall {
    pub(crate) caller: ProductAgentBoundCaller,
    pub(crate) limit: usize,
    pub(crate) run_limit: usize,
    pub(crate) include_completed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AutomationMutationAction {
    Run,
    Pause,
    Resume,
    Rename { name: AutomationName },
    Delete,
}

#[derive(Debug, Clone)]
pub(crate) struct AutomationMutationCall {
    pub(crate) caller: ProductAgentBoundCaller,
    pub(crate) automation_id: String,
    pub(crate) action: AutomationMutationAction,
}

#[derive(Clone)]
pub(crate) struct StaticAutomationService {
    pub(crate) output: Vec<RebornAutomationInfo>,
    pub(crate) list_error: Option<ProductSurfaceError>,
    pub(crate) scheduler_enabled: bool,
    pub(crate) list_calls: Arc<Mutex<Vec<ListAutomationCall>>>,
    /// Scopes returned by `resolve_run_thread_scope`, keyed by the queried
    /// thread id so tests prove the lookup contract rather than accepting a
    /// cached scope for any request.
    pub(crate) resolve_scopes: HashMap<ThreadId, TriggerRunThreadScope>,
    pub(crate) resolve_calls: Arc<Mutex<Vec<ThreadId>>>,
}

impl StaticAutomationService {
    pub(crate) fn new(output: Vec<RebornAutomationInfo>) -> Self {
        Self {
            output,
            list_error: None,
            scheduler_enabled: true,
            list_calls: Arc::new(Mutex::new(Vec::new())),
            resolve_scopes: HashMap::new(),
            resolve_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn with_scheduler_enabled(mut self, scheduler_enabled: bool) -> Self {
        self.scheduler_enabled = scheduler_enabled;
        self
    }

    pub(crate) fn with_list_error(mut self, error: ProductSurfaceError) -> Self {
        self.list_error = Some(error);
        self
    }

    pub(crate) fn with_resolve_scope_for_thread(
        mut self,
        thread_id: ThreadId,
        scope: TriggerRunThreadScope,
    ) -> Self {
        self.resolve_scopes.insert(thread_id, scope);
        self
    }

    pub(crate) fn resolve_calls(&self) -> Vec<ThreadId> {
        self.resolve_calls.lock().expect("lock").clone()
    }

    pub(crate) fn list_calls(&self) -> Vec<ListAutomationCall> {
        self.list_calls.lock().expect("lock").clone()
    }
}

#[async_trait]
impl AutomationProductService for StaticAutomationService {
    fn supports_legacy_approval_notification_backfill(&self) -> bool {
        true
    }

    fn scheduler_enabled(&self) -> bool {
        self.scheduler_enabled
    }

    async fn list_automations(
        &self,
        caller: ProductAgentBoundCaller,
        request: AutomationListRequest,
    ) -> Result<Vec<RebornAutomationInfo>, ProductSurfaceError> {
        self.list_calls
            .lock()
            .expect("lock")
            .push(ListAutomationCall {
                caller,
                limit: request.limit,
                run_limit: request.run_limit,
                include_completed: request.include_completed,
            });
        if let Some(error) = &self.list_error {
            return Err(error.clone());
        }
        Ok(self.output.clone())
    }

    async fn resolve_run_thread_scope(
        &self,
        _caller: ProductAgentBoundCaller,
        thread_id: &ThreadId,
    ) -> Result<Option<TriggerRunThreadScope>, ProductSurfaceError> {
        self.resolve_calls
            .lock()
            .expect("lock")
            .push(thread_id.clone());
        Ok(self.resolve_scopes.get(thread_id).cloned())
    }
}

/// An automation service that initially exposes one trigger thread scope but can
/// have that scope revoked via `revoke()`. Used to verify that the service
/// revalidates authorization on every call rather than caching the result.
pub(crate) struct RevocableAutomationService {
    pub(crate) thread_id: ThreadId,
    pub(crate) scope: TriggerRunThreadScope,
    pub(crate) revoked: Mutex<bool>,
}

impl RevocableAutomationService {
    pub(crate) fn new(thread_id: ThreadId, caller: &ProductSurfaceCaller) -> Self {
        let scope = TriggerRunThreadScope {
            agent_id: caller.agent_id.clone(),
            project_id: caller.project_id.clone(),
            creator_user_id: caller.user_id.clone(),
        };
        Self {
            thread_id,
            scope,
            revoked: Mutex::new(false),
        }
    }

    pub(crate) fn revoke(&self) {
        *self.revoked.lock().expect("lock") = true;
    }
}

#[async_trait]
impl AutomationProductService for RevocableAutomationService {
    async fn list_automations(
        &self,
        _caller: ProductAgentBoundCaller,
        _request: AutomationListRequest,
    ) -> Result<Vec<RebornAutomationInfo>, ProductSurfaceError> {
        Ok(Vec::new())
    }

    async fn resolve_run_thread_scope(
        &self,
        _caller: ProductAgentBoundCaller,
        thread_id: &ThreadId,
    ) -> Result<Option<TriggerRunThreadScope>, ProductSurfaceError> {
        if *self.revoked.lock().expect("lock") {
            return Ok(None);
        }
        if thread_id == &self.thread_id {
            Ok(Some(self.scope.clone()))
        } else {
            Ok(None)
        }
    }
}

/// Projection stream that records every `drain` invocation. Used by the
/// `stream_events` ownership regression to assert that the projection
/// drain is never reached when the ownership probe fails — if the probe
/// were skipped, `drain_count()` would observe the unauthorized read.
#[derive(Default)]
pub(crate) struct RecordingProjectionStream {
    pub(crate) drains: Mutex<Vec<ProjectionSubscriptionRequest>>,
}

impl RecordingProjectionStream {
    pub(crate) fn drain_count(&self) -> usize {
        self.drains.lock().expect("lock").len()
    }

    pub(crate) fn requests(&self) -> Vec<ProjectionSubscriptionRequest> {
        self.drains.lock().expect("lock").clone()
    }
}

#[async_trait]
impl ProjectionStream for RecordingProjectionStream {
    async fn drain(
        &self,
        request: ProjectionSubscriptionRequest,
    ) -> Result<Vec<ProductOutboundEnvelope>, ProductAdapterError> {
        self.drains.lock().expect("lock").push(request);
        Ok(Vec::new())
    }
}

#[derive(Default)]
pub(crate) struct RecordingOperatorLogsService {
    pub(crate) requests: Mutex<Vec<RebornLogQueryRequest>>,
}

impl RecordingOperatorLogsService {
    pub(crate) fn requests(&self) -> Vec<RebornLogQueryRequest> {
        self.requests.lock().expect("lock").clone()
    }
}

#[async_trait]
impl OperatorLogsService for RecordingOperatorLogsService {
    async fn query_logs(
        &self,
        _caller: ProductSurfaceCaller,
        request: RebornLogQueryRequest,
    ) -> Result<RebornLogQueryResponse, ProductSurfaceError> {
        self.requests.lock().expect("lock").push(request);
        Ok(RebornLogQueryResponse {
            source: "test".to_string(),
            entries: Vec::new(),
            next_cursor: None,
            tail_supported: false,
            follow_supported: false,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SetupUpsertCall {
    pub(crate) id: String,
    pub(crate) adapter: String,
    pub(crate) base_url: Option<String>,
    pub(crate) default_model: Option<String>,
    pub(crate) api_key_set: bool,
    pub(crate) set_active: bool,
    pub(crate) model: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SetupSetActiveCall {
    pub(crate) provider_id: String,
    pub(crate) model: Option<String>,
}

pub(crate) struct SetupRecordingLlmConfigService {
    pub(crate) snapshot_calls: Mutex<usize>,
    pub(crate) snapshot_callers: Mutex<Vec<ProductSurfaceCaller>>,
    pub(crate) upsert_provider_calls: Mutex<Vec<SetupUpsertCall>>,
    pub(crate) delete_provider_calls: Mutex<Vec<String>>,
    pub(crate) set_active_calls: Mutex<Vec<SetupSetActiveCall>>,
    pub(crate) test_connection_calls: Mutex<usize>,
    pub(crate) list_models_calls: Mutex<usize>,
    pub(crate) snapshot: Mutex<LlmConfigSnapshot>,
    pub(crate) next_snapshot_error: Mutex<Option<LlmConfigServiceError>>,
    pub(crate) next_upsert_error: Mutex<Option<LlmConfigServiceError>>,
    pub(crate) next_set_active_error: Mutex<Option<LlmConfigServiceError>>,
    pub(crate) next_login_error: Mutex<Option<LlmConfigServiceError>>,
    pub(crate) next_model_resolution: Mutex<Option<Result<Option<String>, LlmConfigServiceError>>>,
    pub(crate) user_model_catalog: Mutex<UserModelCatalog>,
    pub(crate) user_model_preferences: Mutex<HashMap<(String, String), UserModelPreference>>,
    pub(crate) user_model_preference_updates: Mutex<Vec<(String, String, Option<String>)>>,
}

impl Default for SetupRecordingLlmConfigService {
    fn default() -> Self {
        Self {
            snapshot_calls: Mutex::new(0),
            snapshot_callers: Mutex::new(Vec::new()),
            upsert_provider_calls: Mutex::new(Vec::new()),
            delete_provider_calls: Mutex::new(Vec::new()),
            set_active_calls: Mutex::new(Vec::new()),
            test_connection_calls: Mutex::new(0),
            list_models_calls: Mutex::new(0),
            snapshot: Mutex::new(Self::empty_snapshot()),
            next_snapshot_error: Mutex::new(None),
            next_upsert_error: Mutex::new(None),
            next_set_active_error: Mutex::new(None),
            next_login_error: Mutex::new(None),
            next_model_resolution: Mutex::new(None),
            user_model_catalog: Mutex::new(UserModelCatalog {
                selection_enabled: true,
                workspace_default: Some("model-a".to_string()),
                models: vec!["model-a".to_string(), "model-b".to_string()],
                model_entries: Vec::new(),
            }),
            user_model_preferences: Mutex::new(HashMap::new()),
            user_model_preference_updates: Mutex::new(Vec::new()),
        }
    }
}

impl SetupRecordingLlmConfigService {
    pub(crate) fn snapshot_count(&self) -> usize {
        *self.snapshot_calls.lock().expect("lock")
    }

    pub(crate) fn snapshot_callers(&self) -> Vec<ProductSurfaceCaller> {
        self.snapshot_callers.lock().expect("lock").clone()
    }

    pub(crate) fn upsert_provider_count(&self) -> usize {
        self.upsert_provider_calls.lock().expect("lock").len()
    }

    pub(crate) fn set_active_count(&self) -> usize {
        self.set_active_calls.lock().expect("lock").len()
    }

    pub(crate) fn test_connection_count(&self) -> usize {
        *self.test_connection_calls.lock().expect("lock")
    }

    pub(crate) fn list_models_count(&self) -> usize {
        *self.list_models_calls.lock().expect("lock")
    }

    pub(crate) fn use_active_snapshot(&self, provider_id: &str, model: &str) {
        *self.snapshot.lock().expect("lock") = Self::active_snapshot(provider_id, model);
    }

    pub(crate) fn fail_next_snapshot(&self, error: LlmConfigServiceError) {
        *self.next_snapshot_error.lock().expect("lock") = Some(error);
    }

    pub(crate) fn fail_next_upsert(&self, error: LlmConfigServiceError) {
        *self.next_upsert_error.lock().expect("lock") = Some(error);
    }

    pub(crate) fn fail_next_login(&self, error: LlmConfigServiceError) {
        *self.next_login_error.lock().expect("login error lock") = Some(error);
    }

    pub(crate) fn take_login_error(&self) -> Result<(), LlmConfigServiceError> {
        match self
            .next_login_error
            .lock()
            .expect("login error lock")
            .take()
        {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(crate) fn fail_next_set_active(&self, error: LlmConfigServiceError) {
        *self.next_set_active_error.lock().expect("lock") = Some(error);
    }

    pub(crate) fn resolve_next_model_as(
        &self,
        result: Result<Option<String>, LlmConfigServiceError>,
    ) {
        *self.next_model_resolution.lock().expect("lock") = Some(result);
    }

    pub(crate) fn user_model_preference_updates(&self) -> Vec<(String, String, Option<String>)> {
        self.user_model_preference_updates
            .lock()
            .expect("lock")
            .clone()
    }

    pub(crate) fn use_user_model_catalog(&self, catalog: UserModelCatalog) {
        *self.user_model_catalog.lock().expect("lock") = catalog;
    }

    pub(crate) fn user_model_preference_key(caller: &ProductSurfaceCaller) -> (String, String) {
        (
            caller.tenant_id.as_str().to_string(),
            caller.user_id.as_str().to_string(),
        )
    }

    pub(crate) fn empty_snapshot() -> LlmConfigSnapshot {
        LlmConfigSnapshot {
            providers: Vec::new(),
            active: None,
            user_model_policy: None,
        }
    }

    pub(crate) fn active_snapshot(provider_id: &str, model: &str) -> LlmConfigSnapshot {
        LlmConfigSnapshot {
            providers: vec![LlmProviderView {
                id: provider_id.to_string(),
                description: "configured provider".to_string(),
                adapter: "open_ai_completions".to_string(),
                default_model: model.to_string(),
                base_url: Some("https://api.example.test/v1".to_string()),
                builtin: false,
                active: true,
                active_model: Some(model.to_string()),
                api_key_required: true,
                accepts_api_key: true,
                api_key_set: true,
                can_list_models: true,
            }],
            active: Some(LlmActiveSelection {
                provider_id: provider_id.to_string(),
                model: Some(model.to_string()),
            }),
            user_model_policy: None,
        }
    }
}

#[async_trait]
impl LlmConfigService for SetupRecordingLlmConfigService {
    async fn snapshot(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<LlmConfigSnapshot, LlmConfigServiceError> {
        *self.snapshot_calls.lock().expect("lock") += 1;
        self.snapshot_callers.lock().expect("lock").push(caller);
        if let Some(error) = self.next_snapshot_error.lock().expect("lock").take() {
            return Err(error);
        }
        Ok(self.snapshot.lock().expect("lock").clone())
    }

    async fn upsert_provider(
        &self,
        _caller: ProductSurfaceCaller,
        request: UpsertLlmProviderRequest,
    ) -> Result<LlmConfigSnapshot, LlmConfigServiceError> {
        if let Some(error) = self.next_upsert_error.lock().expect("lock").take() {
            return Err(error);
        }
        self.upsert_provider_calls
            .lock()
            .expect("lock")
            .push(SetupUpsertCall {
                id: request.id,
                adapter: request.adapter,
                base_url: request.base_url,
                default_model: request.default_model,
                api_key_set: request.api_key.is_some(),
                set_active: request.set_active,
                model: request.model,
            });
        Ok(self.snapshot.lock().expect("lock").clone())
    }

    async fn delete_provider(
        &self,
        _caller: ProductSurfaceCaller,
        provider_id: String,
    ) -> Result<LlmConfigSnapshot, LlmConfigServiceError> {
        self.delete_provider_calls
            .lock()
            .expect("lock")
            .push(provider_id);
        Ok(self.snapshot.lock().expect("lock").clone())
    }

    async fn set_active(
        &self,
        _caller: ProductSurfaceCaller,
        request: SetActiveLlmRequest,
    ) -> Result<LlmConfigSnapshot, LlmConfigServiceError> {
        if let Some(error) = self.next_set_active_error.lock().expect("lock").take() {
            return Err(error);
        }
        self.set_active_calls
            .lock()
            .expect("lock")
            .push(SetupSetActiveCall {
                provider_id: request.provider_id,
                model: request.model,
            });
        Ok(self.snapshot.lock().expect("lock").clone())
    }

    async fn test_connection(
        &self,
        _caller: ProductSurfaceCaller,
        _request: LlmProbeRequest,
    ) -> Result<LlmProbeResult, LlmConfigServiceError> {
        *self.test_connection_calls.lock().expect("lock") += 1;
        Ok(LlmProbeResult {
            ok: true,
            message: "ok".to_string(),
        })
    }

    async fn list_models(
        &self,
        _caller: ProductSurfaceCaller,
        _request: LlmProbeRequest,
    ) -> Result<LlmModelsResult, LlmConfigServiceError> {
        *self.list_models_calls.lock().expect("lock") += 1;
        Ok(LlmModelsResult {
            ok: true,
            models: vec!["model-a".to_string()],
            model_entries: Vec::new(),
            message: String::new(),
        })
    }

    async fn resolve_user_model(
        &self,
        _caller: ProductSurfaceCaller,
        requested_model: Option<String>,
    ) -> Result<Option<String>, LlmConfigServiceError> {
        self.next_model_resolution
            .lock()
            .expect("lock")
            .take()
            .unwrap_or(Ok(requested_model))
    }

    async fn user_model_catalog(
        &self,
        _caller: ProductSurfaceCaller,
    ) -> Result<UserModelCatalog, LlmConfigServiceError> {
        Ok(self.user_model_catalog.lock().expect("lock").clone())
    }

    async fn user_model_preference(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<UserModelPreference, LlmConfigServiceError> {
        Ok(self
            .user_model_preferences
            .lock()
            .expect("lock")
            .get(&Self::user_model_preference_key(&caller))
            .cloned()
            .unwrap_or(UserModelPreference { model: None }))
    }

    async fn set_user_model_preference(
        &self,
        caller: ProductSurfaceCaller,
        request: SetUserModelPreferenceRequest,
    ) -> Result<UserModelPreference, LlmConfigServiceError> {
        let key = Self::user_model_preference_key(&caller);
        self.user_model_preference_updates
            .lock()
            .expect("lock")
            .push((key.0.clone(), key.1.clone(), request.model.clone()));
        let preference = UserModelPreference {
            model: request.model,
        };
        self.user_model_preferences
            .lock()
            .expect("lock")
            .insert(key, preference.clone());
        Ok(preference)
    }

    // The three vendor logins answer with `next_login_error` when one is armed.
    // They used to `panic!("not used by operator setup tests")`, which made the
    // *failure* half of each path untestable -- and that half is the whole
    // point: WS5 replaced product's own `map_llm_config_error` with the `From`
    // projection declared beside the port, so these three are the call sites
    // that prove the projection is what product actually returns.
    async fn start_nearai_login(
        &self,
        _caller: ProductSurfaceCaller,
        _request: NearAiLoginRequest,
    ) -> Result<NearAiLoginStart, LlmConfigServiceError> {
        self.take_login_error()?;
        Ok(NearAiLoginStart {
            auth_url: "https://auth.example/v1/auth/github".to_string(),
        })
    }

    async fn complete_nearai_wallet_login(
        &self,
        _caller: ProductSurfaceCaller,
        _request: NearAiWalletLoginRequest,
    ) -> Result<NearAiWalletLoginResult, LlmConfigServiceError> {
        self.take_login_error()?;
        Ok(NearAiWalletLoginResult { active: true })
    }

    async fn start_codex_login(
        &self,
        _caller: ProductSurfaceCaller,
    ) -> Result<CodexLoginStart, LlmConfigServiceError> {
        self.take_login_error()?;
        Ok(CodexLoginStart {
            user_code: "ABCD-EFGH".to_string(),
            verification_uri: "https://auth.example/device".to_string(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct StaticOperatorToolCatalogForTest {
    pub(crate) tools: Vec<RebornOperatorToolInfo>,
}

#[async_trait]
impl RebornOperatorToolCatalog for StaticOperatorToolCatalogForTest {
    async fn list_operator_tools(
        &self,
        _caller: &ironclaw_host_api::ids::UserId,
    ) -> Vec<RebornOperatorToolInfo> {
        // Ownership filtering is exercised by the composition-tier catalog
        // test; this static catalog is caller-agnostic on purpose.
        self.tools.clone()
    }
}

#[derive(Clone)]
pub(crate) struct OperatorConfigAutoApproveInvoker {
    pub(crate) auto_approve: Arc<dyn AutoApproveSettingStorePort>,
    pub(crate) overrides: Arc<dyn CapabilityPermissionOverrideStorePort>,
    pub(crate) persistent_policies: Arc<dyn PersistentApprovalPolicyStorePort>,
    pub(crate) tools: Arc<Vec<RebornOperatorToolInfo>>,
}

#[async_trait]
impl ProductCapabilityInvoker for OperatorConfigAutoApproveInvoker {
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        capability: CapabilityId,
        input: serde_json::Value,
        activity_id: ActivityId,
    ) -> Result<Resolution, ProductSurfaceError> {
        match capability.as_str() {
            OPERATOR_CONFIG_SET_AUTO_APPROVE_CAPABILITY_ID => {
                self.invoke_auto_approve(caller, input, activity_id).await
            }
            OPERATOR_CONFIG_SET_TOOL_PERMISSION_CAPABILITY_ID => {
                self.invoke_tool_permission(caller, input, activity_id)
                    .await
            }
            capability => panic!("unexpected operator config capability {capability}"),
        }
    }
}

impl OperatorConfigAutoApproveInvoker {
    pub(crate) async fn invoke_auto_approve(
        &self,
        caller: ProductSurfaceCaller,
        input: serde_json::Value,
        activity_id: ActivityId,
    ) -> Result<Resolution, ProductSurfaceError> {
        let enabled = input
            .get("enabled")
            .and_then(serde_json::Value::as_bool)
            .expect("auto-approve capability input must carry enabled bool");
        let scope = ResourceScope {
            tenant_id: caller.tenant_id.clone(),
            user_id: caller.user_id.clone(),
            agent_id: caller.agent_id.clone(),
            project_id: caller.project_id.clone(),
            mission_id: None,
            thread_id: None,
            invocation_id: InvocationId::from_uuid(activity_id.as_uuid()),
        }
        .tenant_user_settings_scope();
        self.auto_approve
            .set(AutoApproveSettingInput {
                scope,
                enabled,
                updated_by: Principal::User(caller.user_id.clone()),
            })
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        Ok(operator_config_success_resolution(activity_id))
    }

    pub(crate) async fn invoke_tool_permission(
        &self,
        caller: ProductSurfaceCaller,
        input: serde_json::Value,
        activity_id: ActivityId,
    ) -> Result<Resolution, ProductSurfaceError> {
        let capability_id = input
            .get("capability_id")
            .and_then(serde_json::Value::as_str)
            .expect("tool-permission capability input must carry capability_id");
        let state = input
            .get("state")
            .and_then(serde_json::Value::as_str)
            .expect("tool-permission capability input must carry state");
        let tool_capability_id = CapabilityId::new(capability_id).expect("capability id");
        let tool = self
            .tools
            .iter()
            .find(|tool| tool.capability_id == tool_capability_id)
            .unwrap_or_else(|| panic!("operator tool {capability_id}"));
        let scope = ResourceScope {
            tenant_id: caller.tenant_id.clone(),
            user_id: caller.user_id.clone(),
            agent_id: caller.agent_id.clone(),
            project_id: caller.project_id.clone(),
            mission_id: None,
            thread_id: None,
            invocation_id: InvocationId::from_uuid(activity_id.as_uuid()),
        }
        .tenant_user_settings_scope();
        let override_key = ToolPermissionOverrideKey::new(&scope, tool.capability_id.clone());
        let policy_key = PersistentApprovalPolicyKey::new(
            &scope,
            PersistentApprovalAction::Dispatch,
            tool.capability_id.clone(),
            Principal::Extension(tool.provider.clone()),
        );

        match state {
            "default" => {
                self.revoke_tool_policy(&policy_key).await?;
                self.overrides
                    .clear(&override_key)
                    .await
                    .map_err(ProductSurfaceError::internal_from)?;
            }
            "always_allow" => {
                if tool.default_permission == PermissionMode::Deny
                    || tool
                        .effects
                        .iter()
                        .any(|effect| matches!(effect, EffectKind::Financial))
                {
                    return Err(ProductSurfaceError::validation(
                        "state",
                        ProductSurfaceValidationCode::InvalidValue,
                    ));
                }
                self.persistent_policies
                    .allow(PersistentApprovalPolicyInput {
                        scope: scope.clone(),
                        action: PersistentApprovalAction::Dispatch,
                        capability_id: tool.capability_id.clone(),
                        grantee: Principal::Extension(tool.provider.clone()),
                        approved_by: Principal::User(caller.user_id.clone()),
                        constraints: ironclaw_host_api::capability::GrantConstraints {
                            allowed_effects: tool.effects.as_ref().to_vec(),
                            mounts: Default::default(),
                            network: Default::default(),
                            secrets: Vec::new(),
                            resource_ceiling: None,
                            expires_at: None,
                            max_invocations: None,
                        },
                        source_approval_request_id: None,
                    })
                    .await
                    .map_err(ProductSurfaceError::internal_from)?;
                self.overrides
                    .clear(&override_key)
                    .await
                    .map_err(ProductSurfaceError::internal_from)?;
            }
            "ask_each_time" | "disabled" => {
                self.revoke_tool_policy(&policy_key).await?;
                let state = match state {
                    "ask_each_time" => ToolPermissionOverride::AskEachTime,
                    "disabled" => ToolPermissionOverride::Disabled,
                    _ => unreachable!("state matched above"),
                };
                self.overrides
                    .set(ToolPermissionOverrideInput {
                        scope,
                        capability_id: tool.capability_id.clone(),
                        state,
                        updated_by: Principal::User(caller.user_id.clone()),
                    })
                    .await
                    .map_err(ProductSurfaceError::internal_from)?;
            }
            state => panic!("unexpected tool-permission state {state}"),
        }

        Ok(operator_config_success_resolution(activity_id))
    }

    pub(crate) async fn revoke_tool_policy(
        &self,
        policy_key: &PersistentApprovalPolicyKey,
    ) -> Result<(), ProductSurfaceError> {
        match self.persistent_policies.revoke(policy_key).await {
            Ok(_) | Err(PersistentApprovalPolicyError::UnknownPolicy) => Ok(()),
            Err(error) => Err(ProductSurfaceError::internal_from(error)),
        }
    }
}

#[derive(Default)]
pub(crate) struct FakeAdminUsers {
    pub(crate) users: Mutex<HashMap<(String, String), AdminUserRecord>>,
    // Regression counter for item 6 of the PR-2 palette review fix wave:
    // `execute_product_command` must read the admin directory exactly once
    // per request (resolved once at the top and reused), never twice (the
    // audience gate + the Lifecycle/Unknown help-text arm previously each
    // called `caller_is_command_admin`, which calls `get_user`, separately).
    pub(crate) get_user_calls: Mutex<usize>,
}

impl FakeAdminUsers {
    pub(crate) fn with(records: impl IntoIterator<Item = AdminUserRecord>) -> Self {
        Self::with_tenant("tenant-alpha", records)
    }

    pub(crate) fn with_tenant(
        tenant_id: &str,
        records: impl IntoIterator<Item = AdminUserRecord>,
    ) -> Self {
        Self::with_tenant_records(
            records
                .into_iter()
                .map(|record| (tenant_id.to_string(), record)),
        )
    }

    pub(crate) fn with_tenant_records(
        records: impl IntoIterator<Item = (String, AdminUserRecord)>,
    ) -> Self {
        let map = records
            .into_iter()
            .map(|(tenant_id, record)| ((tenant_id, record.user_id.as_str().to_string()), record))
            .collect();
        Self {
            users: Mutex::new(map),
            get_user_calls: Mutex::new(0),
        }
    }

    pub(crate) fn get_user_calls(&self) -> usize {
        *self.get_user_calls.lock().unwrap()
    }

    pub(crate) fn key(tenant: &TenantId, user_id: &UserId) -> (String, String) {
        (tenant.as_str().to_string(), user_id.as_str().to_string())
    }
}

#[async_trait]
impl AdminUserService for FakeAdminUsers {
    async fn list_users(
        &self,
        tenant: &TenantId,
        status: Option<AdminUserStatus>,
        after: Option<&UserId>,
        limit: usize,
    ) -> Result<Vec<AdminUserRecord>, AdminUserError> {
        // Mirror the real port contract: status filter, then user_id-ascending
        // order, then the `after` cursor, then bound to `limit`.
        let mut records: Vec<AdminUserRecord> = self
            .users
            .lock()
            .unwrap()
            .iter()
            .filter(|((tenant_id, _), _)| tenant_id == tenant.as_str())
            .map(|(_, record)| record)
            .filter(|record| status.is_none_or(|want| record.status == want))
            .cloned()
            .collect();
        records.sort_by(|a, b| a.user_id.as_str().cmp(b.user_id.as_str()));
        let after = after.map(UserId::as_str);
        Ok(records
            .into_iter()
            .filter(|record| after.is_none_or(|cursor| record.user_id.as_str() > cursor))
            .take(limit)
            .collect())
    }

    async fn get_user(
        &self,
        tenant: &TenantId,
        user_id: &UserId,
    ) -> Result<Option<AdminUserRecord>, AdminUserError> {
        *self.get_user_calls.lock().unwrap() += 1;
        Ok(self
            .users
            .lock()
            .unwrap()
            .get(&Self::key(tenant, user_id))
            .cloned())
    }

    async fn create_user(
        &self,
        tenant: &TenantId,
        _actor: &UserId,
        fields: AdminCreateUserFields,
    ) -> Result<AdminCreatedUser, AdminUserError> {
        let record = admin_record("created-user", fields.role, AdminUserStatus::Active);
        self.users
            .lock()
            .unwrap()
            .insert(Self::key(tenant, &record.user_id), record.clone());
        Ok(AdminCreatedUser {
            record,
            api_token: SecretString::from("minted-token"),
        })
    }

    async fn update_profile(
        &self,
        tenant: &TenantId,
        user_id: &UserId,
        display_name: Option<String>,
        _metadata: Option<std::collections::BTreeMap<String, String>>,
    ) -> Result<AdminUserRecord, AdminUserError> {
        let mut users = self.users.lock().unwrap();
        let record = users
            .get_mut(&Self::key(tenant, user_id))
            .ok_or(AdminUserError::NotFound)?;
        if display_name.is_some() {
            record.display_name = display_name;
        }
        Ok(record.clone())
    }

    async fn set_status(
        &self,
        tenant: &TenantId,
        user_id: &UserId,
        status: AdminUserStatus,
    ) -> Result<AdminUserRecord, AdminUserError> {
        let mut users = self.users.lock().unwrap();
        let record = users
            .get_mut(&Self::key(tenant, user_id))
            .ok_or(AdminUserError::NotFound)?;
        record.status = status;
        Ok(record.clone())
    }

    async fn set_role(
        &self,
        tenant: &TenantId,
        user_id: &UserId,
        role: AdminUserRole,
    ) -> Result<AdminUserRecord, AdminUserError> {
        let mut users = self.users.lock().unwrap();
        let record = users
            .get_mut(&Self::key(tenant, user_id))
            .ok_or(AdminUserError::NotFound)?;
        record.role = role;
        Ok(record.clone())
    }

    async fn delete_user(&self, tenant: &TenantId, user_id: &UserId) -> Result<(), AdminUserError> {
        self.users
            .lock()
            .unwrap()
            .remove(&Self::key(tenant, user_id));
        Ok(())
    }

    async fn count_active_admins(&self, tenant: &TenantId) -> Result<usize, AdminUserError> {
        Ok(self
            .users
            .lock()
            .unwrap()
            .iter()
            .filter(|((tenant_id, _), record)| {
                tenant_id == tenant.as_str()
                    && record.status == AdminUserStatus::Active
                    && record.role.is_admin()
            })
            .count())
    }

    async fn list_secrets(
        &self,
        _tenant: &TenantId,
        _user_id: &UserId,
    ) -> Result<Vec<AdminUserSecretMeta>, AdminUserError> {
        Ok(Vec::new())
    }

    async fn put_secret(
        &self,
        _tenant: &TenantId,
        _user_id: &UserId,
        handle: SecretHandle,
        _material: SecretString,
    ) -> Result<AdminUserSecretMeta, AdminUserError> {
        Ok(AdminUserSecretMeta {
            handle: handle.as_str().to_string(),
            created_at: None,
            updated_at: None,
        })
    }

    async fn delete_secret(
        &self,
        _tenant: &TenantId,
        _user_id: &UserId,
        _handle: SecretHandle,
    ) -> Result<bool, AdminUserError> {
        Ok(true)
    }
}

// ─── Generic session-inbound channel parameter ──────────────────────────────
//
// The generic session route keys submissions by `extension_id`; the surface
// validates it against the deployment's session-channel directory before
// admitting anything under that identity. Unknown/non-session extensions and
// missing directories fail closed; unparameterized submissions are rejected.

pub(crate) struct StaticSessionChannelDirectory {
    pub(crate) session_channels: Vec<&'static str>,
}

impl ironclaw_product_contracts::session_ingress::SessionChannelDirectory
    for StaticSessionChannelDirectory
{
    fn is_session_channel(&self, extension_id: &str) -> bool {
        self.session_channels.contains(&extension_id)
    }
}
