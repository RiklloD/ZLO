//! Skill context sources, filesystem skill roots, explicit/setup skill
//! activation, and skill execution.

use super::*;

#[tokio::test]
async fn standalone_runtime_wires_input_skill_context_source_to_model_calls() {
    let root = tempfile::tempdir().expect("tempdir");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "skill context ok".to_string(),
        requests: Arc::clone(&requests),
    });
    let skill_source = Arc::new(StaticSkillContextSource::new(vec![
        HostSkillContextCandidate::loaded(
            skill_md(
                "review-helper",
                "review helper description",
                "Use review helper prompt content.",
            ),
            Some(SkillTrust::Trusted),
            Some(SkillVisibility::Visible),
        ),
    ]));
    let skill_context_source: Arc<dyn HostSkillContextSource> = skill_source;
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-skill-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-skill-tenant".to_string(),
        agent_id: "runtime-skill-agent".to_string(),
        source_binding_id: "runtime-skill-source".to_string(),
        reply_target_binding_id: "runtime-skill-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_skill_context_source(skill_context_source)
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "review this"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");

    assert_eq!(reply.status, TurnStatus::Completed);
    assert_eq!(reply.text.as_deref(), Some("skill context ok"));
    let (request_count, skill_message_content) = {
        let requests = requests
            .lock()
            .expect("recording gateway requests lock poisoned");
        let skill_message = requests[0]
            .messages
            .iter()
            .find(|message| {
                message.role == HostManagedModelMessageRole::System
                    && message
                        .content_ref
                        .as_str()
                        .starts_with("msg:snippet.skill.review-helper.")
            })
            .expect("model request should include skill-context system message");
        (requests.len(), skill_message.content.clone())
    };
    assert_eq!(request_count, 1);
    assert!(skill_message_content.contains("review helper description"));
    assert!(skill_message_content.contains("Use review helper prompt content."));

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_runtime_prefers_configured_skill_context_source_over_filesystem_default() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    std::fs::create_dir_all(storage_root.join("system/skills/filesystem-helper"))
        .expect("filesystem skill dir");
    std::fs::write(
        storage_root.join("system/skills/filesystem-helper/SKILL.md"),
        skill_md(
            "filesystem-helper",
            "filesystem helper description",
            "FILESYSTEM_HELPER_PROMPT_SENTINEL",
        ),
    )
    .expect("write filesystem skill");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "configured skill context ok".to_string(),
        requests: Arc::clone(&requests),
    });
    let skill_source = Arc::new(StaticSkillContextSource::new(vec![
        HostSkillContextCandidate::loaded(
            skill_md(
                "configured-helper",
                "configured helper description",
                "CONFIGURED_HELPER_PROMPT_SENTINEL",
            ),
            Some(SkillTrust::Trusted),
            Some(SkillVisibility::Visible),
        ),
    ]));
    let skill_context_source: Arc<dyn HostSkillContextSource> = skill_source;
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-skill-override-owner",
            storage_root,
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-skill-override-tenant".to_string(),
        agent_id: "runtime-skill-override-agent".to_string(),
        source_binding_id: "runtime-skill-override-source".to_string(),
        reply_target_binding_id: "runtime-skill-override-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_skill_context_source(skill_context_source)
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "review this"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");

    assert_eq!(reply.status, TurnStatus::Completed);
    assert_eq!(reply.text.as_deref(), Some("configured skill context ok"));
    let combined_skill_context = {
        let requests = requests
            .lock()
            .expect("recording gateway requests lock poisoned");
        requests[0]
            .messages
            .iter()
            .filter(|message| {
                message.role == HostManagedModelMessageRole::System
                    && message
                        .content_ref
                        .as_str()
                        .starts_with("msg:snippet.skill.")
            })
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(combined_skill_context.contains("configured helper description"));
    assert!(combined_skill_context.contains("CONFIGURED_HELPER_PROMPT_SENTINEL"));
    assert!(!combined_skill_context.contains("filesystem helper description"));
    assert!(!combined_skill_context.contains("FILESYSTEM_HELPER_PROMPT_SENTINEL"));

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_runtime_wires_filesystem_skills_by_default_to_model_calls() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    std::fs::create_dir_all(storage_root.join("system/skills/system-helper"))
        .expect("system skill dir");
    std::fs::write(
        storage_root.join("system/skills/system-helper/SKILL.md"),
        skill_md(
            "system-helper",
            "system helper description",
            "SYSTEM_HELPER_PROMPT_SENTINEL",
        ),
    )
    .expect("write system skill");
    seed_user_skill(
        &storage_root,
        "runtime-filesystem-skill-tenant",
        "runtime-filesystem-skill-owner",
        "local-helper",
        skill_md(
            "local-helper",
            "local helper description",
            "USER_HELPER_PROMPT_SENTINEL",
        ),
    )
    .await;
    seed_tenant_shared_skill(
        &storage_root,
        "runtime-filesystem-skill-tenant",
        "shared-helper",
        skill_md(
            "shared-helper",
            "tenant shared helper description",
            "TENANT_SHARED_PROMPT_SENTINEL",
        ),
    )
    .await;
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "filesystem skill context ok".to_string(),
        requests: Arc::clone(&requests),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-filesystem-skill-owner",
            storage_root,
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-filesystem-skill-tenant".to_string(),
        agent_id: "runtime-filesystem-skill-agent".to_string(),
        source_binding_id: "runtime-filesystem-skill-source".to_string(),
        reply_target_binding_id: "runtime-filesystem-skill-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_POLL_TIMEOUT,
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "/system-helper and /local-helper"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");

    assert_eq!(reply.status, TurnStatus::Completed);
    assert_eq!(reply.text.as_deref(), Some("filesystem skill context ok"));
    let skill_messages = {
        let requests = requests
            .lock()
            .expect("recording gateway requests lock poisoned");
        requests[0]
            .messages
            .iter()
            .filter(|message| {
                message.role == HostManagedModelMessageRole::System
                    && message
                        .content_ref
                        .as_str()
                        .starts_with("msg:snippet.skill.")
            })
            .map(|message| message.content.clone())
            .collect::<Vec<_>>()
    };
    let combined_skill_context = skill_messages.join("\n");
    // Default `listing` injection: the two explicitly-mentioned skills load
    // their full bodies, and every other visible skill (the bundled system
    // skills) collapses into ONE `available-skills` listing message.
    assert_eq!(skill_messages.len(), 3);
    assert!(combined_skill_context.contains("system helper description"));
    assert!(combined_skill_context.contains("SYSTEM_HELPER_PROMPT_SENTINEL"));
    assert!(combined_skill_context.contains("local helper description"));
    assert!(combined_skill_context.contains("USER_HELPER_PROMPT_SENTINEL"));
    assert!(!combined_skill_context.contains("tenant shared helper description"));
    assert!(!combined_skill_context.contains("TENANT_SHARED_PROMPT_SENTINEL"));
    assert!(
        combined_skill_context.contains("builtin.skill_activate"),
        "available-skills listing message must reach the model"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_runtime_backfills_legacy_owner_skill_root() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    std::fs::create_dir_all(storage_root.join("skills/legacy-helper")).expect("legacy skill dir");
    std::fs::write(
        storage_root.join("skills/legacy-helper/SKILL.md"),
        skill_md(
            "legacy-helper",
            "legacy helper description",
            "LEGACY_HELPER_PROMPT_SENTINEL",
        ),
    )
    .expect("write legacy helper skill");

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-legacy-skill-owner",
            storage_root.clone(),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    );
    let runtime = build_reborn_runtime(input).await.expect("runtime");
    let conversation = runtime.new_conversation().await.expect("conversation");

    let result = runtime
        .execute_skill_message(&conversation, "$legacy-helper")
        .await
        .expect("execute skill message");

    assert_eq!(result.plan.activations().len(), 1);
    assert_eq!(result.plan.activations()[0].name, "legacy-helper");
    // The legacy tree is migrated onto the host disk and then imported into the DATABASE, which is
    // the only tree skills are read from. Both are asserted: the disk copy is deliberately left in
    // place so a downgrade is not destructive, and the database copy is what makes the skill usable.
    assert!(
        storage_root
            .join(
                "tenants/reborn-cli/users/runtime-legacy-skill-owner/skills/legacy-helper/SKILL.md"
            )
            .exists(),
        "the on-disk legacy migration must still run, so a downgrade keeps the skill"
    );
    assert!(
        crate::filesystem_assembly::database_file_bytes(
            &storage_root,
            "/tenants/reborn-cli/users/runtime-legacy-skill-owner/skills/legacy-helper/SKILL.md",
        )
        .await
        .is_some(),
        "a legacy skill must be imported into the database-backed tree, or upgrading silently loses \
         every skill the user already had (nearai/ironclaw#7168)"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn execute_skill_message_returns_plan_and_reads_active_bundle_assets() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    seed_user_skill_with_files(
        &storage_root,
        "runtime-skill-exec-tenant",
        "runtime-skill-exec-owner",
        "asset-helper",
        skill_md(
            "asset-helper",
            "asset helper description",
            "ASSET_HELPER_PROMPT_SENTINEL",
        ),
        &[("references/policy.md", "asset helper policy".to_string())],
    )
    .await;
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "asset helper ok".to_string(),
        requests: Arc::clone(&requests),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input("runtime-skill-exec-owner", storage_root)
            .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-skill-exec-tenant".to_string(),
        agent_id: "runtime-skill-exec-agent".to_string(),
        source_binding_id: "runtime-skill-exec-source".to_string(),
        reply_target_binding_id: "runtime-skill-exec-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let result = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.execute_skill_message(&conversation, "$asset-helper use policy"),
    )
    .await
    .expect("skill execution should finish")
    .expect("skill execution should succeed");

    assert_eq!(result.reply.status, TurnStatus::Completed);
    assert_eq!(result.reply.text.as_deref(), Some("asset helper ok"));
    assert_eq!(result.plan.activations().len(), 1);
    assert_eq!(result.plan.activations()[0].name, "asset-helper");
    assert_eq!(
        result.plan.activations()[0].source,
        Some(RebornSkillActivationSource::User)
    );
    assert_eq!(result.plan.active_bundles().len(), 1);
    assert_eq!(result.plan.active_bundles()[0].skill_name, "asset-helper");
    assert_eq!(
        result.plan.run_context().run_id,
        result.reply.run_id,
        "post-activation asset reads must reuse the real activation run context"
    );
    let asset = runtime
        .read_skill_execution_asset(
            &conversation,
            &result.plan,
            &result.plan.activations()[0],
            "references/policy.md",
        )
        .await
        .expect("active bundle asset read succeeds");

    assert_eq!(asset.skill_name, "asset-helper");
    assert_eq!(asset.path, "references/policy.md");
    assert_eq!(asset.into_utf8().unwrap(), "asset helper policy");

    let other_conversation = runtime
        .new_conversation()
        .await
        .expect("other conversation");
    let error = runtime
        .read_skill_execution_asset(
            &other_conversation,
            &result.plan,
            &result.plan.activations()[0],
            "references/policy.md",
        )
        .await
        .expect_err("plan should be bound to its activation conversation");
    assert!(
        error
            .to_string()
            .contains("skill execution plan does not belong to this conversation"),
        "unexpected error: {error}"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_runtime_fails_closed_for_ambiguous_explicit_skill_before_model_call() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    std::fs::create_dir_all(storage_root.join("system/skills/code-review"))
        .expect("system skill dir");
    std::fs::write(
        storage_root.join("system/skills/code-review/SKILL.md"),
        skill_md(
            "code-review",
            "system review description",
            "SYSTEM_REVIEW_PROMPT_SENTINEL",
        ),
    )
    .expect("write system skill");
    seed_user_skill(
        &storage_root,
        "runtime-ambiguous-skill-tenant",
        "runtime-ambiguous-skill-owner",
        "code-review",
        skill_md(
            "code-review",
            "user review description",
            "USER_REVIEW_PROMPT_SENTINEL",
        ),
    )
    .await;
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "should not reach model".to_string(),
        requests: Arc::clone(&requests),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-ambiguous-skill-owner",
            storage_root,
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-ambiguous-skill-tenant".to_string(),
        agent_id: "runtime-ambiguous-skill-agent".to_string(),
        source_binding_id: "runtime-ambiguous-skill-source".to_string(),
        reply_target_binding_id: "runtime-ambiguous-skill-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_POLL_TIMEOUT,
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "/code-review this PR"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");

    assert_ne!(reply.status, TurnStatus::Completed);
    assert!(
        requests
            .lock()
            .expect("recording gateway requests lock poisoned")
            .is_empty(),
        "ambiguous explicit skill should fail before model dispatch"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_runtime_suppresses_explicit_setup_skill_when_workspace_marker_exists() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    std::fs::create_dir_all(storage_root.join("workspace/markers")).expect("marker dir");
    seed_user_skill(
        &storage_root,
        "runtime-setup-marker-tenant",
        "runtime-setup-marker-owner",
        "marker-helper",
        skill_md_with_setup_marker(
            "marker-helper",
            "marker helper description",
            "markers/marker-helper.done",
            "MARKER_HELPER_PROMPT_SENTINEL",
        ),
    )
    .await;
    std::fs::write(
        storage_root.join("workspace/markers/marker-helper.done"),
        "done",
    )
    .expect("write setup marker");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "setup marker ok".to_string(),
        requests: Arc::clone(&requests),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input("runtime-setup-marker-owner", storage_root)
            .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-setup-marker-tenant".to_string(),
        agent_id: "runtime-setup-marker-agent".to_string(),
        source_binding_id: "runtime-setup-marker-source".to_string(),
        reply_target_binding_id: "runtime-setup-marker-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_POLL_TIMEOUT,
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let result = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.execute_skill_message(&conversation, "$marker-helper"),
    )
    .await
    .expect("skill execution should finish")
    .expect("skill execution should succeed");

    assert_eq!(result.reply.status, TurnStatus::Completed);
    assert!(result.plan.activations().is_empty());
    // The setup skill's body must not reach the model when its marker is
    // already satisfied. The always-present one-line available-skills
    // listing snippet (`msg:snippet.skill.available-skills.*`) may still
    // advertise the skill's short description, but the full SKILL.md body —
    // pinned by MARKER_HELPER_PROMPT_SENTINEL — only ships on activation.
    let skill_context = {
        let requests = requests
            .lock()
            .expect("recording gateway requests lock poisoned");
        requests[0]
            .messages
            .iter()
            .filter(|message| {
                message.role == HostManagedModelMessageRole::System
                    && message
                        .content_ref
                        .as_str()
                        .starts_with("msg:snippet.skill.")
            })
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(
        !skill_context.contains("MARKER_HELPER_PROMPT_SENTINEL"),
        "suppressed setup skill body must not be injected, got: {skill_context}"
    );

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_runtime_activates_setup_skill_when_workspace_marker_is_absent() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    seed_user_skill(
        &storage_root,
        "runtime-setup-marker-absent-tenant",
        "runtime-setup-marker-absent-owner",
        "marker-helper",
        skill_md_with_setup_marker(
            "marker-helper",
            "marker helper description",
            "markers/marker-helper.done",
            "MARKER_HELPER_PROMPT_SENTINEL",
        ),
    )
    .await;
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "setup marker absent ok".to_string(),
        requests: Arc::clone(&requests),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-setup-marker-absent-owner",
            storage_root,
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-setup-marker-absent-tenant".to_string(),
        agent_id: "runtime-setup-marker-absent-agent".to_string(),
        source_binding_id: "runtime-setup-marker-absent-source".to_string(),
        reply_target_binding_id: "runtime-setup-marker-absent-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(3),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let result = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.execute_skill_message(&conversation, "$marker-helper"),
    )
    .await
    .expect("skill execution should finish")
    .expect("skill execution should succeed");

    assert_eq!(result.reply.status, TurnStatus::Completed);
    assert_eq!(result.plan.activations().len(), 1);
    assert_eq!(result.plan.activations()[0].name, "marker-helper");
    let skill_context = {
        let requests = requests
            .lock()
            .expect("recording gateway requests lock poisoned");
        requests[0]
            .messages
            .iter()
            .filter(|message| {
                message.role == HostManagedModelMessageRole::System
                    && message
                        .content_ref
                        .as_str()
                        .starts_with("msg:snippet.skill.")
            })
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(skill_context.contains("marker helper description"));
    assert!(skill_context.contains("MARKER_HELPER_PROMPT_SENTINEL"));

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_runtime_rejects_workspace_overlapping_default_skill_roots() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    let workspace_root = storage_root.join("skills");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "should not build".to_string(),
        requests,
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input("runtime-overlap-owner", storage_root)
            .with_local_runtime_workspace_root(workspace_root)
            .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-overlap-tenant".to_string(),
        agent_id: "runtime-overlap-agent".to_string(),
        source_binding_id: "runtime-overlap-source".to_string(),
        reply_target_binding_id: "runtime-overlap-reply".to_string(),
    })
    .with_model_gateway_override(gateway);

    let error = match build_reborn_runtime(input).await {
        Ok(runtime) => {
            runtime.shutdown().await.expect("runtime shutdown");
            panic!("overlapping workspace and skill roots should fail closed");
        }
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("must not overlap default skill root /skills"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn standalone_runtime_skips_invalid_filesystem_skill_before_model_call() {
    // This exercises a complete filesystem-backed runtime bootstrap. Under the
    // crate's parallel test load that can exceed the short poll budget used by
    // smaller scenarios, even though the model path itself completes promptly.
    const INVALID_SKILL_TEST_TIMEOUT: Duration = Duration::from_secs(15);

    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    seed_user_skill(
        &storage_root,
        "runtime-bad-skill-tenant",
        "runtime-bad-skill-owner",
        "bad-helper",
        skill_md(
            "different-name",
            "bad helper description",
            "BAD_HELPER_PROMPT_SENTINEL",
        ),
    )
    .await;
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "invalid skill skipped".to_string(),
        requests: Arc::clone(&requests),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input("runtime-bad-skill-owner", storage_root)
            .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-bad-skill-tenant".to_string(),
        agent_id: "runtime-bad-skill-agent".to_string(),
        source_binding_id: "runtime-bad-skill-source".to_string(),
        reply_target_binding_id: "runtime-bad-skill-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: INVALID_SKILL_TEST_TIMEOUT,
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "hello with no matching skill"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");

    assert_eq!(reply.status, TurnStatus::Completed);
    assert_eq!(reply.text.as_deref(), Some("invalid skill skipped"));
    let combined_request_content = requests
        .lock()
        .expect("recording gateway requests lock poisoned")
        .iter()
        .flat_map(|request| request.messages.iter())
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!combined_request_content.contains("BAD_HELPER_PROMPT_SENTINEL"));

    runtime.shutdown().await.expect("runtime shutdown");
}

/// Thread 1 installs a skill carrying a script; thread 2 activates it and the script RUNS.
///
/// The assertion that matters is the last one: the staged path handed to the model is fed to a real
/// process, and the process prints the script's own marker. Every previous version of this flow
/// satisfied "the skill exists" and still could not run.
#[tokio::test]
async fn thread_one_authors_a_scripted_skill_and_thread_two_executes_it() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");

    // ── Thread 1: author ────────────────────────────────────────────────────────────────────────
    let runtime = build_reborn_runtime(two_thread_runtime_input(storage_root.clone()))
        .await
        .expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");

    let installed = invoke_product_capability(
        bundle.as_ref(),
        two_thread_caller(),
        ironclaw_assistant::SKILL_INSTALL_CAPABILITY_ID,
        serde_json::json!({
            "name": "egfr-calc",
            "content": SKILL_MD,
            // `text` is the schema's field name for a UTF-8 bundle file; `bytes_base64` is the binary form.
            "files": [{"path": "scripts/egfr.py", "text": SKILL_SCRIPT}],
        }),
    )
    .await
    .expect("skill install dispatches");
    assert!(
        matches!(&installed, ironclaw_host_api::resolution::Resolution::Done(outcome) if outcome.verdict.is_success()),
        "installing a skill with a bundled script must succeed, got {installed:?}"
    );

    runtime.shutdown().await.expect("thread one shutdown");

    // ── Thread 2: a NEW runtime over the SAME store, i.e. a later conversation ──────────────────
    // Rebuilt rather than reused on purpose: the reported bug was that a skill survived its own
    // session and nothing else, so a fixture that keeps one runtime alive cannot see it.
    let runtime = build_reborn_runtime(two_thread_runtime_input(storage_root.clone()))
        .await
        .expect("runtime rebuilds over the same store");
    let conversation = runtime
        .new_conversation()
        .await
        .expect("thread two opens a conversation");

    let result = runtime
        .execute_skill_message(&conversation, "$egfr-calc")
        .await
        .expect("thread two executes a skill message");
    let activated: Vec<String> = result
        .plan
        .activations()
        .iter()
        .map(|activation| activation.name.to_string())
        .collect();
    assert!(
        activated.iter().any(|name| name == "egfr-calc"),
        "a skill authored in thread one must activate in thread two; got {activated:?}"
    );

    // ── The payoff: the staged bundle must be a real, runnable path ─────────────────────────────
    //
    // Located by search rather than by assuming a layout: `/workspace` resolves to the shared root
    // under the standalone policy and to `<root>/tenants/<t>/users/<u>` under a per-caller one, and a
    // fixture that hardcodes either spelling tests the spelling instead of the mechanism.
    fn find_staged_script(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                find_staged_script(&path, out);
            } else if path.ends_with(".skills/egfr-calc/scripts/egfr.py") {
                out.push(path);
            }
        }
    }
    // NOTE: this fixture cannot catch a wrong ADVERTISED path. Staging writes through the caller's
    // own view, so the bytes land correctly even when the string handed to the model is wrong -- which
    // is exactly what shipped once (`/workspace/tenants/<t>/users/<u>/.skills/<name>`, resolved a
    // second time beneath the per-caller root, a directory that does not exist). That string is pinned
    // where it is produced, by `runnable_dir_tests` in ironclaw_first_party_extension_ports.
    let mut staged = Vec::new();
    find_staged_script(&storage_root.join("workspace"), &mut staged);
    assert_eq!(
        staged.len(),
        1,
        "activation must stage the bundle exactly once somewhere a process can open it; found \
         {staged:?} under {}",
        storage_root.join("workspace").display()
    );
    let staged_script = staged.remove(0);
    let staged_dir = staged_script
        .parent()
        .and_then(|scripts| scripts.parent())
        .expect("staged script sits at <skill>/scripts/egfr.py")
        .to_path_buf();

    // Run it exactly as the skill body says, from the staged directory. If this fails, an agent
    // following its own skill's instructions fails too -- which is what ten demo runs did.
    let output = std::process::Command::new("python3")
        .arg("scripts/egfr.py")
        .args(["--scr", "1.3", "--age", "62", "--female"])
        .current_dir(&staged_dir)
        .output()
        .expect("python3 must be available to run the staged script");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the staged script must execute: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("FIXTURE-OK") && stdout.contains("stage=G3a"),
        "the answer must come from the script itself, not re-derived arithmetic; got {stdout:?}"
    );

    runtime.shutdown().await.expect("thread two shutdown");
}

