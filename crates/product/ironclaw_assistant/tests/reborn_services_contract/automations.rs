//! Automation listing and lifecycle mutations.

use super::*;

fn caller_without_agent() -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("tenant-alpha").expect("valid tenant"),
        UserId::new("user-alpha").expect("valid user"),
        None,
        Some(ProjectId::new("project-alpha").expect("valid project")),
    )
}

#[derive(Default)]
struct RecordingAutomationService {
    list_calls: Mutex<Vec<ListAutomationCall>>,
    mutation_calls: Mutex<Vec<AutomationMutationCall>>,
    run_updated: Option<bool>,
    run_result: Option<RebornAutomationRunMutationResult>,
}

#[derive(Clone, Default)]
struct RecordingProductResultInvoker {
    outputs: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl RecordingProductResultInvoker {
    fn outputs(&self) -> Vec<serde_json::Value> {
        self.outputs.lock().expect("lock").clone()
    }
}

#[async_trait]
impl ProductCapabilityInvoker for RecordingProductResultInvoker {
    async fn invoke(
        &self,
        _caller: ProductSurfaceCaller,
        _capability: CapabilityId,
        _input: serde_json::Value,
        _activity_id: ActivityId,
    ) -> Result<Resolution, ProductSurfaceError> {
        panic!("runtime-owned capability invocation is not expected")
    }

    async fn complete_product_result(
        &self,
        _caller: ProductSurfaceCaller,
        output: serde_json::Value,
        activity_id: ActivityId,
        summary: &'static str,
    ) -> Result<Resolution, ProductSurfaceError> {
        let byte_len = serde_json::to_vec(&output)
            .expect("recorded product result serializes")
            .len() as u64;
        self.outputs.lock().expect("lock").push(output);
        Ok(Resolution::Done(Outcome {
            refs: OutcomeRefs {
                result: ResultRef::from_uuid(activity_id.as_uuid()),
                byte_len,
                preview: None,
                preview_meta: ResultPreviewMeta::default(),
                origin: None,
                output_digest: None,
            },
            verdict: ToolVerdict::Success,
            summary: SafeSummary::new(summary).expect("static summary is safe"),
            progress: ResultProgress::MadeProgress,
            terminate_hint: TerminateHint::Continue,
        }))
    }
}

impl RecordingAutomationService {
    fn with_run_result(run_result: RebornAutomationRunMutationResult) -> Self {
        Self {
            run_result: Some(run_result),
            ..Self::default()
        }
    }

    fn with_missing_run_target() -> Self {
        Self {
            run_updated: Some(false),
            ..Self::default()
        }
    }

    fn list_calls(&self) -> Vec<ListAutomationCall> {
        self.list_calls.lock().expect("lock").clone()
    }

    fn mutation_calls(&self) -> Vec<AutomationMutationCall> {
        self.mutation_calls.lock().expect("lock").clone()
    }
}

#[async_trait]
impl AutomationProductService for RecordingAutomationService {
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
        Ok(vec![automation_info(
            "trigger-listed",
            "Daily status",
            "0 9 * * *",
            Some(RebornAutomationRunStatus::Ok),
        )])
    }

    async fn resolve_run_thread_scope(
        &self,
        _caller: ProductAgentBoundCaller,
        _thread_id: &ThreadId,
    ) -> Result<Option<TriggerRunThreadScope>, ProductSurfaceError> {
        // Trigger-thread access is not wired in the recording service.
        Ok(None)
    }

    async fn pause_automation(
        &self,
        caller: ProductAgentBoundCaller,
        automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        self.mutation_calls
            .lock()
            .expect("lock")
            .push(AutomationMutationCall {
                caller,
                automation_id,
                action: AutomationMutationAction::Pause,
            });
        Ok(RebornAutomationMutationResponse {
            updated: true,
            automation: Some(automation_info(
                "trigger-paused",
                "Daily status",
                "0 9 * * *",
                None,
            )),
            run_result: None,
        })
    }

