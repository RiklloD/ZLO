//! Operator first-run setup, LLM provider administration, and vendor logins.

use super::*;

async fn query_operator_setup<S: ProductSurface + ?Sized>(
    services: &S,
    caller: ProductSurfaceCaller,
) -> Result<ironclaw_assistant::RebornOperatorSetupResponse, ProductSurfaceError> {
    let page = query_product_surface_page(
        services,
        caller,
        RebornViewQuery {
            view_id: OPERATOR_SETUP_VIEW.id.to_string(),
            params: json!({}),
            cursor: None,
        },
    )
    .await?;
    serde_json::from_value(page.payload).map_err(ProductSurfaceError::internal_from)
}

#[tokio::test]
async fn get_operator_setup_returns_snapshot_from_llm_config() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    llm_config.use_active_snapshot("openai", "gpt-5-mini");
    let services = services_with_setup_llm_config(llm_config.clone());

    let response = query_operator_setup(&services, caller().with_operator_config(true))
        .await
        .expect("setup response");

    assert_eq!(llm_config.snapshot_count(), 1);
    assert_eq!(response.active_provider_id.as_deref(), Some("openai"));
    assert_eq!(response.active_model.as_deref(), Some("gpt-5-mini"));
    assert_eq!(response.status, RebornOperatorSetupStatus::Complete);
    assert!(response.diagnostics.is_empty());
    assert!(response.steps.iter().any(|step| {
        step.name == "profile"
            && step.status == ironclaw_assistant::RebornOperatorSetupStepStatus::Complete
    }));
    assert!(response.steps.iter().any(|step| {
        step.name == "webui_access"
            && step.status == ironclaw_assistant::RebornOperatorSetupStepStatus::Complete
    }));
}

#[tokio::test]
async fn get_operator_setup_without_llm_config_returns_service_unavailable() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let err = query_operator_setup(&services, caller().with_operator_config(true))
        .await
        .expect_err("setup service is unavailable");

    assert_eq!(err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(err.kind, ProductSurfaceErrorKind::ServiceUnavailable);
    assert_eq!(err.status_code, 503);
}

#[tokio::test]
async fn setup_response_reflects_active_provider_and_model() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    llm_config.use_active_snapshot("openai", "gpt-5-mini");
    let services = services_with_setup_llm_config(llm_config.clone());

    let response = services
        .run_operator_setup(caller(), RebornOperatorSetupRequest::default())
        .await
        .expect("setup response");

    assert_eq!(response.active_provider_id.as_deref(), Some("openai"));
    assert_eq!(response.active_model.as_deref(), Some("gpt-5-mini"));
    assert_eq!(response.status, RebornOperatorSetupStatus::Complete);
    assert!(response.steps.iter().any(|step| {
        step.name == "provider"
            && step.status == ironclaw_assistant::RebornOperatorSetupStepStatus::Complete
    }));
    assert!(response.steps.iter().any(|step| {
        step.name == "model"
            && step.status == ironclaw_assistant::RebornOperatorSetupStepStatus::Complete
    }));
    assert!(response.steps.iter().any(|step| {
        step.name == "profile"
            && step.status == ironclaw_assistant::RebornOperatorSetupStepStatus::Complete
    }));
    assert!(response.steps.iter().any(|step| {
        step.name == "webui_access"
            && step.status == ironclaw_assistant::RebornOperatorSetupStepStatus::Complete
    }));
}

#[tokio::test]
async fn run_operator_setup_without_llm_config_returns_service_unavailable() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let err = services
        .run_operator_setup(caller(), RebornOperatorSetupRequest::default())
        .await
        .expect_err("setup service is unavailable");

    assert_eq!(err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(err.kind, ProductSurfaceErrorKind::ServiceUnavailable);
    assert_eq!(err.status_code, 503);
}

