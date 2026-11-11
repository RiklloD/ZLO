//! Admin user management and admin thread scraping.

use super::*;

fn admin_services(fake: FakeAdminUsers) -> RebornServices {
    session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_admin_user_service(Arc::new(fake))
}

fn assert_forbidden(err: ProductSurfaceError) {
    assert_eq!(err.status_code, 403, "expected a 403 authorization failure");
    assert_eq!(err.code, ProductSurfaceErrorCode::Forbidden);
}

#[tokio::test]
#[traced_test]
async fn admin_thread_scraping_reads_only_the_selected_users_threads() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_admin_user_service(Arc::new(FakeAdminUsers::with([
        admin_record("user-alpha", AdminUserRole::Admin, AdminUserStatus::Active),
        admin_record("user-beta", AdminUserRole::Member, AdminUserStatus::Active),
    ])))
    .with_operator_logs_service(Arc::new(RecordingOperatorLogsService::default()));
    setup_owned_thread(&services, caller(), "thread-admin").await;
    let target_caller = caller_for_user("user-beta");
    setup_owned_thread(&services, target_caller.clone(), "thread-target").await;
    let thread_id = ThreadId::new("thread-target").expect("thread id");
    let run_id = TurnRunId::parse(&run_id_string()).expect("run id");
    seed_submitted_message(
        &thread_service,
        &thread_scope_for(&target_caller),
        &thread_id,
        &run_id,
        "target trajectory",
    )
    .await;
    let target = UserId::new("user-beta").expect("target user");

    let threads = services
        .list_admin_thread_scrape_threads(
            caller(),
            RebornAdminThreadScrapeListRequest {
                user_id: target.clone(),
                limit: Some(50),
                cursor: None,
            },
        )
        .await
        .expect("admin thread scrape list");

    assert_eq!(threads.threads.len(), 1);
    assert_eq!(threads.threads[0].thread_id.as_str(), "thread-target");

    let artifact = services
        .build_admin_thread_scrape_artifact(
            caller(),
            RebornAdminThreadScrapeArtifactRequest {
                user_id: target.clone(),
                thread_id: "thread-target".to_string(),
            },
        )
        .await
        .expect("admin thread scrape artifact");
    assert_eq!(artifact.thread_id, "thread-target");

    let run_artifact = services
        .build_admin_thread_scrape_run_artifact(
            caller(),
            RebornAdminThreadScrapeRunArtifactRequest {
                user_id: target,
                thread_id: "thread-target".to_string(),
                run_id: run_id.to_string(),
            },
        )
        .await
        .expect("admin run scrape artifact");
    assert_eq!(run_artifact.thread_id, "thread-target");
    assert_eq!(run_artifact.run.run_id, run_id);
    assert_eq!(run_artifact.messages.len(), 1);
    assert_eq!(run_artifact.messages[0].content, "target trajectory");
    assert!(logs_contain(
        "action=\"threads_listed\" outcome=\"success\""
    ));
    assert!(logs_contain(
        "action=\"thread_artifact_exported\" outcome=\"success\""
    ));
    assert!(logs_contain(
        "action=\"run_artifact_exported\" outcome=\"success\""
    ));
}

#[tokio::test]
#[traced_test]
async fn admin_thread_scraping_rejects_a_non_admin_before_reading_threads() {
    let services = admin_services(FakeAdminUsers::with([
        admin_record("user-alpha", AdminUserRole::Member, AdminUserStatus::Active),
        admin_record("user-beta", AdminUserRole::Member, AdminUserStatus::Active),
    ]));

    let error = services
        .list_admin_thread_scrape_threads(
            caller(),
            RebornAdminThreadScrapeListRequest {
                user_id: UserId::new("user-beta").expect("target user"),
                limit: Some(50),
                cursor: None,
            },
        )
        .await
        .expect_err("member must not scrape threads");

    assert_forbidden(error);
    assert!(logs_contain(
        "action=\"threads_listed\" outcome=\"failure\""
    ));
}