    async fn run_automation(
        &self,
        caller: ProductAgentBoundCaller,
        automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        self.mutation_calls
            .lock()
            .expect("lock")
            .push(AutomationMutationCall {
                caller,
                automation_id,
                action: AutomationMutationAction::Run,
            });
        Ok(RebornAutomationMutationResponse {
            updated: self.run_updated.unwrap_or(true),
            automation: self
                .run_updated
                .unwrap_or(true)
                .then(|| automation_info("trigger-running", "Daily status", "0 9 * * *", None)),
            run_result: self.run_result.clone(),
        })
    }

    async fn resume_automation(
        &self,
        caller: ProductAgentBoundCaller,
        automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        self.mutation_calls
            .lock()
            .expect("lock")
            .push(AutomationMutationCall {
                caller,
                automation_id,
                action: AutomationMutationAction::Resume,
            });
        Ok(RebornAutomationMutationResponse {
            updated: true,
            automation: Some(automation_info(
                "trigger-resumed",
                "Daily status",
                "0 9 * * *",
                None,
            )),
            run_result: None,
        })
    }

    async fn rename_automation(
        &self,
        caller: ProductAgentBoundCaller,
        automation_id: String,
        name: AutomationName,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        self.mutation_calls
            .lock()
            .expect("lock")
            .push(AutomationMutationCall {
                caller,
                automation_id,
                action: AutomationMutationAction::Rename { name },
            });
        Ok(RebornAutomationMutationResponse {
            updated: true,
            automation: Some(automation_info(
                "trigger-renamed",
                "Renamed status",
                "0 9 * * *",
                None,
            )),
            run_result: None,
        })
    }

    async fn delete_automation(
        &self,
        caller: ProductAgentBoundCaller,
        automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        self.mutation_calls
            .lock()
            .expect("lock")
            .push(AutomationMutationCall {
                caller,
                automation_id,
                action: AutomationMutationAction::Delete,
            });
        Ok(RebornAutomationMutationResponse {
            updated: true,
            automation: None,
            run_result: None,
        })
    }
}

#[tokio::test]
async fn list_automation_dispatches_through_product_service() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    let listed = query_automations(
        &services,
        caller(),
        ProductListAutomationsRequest::default().set_limit(10),
    )
    .await
    .expect("list automations");
    assert_eq!(listed.automations.len(), 1);
    assert_eq!(listed.automations[0].automation_id, "trigger-listed");
    assert_eq!(
        listed.automations[0].source,
        RebornAutomationSource::Schedule {
            cron: "0 9 * * *".to_string(),
            timezone: "UTC".to_string(),
        }
    );
    assert_eq!(listed.automations[0].state, RebornAutomationState::Active);
    assert_eq!(
        listed.automations[0].last_status,
        Some(RebornAutomationRunStatus::Ok)
    );
    assert_eq!(listed.automations[0].recent_runs.len(), 1);
    assert_eq!(
        listed.automations[0].recent_runs[0].status,
        RebornAutomationRecentRunStatus::Ok
    );
    assert_eq!(
        listed.automations[0].recent_runs[0]
            .thread_id
            .as_ref()
            .map(|t| t.as_str()),
        Some("thread-listed")
    );

    let list_calls = automation_service.list_calls();
    assert_eq!(list_calls.len(), 1);
    assert_eq!(list_calls[0].caller.user_id.as_str(), "user-alpha");
    assert_eq!(list_calls[0].caller.agent_id.as_str(), "agent-alpha");
    assert_eq!(
        list_calls[0]
            .caller
            .project_id
            .as_ref()
            .map(ProjectId::as_str),
        Some("project-alpha")
    );
    assert_eq!(list_calls[0].limit, 10);
    assert_eq!(
        list_calls[0].run_limit, AUTOMATION_RUN_HISTORY_DEFAULT_PAGE_SIZE as usize,
        "omitted automation run history limit must use AUTOMATION_RUN_HISTORY_DEFAULT_PAGE_SIZE ({})",
        AUTOMATION_RUN_HISTORY_DEFAULT_PAGE_SIZE
    );
}