#[tokio::test]
async fn run_operator_setup_requires_provider_id_for_provider_changes() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    llm_config.use_active_snapshot("openai", "gpt-5-mini");
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_adapter("open_ai_completions")
                .set_api_key(SecretString::from("sk-secret".to_string())),
        )
        .await
        .expect_err("provider changes require provider_id");

    assert_setup_validation(
        err,
        "provider_id",
        ProductSurfaceValidationCode::InvalidValue,
    );
    assert_eq!(llm_config.snapshot_count(), 0);
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_rejects_model_without_provider_id() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    llm_config.use_active_snapshot("openai", "gpt-5-mini");
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default().set_model("gpt-5-mini"),
        )
        .await
        .expect_err("model requires provider_id");

    assert_setup_validation(err, "model", ProductSurfaceValidationCode::InvalidValue);
    assert_eq!(llm_config.snapshot_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_rejects_base_url_without_adapter() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_base_url("https://api.example.test/v1"),
        )
        .await
        .expect_err("base_url requires adapter");

    assert_setup_validation(err, "base_url", ProductSurfaceValidationCode::InvalidValue);
    assert_eq!(llm_config.snapshot_count(), 0);
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_rejects_api_key_without_adapter() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_api_key(SecretString::from("sk-secret".to_string())),
        )
        .await
        .expect_err("api_key requires adapter");

    assert_setup_validation(err, "api_key", ProductSurfaceValidationCode::InvalidValue);
    assert_eq!(llm_config.snapshot_count(), 0);
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_rejects_internal_base_url_before_upsert() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_adapter("open_ai_completions")
                .set_base_url("http://169.254.169.254/latest/meta-data/"),
        )
        .await
        .expect_err("metadata endpoint is rejected");

    assert_setup_validation(err, "base_url", ProductSurfaceValidationCode::InvalidValue);
    assert_eq!(llm_config.upsert_provider_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_rejects_blank_profile_before_provider_write() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_adapter("open_ai_completions")
                .set_profile_id("   "),
        )
        .await
        .expect_err("blank profile id is rejected");

    assert_setup_validation(
        err,
        "profile_id",
        ProductSurfaceValidationCode::InvalidValue,
    );
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_rejects_oversized_profile_before_provider_write() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_adapter("open_ai_completions")
                .set_profile_id("x".repeat(129)),
        )
        .await
        .expect_err("oversized profile id is rejected");

    assert_setup_validation(
        err,
        "profile_id",
        ProductSurfaceValidationCode::InvalidValue,
    );
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_rejects_short_webui_access_token_before_provider_write() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_adapter("open_ai_completions")
                .set_webui_access_token(SecretString::from("too-short".to_string())),
        )
        .await
        .expect_err("short WebUI token is rejected");

    assert_setup_validation(
        err,
        "webui_access_token",
        ProductSurfaceValidationCode::InvalidValue,
    );
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_rejects_serve_weak_webui_access_token_before_provider_write() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_adapter("open_ai_completions")
                .set_webui_access_token(SecretString::from("x".repeat(16))),
        )
        .await
        .expect_err("16-byte WebUI token is rejected");

    assert_setup_validation(
        err,
        "webui_access_token",
        ProductSurfaceValidationCode::InvalidValue,
    );
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_rejects_oversized_webui_access_token_before_provider_write() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_adapter("open_ai_completions")
                .set_webui_access_token(SecretString::from("x".repeat(4097))),
        )
        .await
        .expect_err("oversized WebUI token is rejected");

    assert_setup_validation(
        err,
        "webui_access_token",
        ProductSurfaceValidationCode::InvalidValue,
    );
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn upsert_llm_provider_allows_loopback_base_url_for_self_hosted() {
    // Loopback/private endpoints are the primary self-hosted use case (Ollama,
    // vLLM): the guard must let them through to the service, not reject them as
    // "internal". Only the always-blocked classes (metadata/link-local,
    // multicast, unspecified) are rejected — see the metadata cases above.
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    services
        .invoke(
            caller(),
            CapabilityId::new(LLM_PROVIDER_UPSERT_CAPABILITY_ID).expect("capability id"),
            json!({
                "id": "ollama",
                "adapter": "ollama",
                "base_url": "http://127.0.0.1:11434/v1"
            }),
            ActivityId::new(),
        )
        .await
        .expect("loopback endpoint reaches the service");

    assert_eq!(llm_config.upsert_provider_count(), 1);
}

#[tokio::test]
async fn llm_config_mutations_are_available_as_product_capabilities() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    llm_config.use_active_snapshot("openai", "gpt-5-mini");
    let services = services_with_setup_llm_config(llm_config.clone());

    services
        .invoke(
            caller(),
            CapabilityId::new(LLM_PROVIDER_DELETE_CAPABILITY_ID).expect("capability id"),
            json!({ "provider_id": "acme" }),
            ActivityId::new(),
        )
        .await
        .expect("delete provider");
    services
        .invoke(
            caller(),
            CapabilityId::new(LLM_ACTIVE_SET_CAPABILITY_ID).expect("capability id"),
            json!({ "provider_id": "openai", "model": "gpt-5-mini" }),
            ActivityId::new(),
        )
        .await
        .expect("set active provider");

    assert_eq!(
        llm_config
            .delete_provider_calls
            .lock()
            .expect("lock")
            .as_slice(),
        ["acme"]
    );
    assert_eq!(
        llm_config.set_active_calls.lock().expect("lock").as_slice(),
        [SetupSetActiveCall {
            provider_id: "openai".to_string(),
            model: Some("gpt-5-mini".to_string()),
        }]
    );
}