#[tokio::test]
async fn admin_thread_scraping_does_not_match_a_target_from_another_tenant() {
    let services = admin_services(FakeAdminUsers::with_tenant_records([
        (
            "tenant-alpha".to_string(),
            admin_record("user-alpha", AdminUserRole::Admin, AdminUserStatus::Active),
        ),
        (
            "tenant-beta".to_string(),
            admin_record("user-beta", AdminUserRole::Member, AdminUserStatus::Active),
        ),
    ]));

    let error = services
        .list_admin_thread_scrape_threads(
            caller(),
            RebornAdminThreadScrapeListRequest {
                user_id: UserId::new("user-beta").expect("target user"),
                limit: Some(50),
                cursor: None,
            },
        )
        .await
        .expect_err("cross-tenant target must not be visible");

    assert_eq!(error.status_code, 404);
    assert_eq!(error.code, ProductSurfaceErrorCode::NotFound);
}

#[tokio::test]
#[traced_test]
async fn admin_thread_scrape_views_dispatch_through_query() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_admin_user_service(Arc::new(FakeAdminUsers::with([
        admin_record("user-alpha", AdminUserRole::Admin, AdminUserStatus::Active),
        admin_record("user-beta", AdminUserRole::Member, AdminUserStatus::Active),
    ])));
    setup_owned_thread(&services, caller(), "thread-admin").await;
    let target_caller = caller_for_user("user-beta");
    setup_owned_thread(&services, target_caller.clone(), "thread-target").await;
    let thread_id = ThreadId::new("thread-target").expect("thread id");
    let run_id = TurnRunId::parse(&run_id_string()).expect("run id");
    seed_submitted_message(
        &thread_service,
        &thread_scope_for(&target_caller),
        &thread_id,
        &run_id,
        "target trajectory",
    )
    .await;
    // Two more target threads, created strictly after the seeded one, so the
    // activity-sorted page order is deterministic: [thread-target-new,
    // thread-target-mid, thread-target].
    let mid = thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope_for(&target_caller),
            thread_id: Some(ThreadId::new("thread-target-mid").expect("mid thread id")),
            created_by_actor_id: target_caller.user_id.as_str().to_string(),
            title: Some("Mid target chat".to_string()),
            metadata_json: None,
        })
        .await
        .expect("mid target thread");
    wait_until_after(mid.updated_at.expect("activity stamp")).await;
    thread_service
        .ensure_thread(EnsureThreadRequest {
            scope: thread_scope_for(&target_caller),
            thread_id: Some(ThreadId::new("thread-target-new").expect("new thread id")),
            created_by_actor_id: target_caller.user_id.as_str().to_string(),
            title: Some("New target chat".to_string()),
            metadata_json: None,
        })
        .await
        .expect("new target thread");
    let target = UserId::new("user-beta").expect("target user");

    // The three scrape views must be reachable through the real query
    // dispatch (param deserialization, the page-cursor merge, and
    // view_page_with_cursor propagation) — not only by calling the service
    // methods directly.
    // safety: ProductSurface service query calls in a contract test; no
    // database transaction is involved.
    let threads_page = services
        .query(
            caller(),
            ADMIN_THREAD_SCRAPE_THREADS_VIEW
                .query(
                    RebornAdminThreadScrapeListRequest {
                        user_id: target.clone(),
                        limit: Some(50),
                        cursor: None,
                    },
                    None,
                )
                .expect("threads query"),
        )
        .await
        .expect("threads view");
    let threads: RebornListThreadsResponse =
        serde_json::from_value(threads_page.payload).expect("threads payload");
    assert_eq!(threads.threads.len(), 3);
    assert_eq!(
        threads
            .threads
            .iter()
            .map(|thread| thread.thread_id.as_str())
            .collect::<Vec<_>>(),
        vec!["thread-target-new", "thread-target-mid", "thread-target"],
    );

    // The transport page cursor must override a conflicting params cursor and
    // the page boundary must round-trip as the next cursor. The params cursor
    // alone (the oldest thread) would select an empty page; the transport
    // cursor (the newest thread) must win and return the middle page.
    let cursor_page = services
        .query(
            caller(),
            ADMIN_THREAD_SCRAPE_THREADS_VIEW
                .query(
                    RebornAdminThreadScrapeListRequest {
                        user_id: target.clone(),
                        limit: Some(1),
                        cursor: Some("thread-target".to_string()),
                    },
                    Some("thread-target-new".to_string()),
                )
                .expect("threads query"),
        )
        .await
        .expect("threads view");
    let cursor_threads: RebornListThreadsResponse =
        serde_json::from_value(cursor_page.payload).expect("threads payload");
    assert_eq!(cursor_threads.threads.len(), 1);
    assert_eq!(
        cursor_threads.threads[0].thread_id.as_str(),
        "thread-target-mid",
        "the transport cursor must determine the returned page"
    );
    assert_eq!(
        cursor_page.next_cursor.as_deref(),
        Some("thread-target-mid"),
        "the page boundary must propagate as the next cursor"
    );

    let artifact_page = services
        .query(
            caller(),
            ADMIN_THREAD_SCRAPE_ARTIFACT_VIEW
                .query(
                    RebornAdminThreadScrapeArtifactRequest {
                        user_id: target.clone(),
                        thread_id: "thread-target".to_string(),
                    },
                    None,
                )
                .expect("artifact query"),
        )
        .await
        .expect("artifact view");
    let artifact: RebornThreadArtifact =
        serde_json::from_value(artifact_page.payload).expect("artifact payload");
    assert_eq!(artifact.thread_id, "thread-target");

    let run_page = services
        .query(
            caller(),
            ADMIN_THREAD_SCRAPE_RUN_ARTIFACT_VIEW
                .query(
                    RebornAdminThreadScrapeRunArtifactRequest {
                        user_id: target,
                        thread_id: "thread-target".to_string(),
                        run_id: run_id.to_string(),
                    },
                    None,
                )
                .expect("run query"),
        )
        .await
        .expect("run view");
    let run_artifact: RebornRunArtifact =
        serde_json::from_value(run_page.payload).expect("run payload");
    assert_eq!(run_artifact.run.run_id, run_id);
    assert_eq!(run_artifact.messages.len(), 1);
    assert_eq!(run_artifact.messages[0].content, "target trajectory");
    assert!(logs_contain(
        "action=\"threads_listed\" outcome=\"success\""
    ));
    assert!(logs_contain(
        "action=\"thread_artifact_exported\" outcome=\"success\""
    ));
    assert!(logs_contain(
        "action=\"run_artifact_exported\" outcome=\"success\""
    ));
}