#[tokio::test]
async fn list_automations_rejects_missing_agent_id() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    let err = query_automations(
        &services,
        caller_without_agent(),
        ProductListAutomationsRequest::default().set_limit(10),
    )
    .await
    .expect_err("missing agent id should fail closed");

    assert_eq!(err.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(err.status_code, 400);
    assert_eq!(automation_service.list_calls().len(), 0);
}

#[tokio::test]
async fn list_automations_clamps_oversize_limit_before_product_service() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    query_automations(
        &services,
        caller(),
        ProductListAutomationsRequest::default().set_limit(u32::MAX),
    )
    .await
    .expect("list automations");

    let list_calls = automation_service.list_calls();
    assert_eq!(list_calls.len(), 1);
    assert_eq!(
        list_calls[0].limit, AUTOMATION_LIST_MAX_PAGE_SIZE as usize,
        "automation list limit must be clamped to AUTOMATION_LIST_MAX_PAGE_SIZE ({}) before the product service",
        AUTOMATION_LIST_MAX_PAGE_SIZE
    );
}

#[tokio::test]
async fn list_automations_clamps_zero_limit_before_product_service() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    query_automations(
        &services,
        caller(),
        ProductListAutomationsRequest::default().set_limit(0),
    )
    .await
    .expect("list automations");

    let list_calls = automation_service.list_calls();
    assert_eq!(list_calls.len(), 1);
    assert_eq!(
        list_calls[0].limit, 1,
        "automation list limit must be clamped to at least one row"
    );
}

#[tokio::test]
async fn list_automations_uses_default_limit_when_omitted() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    query_automations(
        &services,
        caller(),
        ProductListAutomationsRequest::default(),
    )
    .await
    .expect("list automations");

    let list_calls = automation_service.list_calls();
    assert_eq!(list_calls.len(), 1);
    assert_eq!(
        list_calls[0].limit, AUTOMATION_LIST_DEFAULT_PAGE_SIZE as usize,
        "omitted automation list limit must use AUTOMATION_LIST_DEFAULT_PAGE_SIZE ({})",
        AUTOMATION_LIST_DEFAULT_PAGE_SIZE
    );
}

#[tokio::test]
async fn list_automations_clamps_oversize_run_limit_before_product_service() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    query_automations(
        &services,
        caller(),
        ProductListAutomationsRequest::default().set_run_limit(u32::MAX),
    )
    .await
    .expect("list automations");

    let list_calls = automation_service.list_calls();
    assert_eq!(list_calls.len(), 1);
    assert_eq!(
        list_calls[0].run_limit, AUTOMATION_RUN_HISTORY_MAX_PAGE_SIZE as usize,
        "automation run history limit must be clamped to AUTOMATION_RUN_HISTORY_MAX_PAGE_SIZE ({}) before the product service",
        AUTOMATION_RUN_HISTORY_MAX_PAGE_SIZE
    );
}

#[tokio::test]
async fn list_automations_allows_zero_run_limit_before_product_service() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    query_automations(
        &services,
        caller(),
        ProductListAutomationsRequest::default().set_run_limit(0),
    )
    .await
    .expect("list automations");

    let list_calls = automation_service.list_calls();
    assert_eq!(list_calls.len(), 1);
    assert_eq!(
        list_calls[0].run_limit, 0,
        "explicit zero automation run history limit must disable embedded run history"
    );
}

#[tokio::test]
async fn list_automations_forwards_include_completed_true_to_product_service() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    query_automations(
        &services,
        caller(),
        ProductListAutomationsRequest::default().set_include_completed(true),
    )
    .await
    .expect("list automations");

    let list_calls = automation_service.list_calls();
    assert_eq!(list_calls.len(), 1);
    assert!(
        list_calls[0].include_completed,
        "include_completed=true must be forwarded to the product service unchanged"
    );
}