/// The demo end to end, with the model mocked and everything around it real.
///
/// Thread 1 installs a skill carrying a script; thread 2 is a NEW runtime over the same store whose
/// mocked model reads the advertised workdir and runs the command through the real `builtin.shell`.
/// Closes the half the sibling fixture cannot see: the path the model is TOLD. When that string was
/// wrong, every command failed with `Failed to spawn command` and nothing noticed.
// Debug builds nest this full shell turn deeper than libtest's 2 MB thread
// stack; `serve` gives its workers 8 MB, so the test does the same.
#[test]
fn the_model_runs_a_skills_script_from_the_workdir_the_body_advertises() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime")
                .block_on(skills_script_from_advertised_workdir())
        })
        .expect("test thread spawns")
        .join()
        .expect("test thread does not panic");
}

async fn skills_script_from_advertised_workdir() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");

    // ── Thread 1: author the skill, script and all ─────────────────────────────────────────────
    let runtime = build_reborn_runtime(two_thread_runtime_input(storage_root.clone()))
        .await
        .expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");
    invoke_product_capability(
        bundle.as_ref(),
        two_thread_caller(),
        ironclaw_assistant::SKILL_INSTALL_CAPABILITY_ID,
        serde_json::json!({
            "name": "egfr-calc",
            "content": SKILL_MD,
            "files": [{"path": "scripts/egfr.py", "text": SKILL_SCRIPT}],
        }),
    )
    .await
    .expect("skill install dispatches");
    runtime.shutdown().await.expect("thread one shutdown");

    // ── Thread 2: a later conversation, driven by the mocked model ─────────────────────────────
    let gateway = Arc::new(SkillShellGateway::default());
    let runtime = build_reborn_runtime(
        two_thread_runtime_input(storage_root.clone()).with_model_gateway_override(gateway.clone()),
    )
    .await
    .expect("runtime rebuilds over the same store");
    let conversation = runtime
        .new_conversation()
        .await
        .expect("thread two opens a conversation");
    runtime
        .execute_skill_message(&conversation, "$egfr-calc")
        .await
        .expect("thread two executes a skill message");

    let workdir = gateway
        .advertised_workdir
        .lock()
        .expect("workdir lock")
        .clone()
        .expect("the model must have been given a staged workdir");
    assert_eq!(
        workdir, "/workspace/.skills/egfr-calc",
        "the body must advertise the plain workspace spelling; any per-caller segment here is \
         resolved a second time by the shell and the directory does not exist"
    );

    let shell_output = gateway
        .shell_output
        .lock()
        .expect("shell output lock")
        .clone()
        .expect("the shell call must have produced a result the model could read");
    assert!(
        !shell_output.contains("Failed to spawn command")
            && !shell_output.contains("No such file or directory"),
        "the shell must resolve the advertised workdir; got {shell_output}"
    );
    assert!(
        shell_output.contains("FIXTURE-OK") && shell_output.contains("stage=G3a"),
        "the answer must come from the staged script itself, through the shell the model called, \
         not from re-derived arithmetic; got {shell_output}"
    );

    runtime.shutdown().await.expect("thread two shutdown");
}