#[tokio::test]
#[traced_test]
async fn admin_thread_scraping_denies_non_admin_artifact_and_run_artifact_exports() {
    let services = admin_services(FakeAdminUsers::with([
        admin_record("user-alpha", AdminUserRole::Member, AdminUserStatus::Active),
        admin_record("user-beta", AdminUserRole::Member, AdminUserStatus::Active),
    ]));

    let thread_error = services
        .build_admin_thread_scrape_artifact(
            caller(),
            RebornAdminThreadScrapeArtifactRequest {
                user_id: UserId::new("user-beta").expect("target user"),
                thread_id: "thread-target".to_string(),
            },
        )
        .await
        .expect_err("member must not export another member's thread artifact");
    assert_forbidden(thread_error);
    assert!(logs_contain(
        "action=\"thread_artifact_exported\" outcome=\"failure\""
    ));

    let run_error = services
        .build_admin_thread_scrape_run_artifact(
            caller(),
            RebornAdminThreadScrapeRunArtifactRequest {
                user_id: UserId::new("user-beta").expect("target user"),
                thread_id: "thread-target".to_string(),
                run_id: "3d54a1f0-0a7f-4b9c-a350-4258f2fa3e18".to_string(),
            },
        )
        .await
        .expect_err("member must not export another member's run artifact");
    assert_forbidden(run_error);
    assert!(logs_contain(
        "action=\"run_artifact_exported\" outcome=\"failure\""
    ));
}

#[tokio::test]
#[traced_test]
async fn admin_thread_scraping_rejects_malformed_ids_before_any_audit_emission() {
    let services = admin_services(FakeAdminUsers::with([admin_record(
        "user-alpha",
        AdminUserRole::Admin,
        AdminUserStatus::Active,
    )]));

    // A newline inside a path segment must be rejected as validation before
    // it can be Display-formatted into the audit trail (forged audit lines).
    let thread_error = services
        .build_admin_thread_scrape_artifact(
            caller(),
            RebornAdminThreadScrapeArtifactRequest {
                user_id: UserId::new("user-alpha").expect("target user"),
                thread_id: "thread-target\nforged-line".to_string(),
            },
        )
        .await
        .expect_err("malformed thread id must be rejected");
    assert_eq!(thread_error.status_code, 400);
    assert_eq!(thread_error.code, ProductSurfaceErrorCode::InvalidRequest);
    assert!(
        !logs_contain("forged-line"),
        "the raw id must never reach the audit trail"
    );

    let run_error = services
        .build_admin_thread_scrape_run_artifact(
            caller(),
            RebornAdminThreadScrapeRunArtifactRequest {
                user_id: UserId::new("user-alpha").expect("target user"),
                thread_id: "thread-target".to_string(),
                run_id: "not-a-uuid\nforged".to_string(),
            },
        )
        .await
        .expect_err("malformed run id must be rejected");
    assert_eq!(run_error.status_code, 400);
    assert_eq!(run_error.code, ProductSurfaceErrorCode::InvalidRequest);
    assert!(
        !logs_contain("forged"),
        "the raw run id must never reach the audit trail"
    );
}