#[tokio::test]
async fn list_automations_forwards_include_completed_false_to_product_service() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    query_automations(
        &services,
        caller(),
        ProductListAutomationsRequest::default(),
    )
    .await
    .expect("list automations");

    let list_calls = automation_service.list_calls();
    assert_eq!(list_calls.len(), 1);
    assert!(
        !list_calls[0].include_completed,
        "include_completed=false must be forwarded to the product service unchanged"
    );
}

#[tokio::test]
async fn pause_automation_rejects_missing_agent_id() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    let err = invoke_json_product_capability(
        &services,
        caller_without_agent(),
        AUTOMATION_PAUSE_CAPABILITY_ID,
        RebornAutomationRequest {
            automation_id: "trigger-alpha".to_string(),
        },
    )
    .await
    .expect_err("missing agent id should fail closed");

    assert_eq!(err.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(err.status_code, 400);
    assert_eq!(automation_service.mutation_calls().len(), 0);
}

#[tokio::test]
async fn run_automation_rejects_missing_agent_id() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    let err = invoke_json_product_capability(
        &services,
        caller_without_agent(),
        AUTOMATION_RUN_CAPABILITY_ID,
        RebornAutomationRequest {
            automation_id: "trigger-alpha".to_string(),
        },
    )
    .await
    .expect_err("missing agent id should fail closed");

    assert_eq!(err.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(err.status_code, 400);
    assert_eq!(automation_service.mutation_calls().len(), 0);
}

#[tokio::test]
async fn resume_automation_rejects_missing_agent_id() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    let err = invoke_json_product_capability(
        &services,
        caller_without_agent(),
        AUTOMATION_RESUME_CAPABILITY_ID,
        RebornAutomationRequest {
            automation_id: "trigger-alpha".to_string(),
        },
    )
    .await
    .expect_err("missing agent id should fail closed");

    assert_eq!(err.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(err.status_code, 400);
    assert_eq!(automation_service.mutation_calls().len(), 0);
}

#[tokio::test]
async fn rename_automation_rejects_missing_agent_id() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    let err = invoke_json_product_capability(
        &services,
        caller_without_agent(),
        AUTOMATION_RENAME_CAPABILITY_ID,
        RebornRenameAutomationProductRequest {
            automation_id: "trigger-alpha".to_string(),
            name: Some("Renamed".to_string()),
        },
    )
    .await
    .expect_err("missing agent id should fail closed");

    assert_eq!(err.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(err.status_code, 400);
    assert_eq!(automation_service.mutation_calls().len(), 0);
}

#[tokio::test]
async fn delete_automation_rejects_missing_agent_id() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    let err = invoke_json_product_capability(
        &services,
        caller_without_agent(),
        AUTOMATION_DELETE_CAPABILITY_ID,
        RebornAutomationRequest {
            automation_id: "trigger-alpha".to_string(),
        },
    )
    .await
    .expect_err("missing agent id should fail closed");

    assert_eq!(err.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(err.status_code, 400);
    assert_eq!(automation_service.mutation_calls().len(), 0);
}