#[tokio::test]
async fn operator_setup_run_is_available_as_product_capability() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    llm_config.use_active_snapshot("openai", "gpt-5-mini");
    let services = services_with_setup_llm_config(llm_config.clone());

    let resolution = services
        .invoke(
            caller(),
            CapabilityId::new(OPERATOR_SETUP_RUN_CAPABILITY_ID).expect("capability id"),
            json!({
                "provider_id": "openai",
                "adapter": "open_ai_completions",
                "base_url": "https://api.example.test/v1",
                "model": "gpt-5-mini",
                "api_key": "sk-secret"
            }),
            ActivityId::new(),
        )
        .await
        .expect("operator setup capability");

    assert!(matches!(
        resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));
    assert_eq!(llm_config.snapshot_count(), 1);
    assert_eq!(
        llm_config
            .upsert_provider_calls
            .lock()
            .expect("lock")
            .as_slice(),
        [SetupUpsertCall {
            id: "openai".to_string(),
            adapter: "open_ai_completions".to_string(),
            base_url: Some("https://api.example.test/v1".to_string()),
            default_model: Some("gpt-5-mini".to_string()),
            api_key_set: true,
            set_active: true,
            model: Some("gpt-5-mini".to_string()),
        }]
    );
}

#[tokio::test]
async fn test_llm_connection_allows_loopback_base_url_for_self_hosted() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    services
        .test_llm_connection(
            caller(),
            LlmProbeRequest {
                adapter: "ollama".to_string(),
                base_url: Some("http://127.0.0.1:11434/v1".to_string()),
                provider_id: "ollama".to_string(),
                model: Some("qwen3:latest".to_string()),
                api_key: None,
            },
        )
        .await
        .expect("loopback probe reaches the service");

    assert_eq!(llm_config.test_connection_count(), 1);
}

#[tokio::test]
async fn list_llm_models_allows_localhost_base_url_for_self_hosted() {
    // Regression: `validate_llm_base_url` used to reject `localhost`, breaking
    // the "Fetch models" button for self-hosted Ollama (the dialog showed
    // "Invalid value (base_url)").
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    services
        .list_llm_models(
            caller(),
            LlmProbeRequest {
                adapter: "ollama".to_string(),
                base_url: Some("http://localhost:11434".to_string()),
                provider_id: "ollama".to_string(),
                model: None,
                api_key: None,
            },
        )
        .await
        .expect("localhost probe reaches the service");

    assert_eq!(llm_config.list_models_count(), 1);
}

#[tokio::test]
async fn list_llm_models_rejects_internal_base_url_before_service() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .list_llm_models(
            caller(),
            LlmProbeRequest {
                adapter: "open_ai_completions".to_string(),
                base_url: Some("http://169.254.169.254/latest/meta-data/".to_string()),
                provider_id: "openai".to_string(),
                model: Some("gpt-5-mini".to_string()),
                api_key: Some(SecretString::from("sk-secret".to_string())),
            },
        )
        .await
        .expect_err("metadata probe endpoint is rejected");

    assert_setup_validation(err, "base_url", ProductSurfaceValidationCode::InvalidValue);
    assert_eq!(llm_config.list_models_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_upserts_and_activates_provider_config() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    llm_config.use_active_snapshot("openai", "gpt-5-mini");
    let services = services_with_setup_llm_config(llm_config.clone());

    let response = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_adapter("open_ai_completions")
                .set_base_url("https://api.example.test/v1")
                .set_model("gpt-5-mini")
                .set_api_key(SecretString::from("sk-secret".to_string())),
        )
        .await
        .expect("setup response");

    assert_eq!(response.status, RebornOperatorSetupStatus::Complete);
    assert_eq!(llm_config.snapshot_count(), 1);
    assert_eq!(
        llm_config
            .upsert_provider_calls
            .lock()
            .expect("lock")
            .as_slice(),
        [SetupUpsertCall {
            id: "openai".to_string(),
            adapter: "open_ai_completions".to_string(),
            base_url: Some("https://api.example.test/v1".to_string()),
            default_model: Some("gpt-5-mini".to_string()),
            api_key_set: true,
            set_active: true,
            model: Some("gpt-5-mini".to_string()),
        }]
    );
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_ignores_redacted_webui_access_token_sentinel() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    llm_config.use_active_snapshot("openai", "gpt-5-mini");
    let services = services_with_setup_llm_config(llm_config.clone());

    let response = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_model("gpt-5-mini")
                .set_webui_access_token(SecretString::from("••••••••".to_string())),
        )
        .await
        .expect("setup response");

    assert_eq!(response.status, RebornOperatorSetupStatus::Complete);
    let webui_step = response
        .steps
        .iter()
        .find(|step| step.name == "webui_access")
        .expect("webui access step");
    assert_eq!(
        webui_step.status,
        ironclaw_assistant::RebornOperatorSetupStepStatus::Complete
    );
    assert_eq!(
        llm_config.set_active_calls.lock().expect("lock").as_slice(),
        [SetupSetActiveCall {
            provider_id: "openai".to_string(),
            model: Some("gpt-5-mini".to_string()),
        }]
    );
    let serialized = serde_json::to_string(&response).expect("serialize setup response");
    assert!(!serialized.contains("••••••••"));
}