#[tokio::test]
async fn admin_users_are_available_as_product_views_and_capabilities() {
    let services = admin_services(FakeAdminUsers::with([
        admin_record("user-alpha", AdminUserRole::Admin, AdminUserStatus::Active),
        admin_record("user-beta", AdminUserRole::Member, AdminUserStatus::Active),
    ]));
    let target = UserId::new("user-beta").expect("user");

    // safety: these are ProductSurface service query calls in a contract test;
    // no database transaction is involved.
    let users = services
        .query(
            caller(),
            ADMIN_USERS_VIEW
                .query(
                    RebornAdminUserListQuery {
                        limit: Some(2),
                        ..Default::default()
                    },
                    None,
                )
                .expect("admin users query"),
        )
        .await
        .expect("admin users view");
    let users: RebornAdminUserListResponse =
        serde_json::from_value(users.payload).expect("admin users payload");
    assert_eq!(users.users.len(), 2);
    assert_eq!(users.users[1].user_id.as_str(), "user-beta");
    assert_eq!(users.next_cursor.as_deref(), Some("user-beta"));

    let user = services
        .query(
            caller(),
            ADMIN_USER_VIEW
                .query(
                    RebornAdminUserRequest {
                        user_id: target.clone(),
                    },
                    None,
                )
                .expect("admin user query"),
        )
        .await
        .expect("admin user view");
    let user: RebornAdminUserResponse =
        serde_json::from_value(user.payload).expect("admin user payload");
    assert_eq!(user.user.user_id.as_str(), "user-beta");

    let secrets = services
        .query(
            caller(),
            ADMIN_USER_SECRETS_VIEW
                .query(
                    RebornAdminUserRequest {
                        user_id: target.clone(),
                    },
                    None,
                )
                .expect("admin user secrets query"),
        )
        .await
        .expect("admin user secrets view");
    let secrets: RebornAdminUserSecretsListResponse =
        serde_json::from_value(secrets.payload).expect("admin user secrets payload");
    assert!(secrets.secrets.is_empty());

    let update_resolution = services
        .invoke(
            caller(),
            CapabilityId::new(ADMIN_USER_UPDATE_CAPABILITY_ID).expect("capability id"),
            serde_json::to_value(RebornAdminUpdateUserProductRequest {
                user_id: target.clone(),
                display_name: Some("Beta Renamed".to_string()),
                metadata: None,
            })
            .expect("admin update input"),
            ActivityId::new(),
        )
        .await
        .expect("admin update capability");
    assert!(matches!(
        update_resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));

    let updated = services
        .query(
            caller(),
            ADMIN_USER_VIEW
                .query(
                    RebornAdminUserRequest {
                        user_id: target.clone(),
                    },
                    None,
                )
                .expect("admin user query"),
        )
        .await
        .expect("admin user view after update");
    let updated: RebornAdminUserResponse =
        serde_json::from_value(updated.payload).expect("admin user payload");
    assert_eq!(updated.user.display_name.as_deref(), Some("Beta Renamed"));

    let put_secret_resolution = services
        .invoke(
            caller(),
            CapabilityId::new(ADMIN_USER_PUT_SECRET_CAPABILITY_ID).expect("capability id"),
            serde_json::to_value(RebornAdminPutSecretProductRequest {
                user_id: target.clone(),
                handle: "openai_api_key".to_string(),
                value: "sk-test".to_string(),
            })
            .expect("admin secret put input"),
            ActivityId::new(),
        )
        .await
        .expect("admin secret put capability");
    assert!(matches!(
        put_secret_resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));

    let delete_secret_resolution = services
        .invoke(
            caller(),
            CapabilityId::new(ADMIN_USER_DELETE_SECRET_CAPABILITY_ID).expect("capability id"),
            serde_json::to_value(RebornAdminDeleteSecretProductRequest {
                user_id: target.clone(),
                handle: "openai_api_key".to_string(),
            })
            .expect("admin secret delete input"),
            ActivityId::new(),
        )
        .await
        .expect("admin secret delete capability");
    assert!(matches!(
        delete_secret_resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));

    let status_resolution = services
        .invoke(
            caller(),
            CapabilityId::new(ADMIN_USER_SET_STATUS_CAPABILITY_ID).expect("capability id"),
            serde_json::to_value(RebornAdminSetStatusProductRequest {
                user_id: target.clone(),
                status: AdminUserStatus::Suspended,
            })
            .expect("admin status input"),
            ActivityId::new(),
        )
        .await
        .expect("admin status capability");
    assert!(matches!(
        status_resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));

    let suspended = services
        .query(
            caller(),
            ADMIN_USER_VIEW
                .query(
                    RebornAdminUserRequest {
                        user_id: target.clone(),
                    },
                    None,
                )
                .expect("admin user query"),
        )
        .await
        .expect("admin user view after status");
    let suspended: RebornAdminUserResponse =
        serde_json::from_value(suspended.payload).expect("admin user payload");
    assert_eq!(suspended.user.status, AdminUserStatus::Suspended);

    let role_resolution = services
        .invoke(
            caller(),
            CapabilityId::new(ADMIN_USER_SET_ROLE_CAPABILITY_ID).expect("capability id"),
            serde_json::to_value(RebornAdminSetRoleProductRequest {
                user_id: target.clone(),
                role: AdminUserRole::Admin,
            })
            .expect("admin role input"),
            ActivityId::new(),
        )
        .await
        .expect("admin role capability");
    assert!(matches!(
        role_resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));

    let promoted = services
        .query(
            caller(),
            ADMIN_USER_VIEW
                .query(
                    RebornAdminUserRequest {
                        user_id: target.clone(),
                    },
                    None,
                )
                .expect("admin user query"),
        )
        .await
        .expect("admin user view after role");
    let promoted: RebornAdminUserResponse =
        serde_json::from_value(promoted.payload).expect("admin user payload");
    assert_eq!(promoted.user.role, AdminUserRole::Admin);

    let delete_resolution = services
        .invoke(
            caller(),
            CapabilityId::new(ADMIN_USER_DELETE_CAPABILITY_ID).expect("capability id"),
            serde_json::to_value(RebornAdminUserRequest {
                user_id: target.clone(),
            })
            .expect("admin delete input"),
            ActivityId::new(),
        )
        .await
        .expect("admin delete capability");
    assert!(matches!(
        delete_resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));

    let deleted = services
        .query(
            caller(),
            ADMIN_USER_VIEW
                .query(RebornAdminUserRequest { user_id: target }, None)
                .expect("admin user query"),
        )
        .await
        .expect_err("deleted user is no longer readable");
    assert_eq!(deleted.code, ProductSurfaceErrorCode::NotFound);
}