#[tokio::test]
async fn automation_mutations_forward_caller_scope_to_product_service() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());
    let caller = caller();
    let expected_agent_id = caller.agent_id.clone().expect("agent id");

    let pause = invoke_json_product_capability(
        &services,
        caller.clone(),
        AUTOMATION_PAUSE_CAPABILITY_ID,
        RebornAutomationRequest {
            automation_id: "trigger-alpha".to_string(),
        },
    )
    .await
    .expect("pause automation");
    assert!(matches!(
        pause,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));

    let run = invoke_json_product_capability(
        &services,
        caller.clone(),
        AUTOMATION_RUN_CAPABILITY_ID,
        RebornAutomationRequest {
            automation_id: "trigger-alpha".to_string(),
        },
    )
    .await
    .expect("run automation");
    assert!(matches!(
        run,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));

    let resume = invoke_json_product_capability(
        &services,
        caller.clone(),
        AUTOMATION_RESUME_CAPABILITY_ID,
        RebornAutomationRequest {
            automation_id: "trigger-alpha".to_string(),
        },
    )
    .await
    .expect("resume automation");
    assert!(matches!(
        resume,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));

    let rename = invoke_json_product_capability(
        &services,
        caller.clone(),
        AUTOMATION_RENAME_CAPABILITY_ID,
        RebornRenameAutomationProductRequest {
            automation_id: "trigger-alpha".to_string(),
            name: Some("  Renamed status  ".to_string()),
        },
    )
    .await
    .expect("rename automation");
    assert!(matches!(
        rename,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));

    let delete = invoke_json_product_capability(
        &services,
        caller.clone(),
        AUTOMATION_DELETE_CAPABILITY_ID,
        RebornAutomationRequest {
            automation_id: "trigger-alpha".to_string(),
        },
    )
    .await
    .expect("delete automation");
    assert!(matches!(
        delete,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));

    let calls = automation_service.mutation_calls();
    assert_eq!(calls.len(), 5);
    assert_eq!(calls[0].action, AutomationMutationAction::Pause);
    assert_eq!(calls[0].automation_id, "trigger-alpha");
    assert_eq!(calls[0].caller.tenant_id, caller.tenant_id);
    assert_eq!(calls[0].caller.user_id, caller.user_id);
    assert_eq!(calls[0].caller.agent_id, expected_agent_id);
    assert_eq!(calls[0].caller.project_id, caller.project_id);
    assert_eq!(calls[1].action, AutomationMutationAction::Run);
    assert_eq!(calls[1].automation_id, "trigger-alpha");
    assert_eq!(calls[1].caller.tenant_id, caller.tenant_id);
    assert_eq!(calls[1].caller.user_id, caller.user_id);
    assert_eq!(calls[1].caller.agent_id, expected_agent_id);
    assert_eq!(calls[1].caller.project_id, caller.project_id);
    assert_eq!(calls[2].action, AutomationMutationAction::Resume);
    assert_eq!(calls[2].automation_id, "trigger-alpha");
    assert_eq!(calls[2].caller.tenant_id, caller.tenant_id);
    assert_eq!(calls[2].caller.user_id, caller.user_id);
    assert_eq!(calls[2].caller.agent_id, expected_agent_id);
    assert_eq!(calls[2].caller.project_id, caller.project_id);
    assert_eq!(
        calls[3].action,
        AutomationMutationAction::Rename {
            name: AutomationName::new("Renamed status").expect("valid automation name")
        }
    );
    assert_eq!(calls[3].automation_id, "trigger-alpha");
    assert_eq!(calls[3].caller.tenant_id, caller.tenant_id);
    assert_eq!(calls[3].caller.user_id, caller.user_id);
    assert_eq!(calls[3].caller.agent_id, expected_agent_id);
    assert_eq!(calls[3].caller.project_id, caller.project_id);
    assert_eq!(calls[4].action, AutomationMutationAction::Delete);
    assert_eq!(calls[4].automation_id, "trigger-alpha");
    assert_eq!(calls[4].caller.tenant_id, caller.tenant_id);
    assert_eq!(calls[4].caller.user_id, caller.user_id);
    assert_eq!(calls[4].caller.agent_id, expected_agent_id);
    assert_eq!(calls[4].caller.project_id, caller.project_id);
}