#[tokio::test]
async fn run_operator_setup_rejects_unwired_host_mutations_before_provider_write() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_adapter("open_ai_completions")
                .set_profile_id("production")
                .set_webui_access_token(SecretString::from(
                    "webui-secret-token-value-32-bytes".to_string(),
                )),
        )
        .await
        .expect_err("unwired host mutations fail closed");

    assert_eq!(err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(err.kind, ProductSurfaceErrorKind::ServiceUnavailable);
    assert_eq!(err.status_code, 503);
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_rejects_profile_only_host_mutation_before_provider_write() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_adapter("open_ai_completions")
                .set_profile_id("production"),
        )
        .await
        .expect_err("unwired profile mutation fails closed");

    assert_eq!(err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(err.kind, ProductSurfaceErrorKind::ServiceUnavailable);
    assert_eq!(err.status_code, 503);
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_rejects_token_only_host_mutation_before_provider_write() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    let err = services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_adapter("open_ai_completions")
                .set_webui_access_token(SecretString::from(
                    "webui-secret-token-value-32-bytes".to_string(),
                )),
        )
        .await
        .expect_err("unwired WebUI token mutation fails closed");

    assert_eq!(err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(err.kind, ProductSurfaceErrorKind::ServiceUnavailable);
    assert_eq!(err.status_code, 503);
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_selects_existing_provider_without_adapter() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());

    services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_model("gpt-5-mini"),
        )
        .await
        .expect("setup response");

    assert_eq!(llm_config.snapshot_count(), 1);
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(
        llm_config.set_active_calls.lock().expect("lock").as_slice(),
        [SetupSetActiveCall {
            provider_id: "openai".to_string(),
            model: Some("gpt-5-mini".to_string()),
        }]
    );
}

#[tokio::test]
async fn run_operator_setup_without_provider_change_returns_snapshot() {
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    llm_config.use_active_snapshot("openai", "gpt-5-mini");
    let services = services_with_setup_llm_config(llm_config.clone());

    let response = services
        .run_operator_setup(caller(), RebornOperatorSetupRequest::default())
        .await
        .expect("setup response");

    assert_eq!(response.status, RebornOperatorSetupStatus::Complete);
    assert_eq!(llm_config.snapshot_count(), 1);
    assert_eq!(llm_config.upsert_provider_count(), 0);
    assert_eq!(llm_config.set_active_count(), 0);
}