/// Drive EVERY admin verb through the service and assert each is a 403.
/// `authorize_admin` is a predicate that gates side effects, so it must be
/// tested at every call site — not just `list` (.claude/rules/testing.md,
/// "test through the caller"): a verb that forgot to call it would be an
/// unauthorized read/mutation hole invisible to a single-endpoint test.
async fn assert_every_admin_verb_forbidden(services: &RebornServices) {
    let target = UserId::new("some-target").expect("user");
    assert_forbidden(
        services
            .list_admin_users(caller(), RebornAdminUserListQuery::default())
            .await
            .expect_err("list"),
    );
    assert_forbidden(
        services
            .get_admin_user(caller(), target.clone())
            .await
            .expect_err("get"),
    );
    assert_forbidden(
        services
            .create_admin_user(
                caller(),
                RebornAdminCreateUserRequest {
                    email: None,
                    display_name: None,
                    role: AdminUserRole::Member,
                },
            )
            .await
            .expect_err("create"),
    );
    assert_forbidden(
        services
            .update_admin_user(
                caller(),
                target.clone(),
                RebornAdminUpdateUserRequest::default(),
            )
            .await
            .expect_err("update"),
    );
    assert_forbidden(
        services
            .set_admin_user_status(
                caller(),
                target.clone(),
                RebornAdminSetStatusRequest {
                    status: AdminUserStatus::Suspended,
                },
            )
            .await
            .expect_err("status"),
    );
    assert_forbidden(
        services
            .set_admin_user_role(
                caller(),
                target.clone(),
                RebornAdminSetRoleRequest {
                    role: AdminUserRole::Admin,
                },
            )
            .await
            .expect_err("role"),
    );
    assert_forbidden(
        services
            .delete_admin_user(caller(), target.clone())
            .await
            .expect_err("delete"),
    );
    assert_forbidden(
        services
            .list_admin_user_secrets(caller(), target.clone())
            .await
            .expect_err("list_secrets"),
    );
    assert_forbidden(
        services
            .put_admin_user_secret(
                caller(),
                target.clone(),
                SecretHandle::new("handle").unwrap(),
                RebornAdminPutSecretRequest {
                    value: "v".to_string(),
                },
            )
            .await
            .expect_err("put_secret"),
    );
    assert_forbidden(
        services
            .delete_admin_user_secret(caller(), target, SecretHandle::new("handle").unwrap())
            .await
            .expect_err("delete_secret"),
    );
}