#[tokio::test]
async fn automation_mutations_are_available_as_product_capabilities() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    for (capability_id, input) in [
        (
            AUTOMATION_PAUSE_CAPABILITY_ID,
            serde_json::to_value(RebornAutomationRequest {
                automation_id: "trigger-alpha".to_string(),
            })
            .expect("pause input"),
        ),
        (
            AUTOMATION_RUN_CAPABILITY_ID,
            serde_json::to_value(RebornAutomationRequest {
                automation_id: "trigger-alpha".to_string(),
            })
            .expect("run input"),
        ),
        (
            AUTOMATION_RESUME_CAPABILITY_ID,
            serde_json::to_value(RebornAutomationRequest {
                automation_id: "trigger-alpha".to_string(),
            })
            .expect("resume input"),
        ),
        (
            AUTOMATION_RENAME_CAPABILITY_ID,
            serde_json::to_value(RebornRenameAutomationProductRequest {
                automation_id: "trigger-alpha".to_string(),
                name: Some("Renamed status".to_string()),
            })
            .expect("rename input"),
        ),
        (
            AUTOMATION_DELETE_CAPABILITY_ID,
            serde_json::to_value(RebornAutomationRequest {
                automation_id: "trigger-alpha".to_string(),
            })
            .expect("delete input"),
        ),
    ] {
        let resolution = services
            .invoke(
                caller(),
                CapabilityId::new(capability_id).expect("capability id"),
                input,
                ActivityId::new(),
            )
            .await
            .expect("automation capability");
        assert!(matches!(
            resolution,
            Resolution::Done(outcome) if outcome.verdict.is_success()
        ));
    }

    let calls = automation_service.mutation_calls();
    assert_eq!(calls.len(), 5);
    assert_eq!(calls[0].action, AutomationMutationAction::Pause);
    assert_eq!(calls[1].action, AutomationMutationAction::Run);
    assert_eq!(calls[2].action, AutomationMutationAction::Resume);
    assert_eq!(
        calls[3].action,
        AutomationMutationAction::Rename {
            name: AutomationName::new("Renamed status").expect("valid automation name")
        }
    );
    assert_eq!(calls[4].action, AutomationMutationAction::Delete);
}

#[tokio::test]
async fn automation_run_capability_distinguishes_submitted_from_replayed() {
    for (status, expected_summary) in [
        (
            RebornAutomationRunMutationStatus::Submitted,
            "automation started",
        ),
        (
            RebornAutomationRunMutationStatus::Replayed,
            "automation run was already submitted",
        ),
    ] {
        let automation_service = Arc::new(RecordingAutomationService::with_run_result(
            RebornAutomationRunMutationResult {
                status,
                run_id: TurnRunId::new(),
            },
        ));
        let result_invoker = RecordingProductResultInvoker::default();
        let services = RebornServices::new_with_product_ports(
            Arc::new(InMemorySessionThreadService::default()),
            Arc::new(FakeTurnCoordinator::default()),
            result_invoker.clone(),
            ironclaw_assistant::UnavailableRebornViewProvider,
        )
        .with_session_channel_directory(Arc::new(StaticSessionChannelDirectory {
            session_channels: vec!["web-app"],
        }))
        .with_automation_product_service(automation_service);

        let resolution = invoke_json_product_capability(
            &services,
            caller(),
            AUTOMATION_RUN_CAPABILITY_ID,
            RebornAutomationRequest {
                automation_id: "trigger-alpha".to_string(),
            },
        )
        .await
        .expect("automation run capability");

        let Resolution::Done(outcome) = resolution else {
            panic!("automation run must return a completed outcome");
        };
        assert_eq!(outcome.summary.as_str(), expected_summary);
        assert!(outcome.refs.byte_len > 0);
        let outputs = result_invoker.outputs();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0]["status"], json!(status));
        assert!(outputs[0]["run_id"].as_str().is_some());
    }

    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(Arc::new(
        RecordingAutomationService::with_missing_run_target(),
    ));
    let error = invoke_json_product_capability(
        &services,
        caller(),
        AUTOMATION_RUN_CAPABILITY_ID,
        RebornAutomationRequest {
            automation_id: "missing-trigger".to_string(),
        },
    )
    .await
    .expect_err("a missing automation must not report a successful run");
    assert_eq!(error.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(error.status_code, 404);
}