#[tokio::test]
async fn run_operator_setup_propagates_llm_config_service_error() {
    let upsert_config = Arc::new(SetupRecordingLlmConfigService::default());
    upsert_config.fail_next_upsert(LlmConfigServiceError::Unavailable);
    let upsert_services = services_with_setup_llm_config(upsert_config);
    let upsert_err = upsert_services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default()
                .set_provider_id("openai")
                .set_adapter("open_ai_completions"),
        )
        .await
        .expect_err("upsert error propagates");
    assert_eq!(upsert_err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(upsert_err.status_code, 503);

    let set_active_config = Arc::new(SetupRecordingLlmConfigService::default());
    set_active_config.fail_next_set_active(LlmConfigServiceError::Unavailable);
    let set_active_services = services_with_setup_llm_config(set_active_config);
    let set_active_err = set_active_services
        .run_operator_setup(
            caller(),
            RebornOperatorSetupRequest::default().set_provider_id("openai"),
        )
        .await
        .expect_err("set_active error propagates");
    assert_eq!(set_active_err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(set_active_err.status_code, 503);

    let snapshot_config = Arc::new(SetupRecordingLlmConfigService::default());
    snapshot_config.fail_next_snapshot(LlmConfigServiceError::Unavailable);
    let snapshot_services = services_with_setup_llm_config(snapshot_config);
    let snapshot_err = snapshot_services
        .run_operator_setup(caller(), RebornOperatorSetupRequest::default())
        .await
        .expect_err("snapshot error propagates");
    assert_eq!(snapshot_err.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(snapshot_err.status_code, 503);
}

/// The three vendor-login paths, driven through `RebornServices`, on their
/// failure half.
///
/// WS5 moved `LlmConfigServiceError`'s projection onto `ProductSurfaceError`
/// out of product (`map_llm_config_error`) and into the `From` impl declared
/// beside the port, so these three `.map_err(ProductSurfaceError::from)` call
/// sites are what proves product still answers with the sanitized taxonomy --
/// and there is a wrapper plus an `Option` unwrap between the port and the
/// answer, so a direct test of the `From` impl would not prove it
/// (`.claude/rules/testing.md`, "Test through the caller").
///
/// `Unavailable` is the arm that matters: it is the only retryable one, so a
/// projection that lost it would turn a transient backend failure into a
/// permanent-looking error in the Inference tab.
#[tokio::test]
async fn vendor_login_failures_project_the_sanitized_port_taxonomy() {
    let nearai_request = || NearAiLoginRequest {
        provider: ironclaw_product_contracts::operator_llm::NearAiAuthProvider::Github,
        origin: "https://app.example".to_string(),
    };
    let wallet_request = || NearAiWalletLoginRequest {
        account_id: "alice.near".to_string(),
        public_key: "ed25519:key".to_string(),
        signature: "c2ln".to_string(),
        message: "login".to_string(),
        recipient: "ironclaw".to_string(),
        nonce: vec![0u8; 32],
        callback_url: None,
    };

    // Unavailable -> 503, retryable.
    let llm_config = Arc::new(SetupRecordingLlmConfigService::default());
    let services = services_with_setup_llm_config(llm_config.clone());
    llm_config.fail_next_login(LlmConfigServiceError::Unavailable);
    let error = services
        .start_nearai_login(caller(), nearai_request())
        .await
        .expect_err("an unavailable backend must fail the login");
    assert_eq!(error.status_code, 503);
    assert!(error.retryable, "Unavailable is the one retryable arm");

    llm_config.fail_next_login(LlmConfigServiceError::Unavailable);
    let error = services
        .start_codex_login(caller())
        .await
        .expect_err("an unavailable backend must fail the codex login");
    assert_eq!(error.status_code, 503);
    assert!(error.retryable);

    llm_config.fail_next_login(LlmConfigServiceError::Unavailable);
    let error = services
        .complete_nearai_wallet_login(caller(), wallet_request())
        .await
        .expect_err("an unavailable backend must fail the wallet login");
    assert_eq!(error.status_code, 503);
    assert!(error.retryable);

    // The non-retryable arms keep their own statuses, and none of them leaks a
    // backend string: `InvalidRequest`'s reason is dropped by the projection.
    for (arm, status) in [
        (
            LlmConfigServiceError::InvalidRequest {
                field: Some("provider".to_string()),
                reason: "backend said /var/lib/ironclaw is unreadable".to_string(),
            },
            400,
        ),
        (LlmConfigServiceError::NotFound, 404),
        (LlmConfigServiceError::Internal, 500),
    ] {
        llm_config.fail_next_login(arm);
        let error = services
            .start_nearai_login(caller(), nearai_request())
            .await
            .expect_err("the armed error must surface");
        assert_eq!(error.status_code, status);
        assert!(!error.retryable, "only Unavailable is retryable");
        let on_the_wire = serde_json::to_string(&error).expect("surface error serializes");
        assert!(
            !on_the_wire.contains("/var/lib/ironclaw"),
            "no backend string may cross the membrane: {on_the_wire}"
        );
    }

    // And the success half still works, so the double is not simply broken.
    let start = services
        .start_nearai_login(caller(), nearai_request())
        .await
        .expect("a healthy backend starts the login");
    assert!(start.auth_url.contains("/v1/auth/github"));
}