#[tokio::test]
async fn admin_member_caller_is_forbidden_on_every_verb() {
    // caller() resolves to user-alpha; seeded as a plain member → 403 on EVERY
    // admin verb, not just list.
    let services = admin_services(FakeAdminUsers::with([admin_record(
        "user-alpha",
        AdminUserRole::Member,
        AdminUserStatus::Active,
    )]));
    assert_every_admin_verb_forbidden(&services).await;
    // Self-privilege-escalation: a member cannot promote their own record.
    assert_forbidden(
        services
            .set_admin_user_role(
                caller(),
                UserId::new("user-alpha").expect("user"),
                RebornAdminSetRoleRequest {
                    role: AdminUserRole::Admin,
                },
            )
            .await
            .expect_err("a member must not promote themselves"),
    );
}

#[tokio::test]
async fn admin_unknown_caller_is_forbidden_on_every_verb() {
    // The caller has no user record at all. Same 403 as a member — the service
    // must never leak (via a different status/code) whether the caller record
    // exists but is under-privileged vs. does not exist.
    let services = admin_services(FakeAdminUsers::default());
    assert_every_admin_verb_forbidden(&services).await;
}

#[tokio::test]
async fn admin_suspended_admin_is_forbidden_on_every_verb() {
    // Regression: `authorize_admin` used to check `role` only, so a SUSPENDED
    // admin kept full control (the role field still reads Admin). Status now
    // gates authorization, so suspending an admin revokes their admin API
    // access immediately — on every verb.
    let services = admin_services(FakeAdminUsers::with([admin_record(
        "user-alpha",
        AdminUserRole::Admin,
        AdminUserStatus::Suspended,
    )]));
    assert_every_admin_verb_forbidden(&services).await;

    // A suspended OWNER is likewise locked out (owner also clears the role
    // boundary, so status must gate it too).
    let services_owner = admin_services(FakeAdminUsers::with([admin_record(
        "user-alpha",
        AdminUserRole::Owner,
        AdminUserStatus::Suspended,
    )]));
    assert_forbidden(
        services_owner
            .list_admin_users(caller(), RebornAdminUserListQuery::default())
            .await
            .expect_err("a suspended owner must not reach the admin surface"),
    );
}

#[tokio::test]
async fn admin_caller_lists_and_creates_with_one_time_token() {
    let services = admin_services(FakeAdminUsers::with([admin_record(
        "user-alpha",
        AdminUserRole::Admin,
        AdminUserStatus::Active,
    )]));
    services
        .list_admin_users(caller(), RebornAdminUserListQuery::default())
        .await
        .expect("an admin may list users");
    let created = services
        .create_admin_user(
            caller(),
            RebornAdminCreateUserRequest {
                email: Some("new@acme.com".to_string()),
                display_name: Some("New".to_string()),
                role: AdminUserRole::Member,
            },
        )
        .await
        .expect("an admin may create a user");
    assert_eq!(created.api_token, "minted-token");
}