#[tokio::test]
async fn rename_automation_validates_name_before_product_service() {
    let automation_service = Arc::new(RecordingAutomationService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(automation_service.clone());

    for (request, expected_code) in [
        (
            ProductRenameAutomationRequest { name: None },
            ProductSurfaceValidationCode::MissingField,
        ),
        (
            ProductRenameAutomationRequest {
                name: Some("  ".to_string()),
            },
            ProductSurfaceValidationCode::Blank,
        ),
        (
            ProductRenameAutomationRequest {
                name: Some("x".repeat(257)),
            },
            ProductSurfaceValidationCode::TooLong,
        ),
    ] {
        let err = invoke_json_product_capability(
            &services,
            caller(),
            AUTOMATION_RENAME_CAPABILITY_ID,
            RebornRenameAutomationProductRequest {
                automation_id: "trigger-alpha".to_string(),
                name: request.name,
            },
        )
        .await
        .expect_err("invalid name should fail before service");

        assert_eq!(err.code, ProductSurfaceErrorCode::InvalidRequest);
        assert_eq!(err.status_code, 400);
        assert_eq!(err.field.as_deref(), Some("name"));
        assert_eq!(err.validation_code, Some(expected_code));
    }

    assert_eq!(automation_service.mutation_calls().len(), 0);
}

#[test]
fn reborn_automation_state_round_trips_serde_for_every_variant() {
    let cases = [
        (RebornAutomationState::Active, "\"active\""),
        (RebornAutomationState::Scheduled, "\"scheduled\""),
        (RebornAutomationState::Paused, "\"paused\""),
        (RebornAutomationState::Disabled, "\"disabled\""),
        (RebornAutomationState::Inactive, "\"inactive\""),
        (RebornAutomationState::Completed, "\"completed\""),
        (RebornAutomationState::Unknown, "\"unknown\""),
    ];

    for (state, expected_wire) in cases {
        let serialized = serde_json::to_string(&state).expect("serialize state");
        assert_eq!(serialized, expected_wire);
        let deserialized: RebornAutomationState =
            serde_json::from_str(&serialized).expect("deserialize state");
        assert_eq!(deserialized, state);
    }
}

#[test]
fn reborn_automation_recent_run_info_round_trips_typed_ids_and_preserves_unknown_status() {
    let recent_run = RebornAutomationRecentRunInfo {
        run_id: Some(automation_run_id()),
        thread_id: Some(ThreadId::new("thread-listed").expect("valid thread id")),
        fire_slot: Some("2026-06-03T09:00:00Z".parse().expect("fire slot")),
        status: RebornAutomationRecentRunStatus::Running,
        submitted_at: "2026-06-03T09:00:01Z".parse().expect("submitted at"),
        completed_at: None,
    };

    let serialized = serde_json::to_value(&recent_run).expect("serialize recent run");
    assert_eq!(
        serialized,
        json!({
            "run_id": "11111111-1111-1111-1111-111111111111",
            "thread_id": "thread-listed",
            "fire_slot": "2026-06-03T09:00:00Z",
            "status": "running",
            "submitted_at": "2026-06-03T09:00:01Z",
        })
    );

    let deserialized: RebornAutomationRecentRunInfo =
        serde_json::from_value(serialized).expect("deserialize recent run");
    assert_eq!(deserialized, recent_run);

    let future_status: RebornAutomationRecentRunInfo = serde_json::from_value(json!({
        "run_id": "11111111-1111-1111-1111-111111111111",
        "thread_id": "thread-listed",
        "status": "cancelled",
        "submitted_at": "2026-06-03T09:00:01Z",
    }))
    .expect("deserialize future recent run status");
    assert_eq!(
        future_status.status,
        RebornAutomationRecentRunStatus::Unknown
    );

    let defaulted_status: RebornAutomationRecentRunInfo = serde_json::from_value(json!({
        "run_id": "11111111-1111-1111-1111-111111111111",
        "thread_id": "thread-listed",
        "submitted_at": "2026-06-03T09:00:01Z",
    }))
    .expect("deserialize defaulted recent run status");
    assert_eq!(
        defaulted_status.status,
        RebornAutomationRecentRunStatus::Unknown
    );

    serde_json::from_value::<RebornAutomationRecentRunInfo>(json!({
        "run_id": "11111111-1111-1111-1111-111111111111",
        "thread_id": "thread-listed",
        "status": { "backend": "future" },
        "submitted_at": "2026-06-03T09:00:01Z",
    }))
    .expect_err("recent run rejects malformed status");

    serde_json::from_value::<RebornAutomationRecentRunInfo>(json!({
        "run_id": "not-a-uuid",
        "thread_id": "thread-listed",
        "status": "running",
        "submitted_at": "2026-06-03T09:00:01Z",
    }))
    .expect_err("recent run rejects malformed run_id");

    serde_json::from_value::<RebornAutomationRecentRunInfo>(json!({
        "run_id": "11111111-1111-1111-1111-111111111111",
        "thread_id": "thread/listed",
        "status": "running",
        "submitted_at": "2026-06-03T09:00:01Z",
    }))
    .expect_err("recent run rejects malformed thread_id");
}

#[tokio::test]
async fn list_automations_returns_empty_list() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(Arc::new(StaticAutomationService::new(Vec::new())));

    let listed = query_automations(
        &services,
        caller(),
        ProductListAutomationsRequest::default(),
    )
    .await
    .expect("list automations");

    assert!(listed.automations.is_empty());
    // Default service reports the scheduler as running.
    assert!(listed.scheduler_enabled);
}