/// A manifest with no `description:` must not become an invisible skill.
///
/// Measured with a real model: asked to save a reusable skill, it wrote frontmatter carrying `name:`
/// alone. The install succeeded, Settings listed it, and every later discovery pass skipped it with
/// only a `warn!` — so the skill existed and could never be used again.
#[tokio::test]
async fn a_skill_installed_without_a_description_is_still_discoverable() {
    let root = tempfile::tempdir().expect("tempdir");
    let storage_root = root.path().join("standalone");
    let runtime = build_reborn_runtime(two_thread_runtime_input(storage_root))
        .await
        .expect("runtime builds");
    let bundle = runtime.product_surface(None).expect("product surface");

    invoke_product_capability(
        bundle.as_ref(),
        two_thread_caller(),
        ironclaw_assistant::SKILL_INSTALL_CAPABILITY_ID,
        serde_json::json!({
            "name": "no-description",
            "content": "---\nname: no-description\n---\n\nConvert lab values between conventional and SI units.\n",
        }),
    )
    .await
    .expect("install dispatches");

    let conversation = runtime.new_conversation().await.expect("conversation");
    let result = runtime
        .execute_skill_message(&conversation, "$no-description")
        .await
        .expect("execute skill message");
    let activated: Vec<String> = result
        .plan
        .activations()
        .iter()
        .map(|activation| activation.name.to_string())
        .collect();
    assert!(
        activated.iter().any(|name| name == "no-description"),
        "a description-less manifest must be repaired at the write, not silently skipped by \
         discovery forever; activated {activated:?}"
    );

    runtime.shutdown().await.expect("shutdown");
}