#[tokio::test]
async fn admin_list_forwards_status_filter_to_the_port() {
    // A dropped `query.status` extractor or a broken active/suspended mapping
    // would silently return every user. Seed one active + one suspended admin
    // and assert `?status=` narrows the caller-visible result.
    let services = admin_services(FakeAdminUsers::with([
        admin_record("user-active", AdminUserRole::Admin, AdminUserStatus::Active),
        admin_record(
            "user-suspended",
            AdminUserRole::Admin,
            AdminUserStatus::Suspended,
        ),
    ]));

    let suspended = services
        .list_admin_users(
            caller().with_operator_config(true),
            RebornAdminUserListQuery {
                status: Some(AdminUserStatus::Suspended),
                ..Default::default()
            },
        )
        .await
        .expect("list suspended");
    assert_eq!(suspended.users.len(), 1, "only the suspended user matches");
    assert_eq!(suspended.users[0].user_id.as_str(), "user-suspended");

    let active = services
        .list_admin_users(
            caller().with_operator_config(true),
            RebornAdminUserListQuery {
                status: Some(AdminUserStatus::Active),
                ..Default::default()
            },
        )
        .await
        .expect("list active");
    assert_eq!(active.users.len(), 1, "only the active user matches");
    assert_eq!(active.users[0].user_id.as_str(), "user-active");

    let all = services
        .list_admin_users(
            caller().with_operator_config(true),
            RebornAdminUserListQuery::default(),
        )
        .await
        .expect("list all");
    assert_eq!(all.users.len(), 2, "no filter returns both");
}

#[tokio::test]
async fn admin_list_bounds_pages_and_threads_the_cursor() {
    // The service must clamp the page and derive a `next_cursor` from a full
    // page, then honor that cursor on the next call — so a large tenant is
    // paged, not returned (and scanned) in one unbounded response.
    let services = admin_services(FakeAdminUsers::with([
        admin_record("user-a", AdminUserRole::Admin, AdminUserStatus::Active),
        admin_record("user-b", AdminUserRole::Member, AdminUserStatus::Active),
        admin_record("user-c", AdminUserRole::Member, AdminUserStatus::Active),
    ]));

    let page1 = services
        .list_admin_users(
            caller().with_operator_config(true),
            RebornAdminUserListQuery {
                limit: Some(2),
                ..Default::default()
            },
        )
        .await
        .expect("page 1");
    assert_eq!(page1.users.len(), 2, "the page honors the limit");
    assert_eq!(page1.users[0].user_id.as_str(), "user-a");
    assert_eq!(page1.users[1].user_id.as_str(), "user-b");
    let cursor = page1.next_cursor.expect("a full page yields a next cursor");
    assert_eq!(cursor, "user-b", "the cursor is the last id on the page");

    let page2 = services
        .list_admin_users(
            caller().with_operator_config(true),
            RebornAdminUserListQuery {
                limit: Some(2),
                cursor: Some(cursor),
                ..Default::default()
            },
        )
        .await
        .expect("page 2");
    assert_eq!(page2.users.len(), 1, "the final page holds the remainder");
    assert_eq!(page2.users[0].user_id.as_str(), "user-c");
    assert!(
        page2.next_cursor.is_none(),
        "a short page means no more users"
    );
}