#[tokio::test]
async fn list_automations_surfaces_disabled_scheduler() {
    // Regression: when the trigger poller is off, the response must report
    // scheduler_enabled=false so the browser can warn that listed automations
    // will not fire. Previously the wire response had no such signal.
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_automation_product_service(Arc::new(
        StaticAutomationService::new(Vec::new()).with_scheduler_enabled(false),
    ));

    let listed = query_automations(
        &services,
        caller(),
        ProductListAutomationsRequest::default(),
    )
    .await
    .expect("list automations");

    assert!(!listed.scheduler_enabled);
}

#[tokio::test]
async fn automation_service_unwired_fails_closed() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let error = query_automations(
        &services,
        caller(),
        ProductListAutomationsRequest::default(),
    )
    .await
    .expect_err("unwired automation service");

    assert_eq!(error.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(error.status_code, 503);
    assert!(error.retryable);
}

async fn query_automations<S: ProductSurface + ?Sized>(
    services: &S,
    caller: ProductSurfaceCaller,
    request: ProductListAutomationsRequest,
) -> Result<RebornListAutomationsResponse, ProductSurfaceError> {
    let page = query_product_surface_page(services, caller, AUTOMATIONS_VIEW.query(request, None)?)
        .await?;
    AUTOMATIONS_VIEW.decode_page(page)
}

async fn invoke_json_product_capability<S, T>(
    services: &S,
    caller: ProductSurfaceCaller,
    capability_id: &str,
    input: T,
) -> Result<Resolution, ProductSurfaceError>
where
    S: ProductSurface + ?Sized,
    T: Serialize,
{
    let response = services
        .invoke(
            caller,
            ironclaw_product_contracts::surface::ProductSurfaceInvokeRequest {
                operation_id: CapabilityId::new(capability_id).expect("capability id"),
                input: serde_json::to_value(input).expect("capability input"),
                activity_id: ActivityId::new(),
            },
        )
        .await?;
    serde_json::from_value(response.output).map_err(ProductSurfaceError::internal_from)
}