#[tokio::test]
async fn admin_list_rejects_a_malformed_cursor() {
    let services = admin_services(FakeAdminUsers::default());
    let err = services
        .list_admin_users(
            caller().with_operator_config(true),
            RebornAdminUserListQuery {
                cursor: Some("not a valid user id \u{7f}".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect_err("a malformed cursor is caller input at fault");
    assert_eq!(err.status_code, 400);
}

#[tokio::test]
async fn admin_operator_bypasses_role_check() {
    // An env-bearer operator has no user record but is an implicit admin.
    let services = admin_services(FakeAdminUsers::default());
    let operator = caller().with_operator_config(true);
    services
        .list_admin_users(operator, RebornAdminUserListQuery::default())
        .await
        .expect("an operator token clears the admin boundary without a user record");
}

#[tokio::test]
async fn admin_last_admin_protection_blocks_demote_suspend_and_delete() {
    // caller() (user-alpha) is the SOLE active admin. Demote, suspend, AND
    // delete must all be blocked — any of the three would otherwise strand the
    // tenant with zero active admins. `delete` has its own guard distinct from
    // the demote/suspend path, so it is covered here explicitly.
    let services = admin_services(FakeAdminUsers::with([admin_record(
        "user-alpha",
        AdminUserRole::Admin,
        AdminUserStatus::Active,
    )]));
    let target = UserId::new("user-alpha").expect("user");

    let demote = services
        .set_admin_user_role(
            caller(),
            target.clone(),
            RebornAdminSetRoleRequest {
                role: AdminUserRole::Member,
            },
        )
        .await
        .expect_err("demoting the sole admin must be blocked");
    assert_eq!(demote.status_code, 409);
    assert_eq!(demote.field.as_deref(), Some("last_admin"));

    let suspend = services
        .set_admin_user_status(
            caller(),
            target.clone(),
            RebornAdminSetStatusRequest {
                status: AdminUserStatus::Suspended,
            },
        )
        .await
        .expect_err("suspending the sole admin must be blocked");
    assert_eq!(suspend.status_code, 409);
    assert_eq!(suspend.field.as_deref(), Some("last_admin"));

    let delete = services
        .delete_admin_user(caller(), target)
        .await
        .expect_err("deleting the sole admin must be blocked");
    assert_eq!(delete.status_code, 409);
    assert_eq!(delete.field.as_deref(), Some("last_admin"));
}

#[tokio::test]
async fn admin_last_admin_protection_allows_demote_with_a_second_admin() {
    // With two active admins, demoting one is allowed.
    let services = admin_services(FakeAdminUsers::with([
        admin_record("user-alpha", AdminUserRole::Admin, AdminUserStatus::Active),
        admin_record("user-beta", AdminUserRole::Admin, AdminUserStatus::Active),
    ]));
    services
        .set_admin_user_role(
            caller(),
            UserId::new("user-beta").expect("user"),
            RebornAdminSetRoleRequest {
                role: AdminUserRole::Member,
            },
        )
        .await
        .expect("demoting one of two admins is allowed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_last_admin_protection_survives_concurrent_demotion() {
    // Two active admins. Fire both demotions concurrently: without serialization
    // each `ensure_not_last_admin` reads "2 admins", both pass, and both land →
    // 0 admins (a TOCTOU race). The per-tenant admin-mutation lock serializes
    // the check+mutation, so exactly one demotion succeeds and the other is a
    // 409 — the tenant always keeps an admin. Runs on a multi-thread runtime so
    // the two calls are genuinely parallel.
    let services = std::sync::Arc::new(admin_services(FakeAdminUsers::with([
        admin_record("user-alpha", AdminUserRole::Admin, AdminUserStatus::Active),
        admin_record("user-beta", AdminUserRole::Admin, AdminUserStatus::Active),
    ])));

    // Both demotions run as an OPERATOR caller (bypasses the role check and is
    // never itself demoted), so the second failure is deterministically the
    // 409 last-admin block — not a 403 from the caller losing its own role.
    let demote = |uid: &'static str| {
        let services = std::sync::Arc::clone(&services);
        async move {
            services
                .set_admin_user_role(
                    caller().with_operator_config(true),
                    UserId::new(uid).expect("user"),
                    RebornAdminSetRoleRequest {
                        role: AdminUserRole::Member,
                    },
                )
                .await
        }
    };

    let (alpha, beta) = tokio::join!(demote("user-alpha"), demote("user-beta"));

    let successes = [alpha.is_ok(), beta.is_ok()]
        .into_iter()
        .filter(|ok| *ok)
        .count();
    let blocked = [&alpha, &beta]
        .into_iter()
        .filter(|result| matches!(result, Err(err) if err.status_code == 409))
        .count();
    assert_eq!(successes, 1, "exactly one concurrent demotion may land");
    assert_eq!(
        blocked, 1,
        "the other must be blocked by last-admin protection (never stranded at 0 admins)"
    );

    let remaining = services
        .list_admin_users(
            caller().with_operator_config(true),
            RebornAdminUserListQuery::default(),
        )
        .await
        .expect("list")
        .users
        .into_iter()
        .filter(|u| u.role.is_admin() && u.status == AdminUserStatus::Active)
        .count();
    assert_eq!(
        remaining, 1,
        "the tenant must never be stranded without an admin"
    );
}
