//! Product-surface descriptors, view/capability dispatch, project and filesystem reads, and the error taxonomy.

use super::*;

#[tokio::test]
async fn default_invoke_uses_canonical_host_types_and_fails_closed() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let result: Result<Resolution, ProductSurfaceError> = services
        .invoke(
            caller(),
            CapabilityId::new("product.test_invoke").expect("valid capability id"),
            json!({"request": "test"}),
            ActivityId::new(),
        )
        .await;

    let error = result.expect_err("unwired invoke must fail closed");
    assert_eq!(error.code, ProductSurfaceErrorCode::Unavailable);
    assert_eq!(error.kind, ProductSurfaceErrorKind::ServiceUnavailable);
    assert_eq!(error.status_code, 503);
    assert!(!error.retryable);
}

#[tokio::test]
async fn trace_hold_authorize_capability_decodes_typed_product_input() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    let operation_id = TRACE_HOLD_AUTHORIZE_COMMAND
        .capability_id()
        .expect("trace hold capability id");

    let response = ProductSurface::invoke(
        &services,
        caller(),
        ironclaw_product_contracts::surface::ProductSurfaceInvokeRequest {
            operation_id: operation_id.clone(),
            input: serde_json::to_value(RebornTraceHoldAuthorizeProductRequest {
                submission_id: uuid::Uuid::new_v4().to_string(),
            })
            .expect("trace hold input"),
            activity_id: ActivityId::new(),
        },
    )
    .await
    .expect("unknown valid submission id is a no-op");
    let decoded: RebornTraceHoldAuthorizeResponse =
        serde_json::from_value(response.output).expect("trace hold response");
    assert!(!decoded.authorized);

    let error = ProductSurface::invoke(
        &services,
        caller(),
        ironclaw_product_contracts::surface::ProductSurfaceInvokeRequest {
            operation_id,
            input: serde_json::to_value(RebornTraceHoldAuthorizeProductRequest {
                submission_id: "not-a-submission-id".to_string(),
            })
            .expect("trace hold input"),
            activity_id: ActivityId::new(),
        },
    )
    .await
    .expect_err("invalid submission id must fail validation");

    assert_eq!(error.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(error.kind, ProductSurfaceErrorKind::Validation);
    assert_eq!(error.field.as_deref(), Some("submission_id"));
    assert_eq!(
        error.validation_code,
        Some(ProductSurfaceValidationCode::InvalidId)
    );
}

#[tokio::test]
async fn duplicate_create_thread_replays_generated_thread_for_same_client_action() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    let request = || {
        serde_json::from_value::<ProductCreateThreadRequest>(json!({
            "client_action_id": "create-duplicate"
        }))
        .expect("request")
    };

    let first = services
        .create_thread(caller(), request())
        .await
        .expect("first create succeeds");
    let replayed = services
        .create_thread(caller(), request())
        .await
        .expect("duplicate create replays");

    assert_eq!(first.thread.thread_id, replayed.thread.thread_id);
    assert_eq!(first.thread.metadata_json, replayed.thread.metadata_json);
}

#[tokio::test]
async fn create_thread_metadata_is_serialized_json() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    let client_action_id = "create-quote-\"-slash-\\-line-\u{2028}".to_string();

    let response = services
        .create_thread(
            caller(),
            serde_json::from_value::<ProductCreateThreadRequest>(json!({
                "client_action_id": client_action_id
            }))
            .expect("request"),
        )
        .await
        .expect("create succeeds");

    let metadata = response.thread.metadata_json.expect("metadata");
    let metadata: serde_json::Value = serde_json::from_str(&metadata).expect("metadata json");
    assert_eq!(
        metadata["client_action_id"].as_str(),
        Some(client_action_id.as_str())
    );
}

/// Project service that authorizes exactly one project id through `get_project`
/// and fails everything else, so create-thread project authorization can be
/// driven from the caller without a real repository.
#[derive(Debug)]
struct AuthorizingProjectService {
    allowed_project_id: String,
}

#[async_trait]
impl ProjectService for AuthorizingProjectService {
    async fn list_projects(
        &self,
        _caller: ProjectCaller,
        _request: RebornListProjectsRequest,
    ) -> Result<RebornListProjectsResponse, ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn create_project(
        &self,
        _caller: ProjectCaller,
        _request: RebornCreateProjectRequest,
    ) -> Result<RebornProjectResponse, ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn get_project(
        &self,
        _caller: ProjectCaller,
        request: RebornGetProjectRequest,
    ) -> Result<RebornProjectResponse, ProjectServiceError> {
        if request.project_id == self.allowed_project_id {
            Ok(RebornProjectResponse {
                project: RebornProjectInfo {
                    project_id: self.allowed_project_id.clone(),
                    name: "Authorized".to_string(),
                    description: String::new(),
                    icon: None,
                    color: None,
                    metadata: serde_json::json!({}),
                    state: RebornProjectState::Active,
                    role: RebornProjectRole::Owner,
                    created_at: "1970-01-01T00:00:00Z".parse().expect("created at"),
                    updated_at: "1970-01-01T00:00:00Z".parse().expect("updated at"),
                },
            })
        } else {
            // Mirrors the real service: no access (or unknown) collapses to NotFound.
            Err(ProjectServiceError::NotFound)
        }
    }

    async fn update_project(
        &self,
        _caller: ProjectCaller,
        _request: RebornUpdateProjectRequest,
    ) -> Result<RebornProjectResponse, ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn delete_project(
        &self,
        _caller: ProjectCaller,
        _request: RebornDeleteProjectRequest,
    ) -> Result<(), ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn list_members(
        &self,
        _caller: ProjectCaller,
        _request: RebornListMembersRequest,
    ) -> Result<RebornListMembersResponse, ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn add_member(
        &self,
        _caller: ProjectCaller,
        _request: RebornAddMemberRequest,
    ) -> Result<RebornProjectMemberInfo, ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn update_member_role(
        &self,
        _caller: ProjectCaller,
        _request: RebornUpdateMemberRoleRequest,
    ) -> Result<RebornProjectMemberInfo, ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }

    async fn remove_member(
        &self,
        _caller: ProjectCaller,
        _request: RebornRemoveMemberRequest,
    ) -> Result<(), ProjectServiceError> {
        Err(ProjectServiceError::Internal)
    }
}

struct EmptyFilesystemBrowser;

#[async_trait]
impl FilesystemBrowseReader for EmptyFilesystemBrowser {
    fn available_mounts(&self) -> Vec<FsMount> {
        vec![FsMount::Memory]
    }

    async fn list_dir(
        &self,
        _scope: &ResourceScope,
        _mount: FsMount,
        _path: &str,
    ) -> Result<Vec<ProjectFsEntry>, ProjectFsError> {
        Ok(Vec::new())
    }

    async fn read_file(
        &self,
        _scope: &ResourceScope,
        _mount: FsMount,
        _path: &str,
    ) -> Result<ProjectFsFile, ProjectFsError> {
        Err(ProjectFsError::NotFound)
    }

    async fn stat(
        &self,
        _scope: &ResourceScope,
        _mount: FsMount,
        _path: &str,
    ) -> Result<ProjectFsStat, ProjectFsError> {
        Err(ProjectFsError::NotFound)
    }
}

struct StaticProjectFilesystemReader;

#[async_trait]
impl ProjectFilesystemReader for StaticProjectFilesystemReader {
    async fn list_dir(
        &self,
        _thread_scope: &ThreadScope,
        path: &str,
    ) -> Result<Vec<ProjectFsEntry>, ProjectFsError> {
        Ok(vec![ProjectFsEntry {
            name: "report.md".to_string(),
            path: format!("{}/report.md", path.trim_end_matches('/')),
            kind: ProjectFsEntryKind::File,
        }])
    }

    async fn read_file(
        &self,
        _thread_scope: &ThreadScope,
        _path: &str,
    ) -> Result<ironclaw_host_api::attachment::WorkspaceFile, ProjectFsError> {
        Err(ProjectFsError::NotFound)
    }

    async fn stat(
        &self,
        _thread_scope: &ThreadScope,
        path: &str,
    ) -> Result<ProjectFsStat, ProjectFsError> {
        Ok(ProjectFsStat {
            path: path.to_string(),
            kind: ProjectFsEntryKind::File,
            size_bytes: 7,
            mime_type: "text/markdown".to_string(),
        })
    }
}

struct StaticFilesystemBrowser;

#[async_trait]
impl FilesystemBrowseReader for StaticFilesystemBrowser {
    fn available_mounts(&self) -> Vec<FsMount> {
        vec![FsMount::Memory, FsMount::Workspace]
    }

    async fn list_dir(
        &self,
        _scope: &ResourceScope,
        _mount: FsMount,
        path: &str,
    ) -> Result<Vec<ProjectFsEntry>, ProjectFsError> {
        Ok(vec![ProjectFsEntry {
            name: "today.md".to_string(),
            path: format!("{}/today.md", path.trim_end_matches('/')),
            kind: ProjectFsEntryKind::File,
        }])
    }

    async fn read_file(
        &self,
        _scope: &ResourceScope,
        _mount: FsMount,
        _path: &str,
    ) -> Result<ProjectFsFile, ProjectFsError> {
        Err(ProjectFsError::NotFound)
    }

    async fn stat(
        &self,
        _scope: &ResourceScope,
        _mount: FsMount,
        path: &str,
    ) -> Result<ProjectFsStat, ProjectFsError> {
        Ok(ProjectFsStat {
            path: path.to_string(),
            kind: ProjectFsEntryKind::File,
            size_bytes: 7,
            mime_type: "text/markdown".to_string(),
        })
    }
}

#[derive(Default)]
struct RecordingProjectService {
    listed: Mutex<usize>,
    read: Mutex<Vec<String>>,
    updated: Mutex<Vec<RebornUpdateProjectRequest>>,
    deleted: Mutex<Vec<String>>,
    listed_members: Mutex<Vec<String>>,
    added_members: Mutex<Vec<RebornAddMemberRequest>>,
    updated_members: Mutex<Vec<RebornUpdateMemberRoleRequest>>,
    removed_members: Mutex<Vec<RebornRemoveMemberRequest>>,
}

#[async_trait]
impl ProjectService for RecordingProjectService {
    async fn list_projects(
        &self,
        _caller: ProjectCaller,
        _request: RebornListProjectsRequest,
    ) -> Result<RebornListProjectsResponse, ProjectServiceError> {
        *self.listed.lock().expect("lock") += 1;
        Ok(RebornListProjectsResponse {
            projects: vec![sample_reborn_project("project-alpha")],
            total_projects: 1,
            active_projects: 1,
            archived_projects: 0,
        })
    }

    async fn create_project(
        &self,
        _caller: ProjectCaller,
        request: RebornCreateProjectRequest,
    ) -> Result<RebornProjectResponse, ProjectServiceError> {
        Ok(RebornProjectResponse {
            project: sample_reborn_project(&request.name),
        })
    }

    async fn get_project(
        &self,
        _caller: ProjectCaller,
        request: RebornGetProjectRequest,
    ) -> Result<RebornProjectResponse, ProjectServiceError> {
        self.read
            .lock()
            .expect("lock")
            .push(request.project_id.clone());
        Ok(RebornProjectResponse {
            project: sample_reborn_project(&request.project_id),
        })
    }

    async fn update_project(
        &self,
        _caller: ProjectCaller,
        request: RebornUpdateProjectRequest,
    ) -> Result<RebornProjectResponse, ProjectServiceError> {
        self.updated.lock().expect("lock").push(request.clone());
        Ok(RebornProjectResponse {
            project: sample_reborn_project(&request.project_id),
        })
    }

    async fn delete_project(
        &self,
        _caller: ProjectCaller,
        request: RebornDeleteProjectRequest,
    ) -> Result<(), ProjectServiceError> {
        self.deleted.lock().expect("lock").push(request.project_id);
        Ok(())
    }

    async fn list_members(
        &self,
        _caller: ProjectCaller,
        request: RebornListMembersRequest,
    ) -> Result<RebornListMembersResponse, ProjectServiceError> {
        self.listed_members
            .lock()
            .expect("lock")
            .push(request.project_id);
        Ok(RebornListMembersResponse {
            members: vec![sample_reborn_project_member("user-beta")],
        })
    }

    async fn add_member(
        &self,
        _caller: ProjectCaller,
        request: RebornAddMemberRequest,
    ) -> Result<RebornProjectMemberInfo, ProjectServiceError> {
        self.added_members
            .lock()
            .expect("lock")
            .push(request.clone());
        Ok(sample_reborn_project_member(&request.user_id))
    }

    async fn update_member_role(
        &self,
        _caller: ProjectCaller,
        request: RebornUpdateMemberRoleRequest,
    ) -> Result<RebornProjectMemberInfo, ProjectServiceError> {
        self.updated_members
            .lock()
            .expect("lock")
            .push(request.clone());
        Ok(sample_reborn_project_member(&request.user_id))
    }

    async fn remove_member(
        &self,
        _caller: ProjectCaller,
        request: RebornRemoveMemberRequest,
    ) -> Result<(), ProjectServiceError> {
        self.removed_members.lock().expect("lock").push(request);
        Ok(())
    }
}

#[tokio::test]
async fn malformed_view_params_are_a_validation_error_not_a_server_fault() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()));

    for view_id in [
        THREADS_VIEW.id,
        AUTOMATIONS_VIEW.id,
        TIMELINE_VIEW.id,
        LOGS_VIEW.id,
        PROJECT_FS_LIST_VIEW.id,
        THREAD_ARTIFACT_VIEW.id,
        NOTIFICATIONS_VIEW.id,
        ironclaw_product_contracts::history::HISTORY_SEARCH_VIEW.id,
    ] {
        // A bare string is wrong for every view, and serde's message quotes it.
        for params in [serde_json::json!("super-secret-token")] {
            let error = services
                .query(
                    caller(),
                    RebornViewQuery {
                        view_id: view_id.to_string(),
                        params,
                        cursor: None,
                    },
                )
                .await
                .expect_err("malformed params are rejected");
            assert_eq!(
                error.kind,
                ProductSurfaceErrorKind::Validation,
                "{view_id}: {error:?}"
            );
            assert_eq!(error.status_code, 400, "{view_id}");
            assert_eq!(error.field.as_deref(), Some("input"), "{view_id}");
            assert_eq!(
                error.validation_code,
                Some(ProductSurfaceValidationCode::InvalidValue),
                "{view_id}"
            );
            assert!(
                !format!("{error:?}").contains("super-secret-token"),
                "{view_id}: the rejection does not echo the payload"
            );
        }
    }
}

#[tokio::test]
async fn project_and_filesystem_reads_are_available_as_product_views() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let project_service = Arc::new(RecordingProjectService::default());
    let services = session_services(thread_service, Arc::new(FakeTurnCoordinator::default()))
        .with_project_filesystem_reader(Arc::new(StaticProjectFilesystemReader))
        .with_filesystem_browser(Arc::new(StaticFilesystemBrowser))
        .with_project_service(project_service.clone());
    create_thread_for(&services, caller(), "thread-product-surface").await;

    // safety: these are ProductSurface service query calls in a contract test;
    // no database transaction is involved.
    let project_fs_list = services
        .query(
            caller(),
            PROJECT_FS_LIST_VIEW
                .query(
                    RebornProjectFsListRequest {
                        thread_id: "thread-product-surface".to_string(),
                        path: "/workspace".to_string(),
                    },
                    None,
                )
                .expect("project fs list query"),
        )
        .await
        .expect("project fs list view");
    let project_fs_list: RebornProjectFsListResponse =
        serde_json::from_value(project_fs_list.payload).expect("project fs list payload");
    assert_eq!(project_fs_list.entries[0].path, "/workspace/report.md");

    let project_fs_stat = services
        .query(
            caller(),
            PROJECT_FS_STAT_VIEW
                .query(
                    RebornProjectFsStatRequest {
                        thread_id: "thread-product-surface".to_string(),
                        path: "/workspace/report.md".to_string(),
                    },
                    None,
                )
                .expect("project fs stat query"),
        )
        .await
        .expect("project fs stat view");
    let project_fs_stat: RebornProjectFsStatResponse =
        serde_json::from_value(project_fs_stat.payload).expect("project fs stat payload");
    assert_eq!(project_fs_stat.stat.mime_type, "text/markdown");

    let mounts = services
        .query(
            caller(),
            FS_MOUNTS_VIEW
                .query(RebornFsMountsRequest {}, None)
                .expect("fs mounts query"),
        )
        .await
        .expect("fs mounts view");
    let mounts: RebornFsMountsResponse =
        serde_json::from_value(mounts.payload).expect("fs mounts payload");
    assert_eq!(mounts.mounts.len(), 2);

    let fs_list = services
        .query(
            caller(),
            FS_LIST_VIEW
                .query(
                    RebornFsListRequest {
                        mount: FsMount::Memory,
                        path: "daily".to_string(),
                        project_id: None,
                    },
                    None,
                )
                .expect("fs list query"),
        )
        .await
        .expect("fs list view");
    let fs_list: RebornFsListResponse =
        serde_json::from_value(fs_list.payload).expect("fs list payload");
    assert_eq!(fs_list.entries[0].path, "daily/today.md");

    let fs_stat = services
        .query(
            caller(),
            FS_STAT_VIEW
                .query(
                    RebornFsStatRequest {
                        mount: FsMount::Memory,
                        path: "daily/today.md".to_string(),
                        project_id: None,
                    },
                    None,
                )
                .expect("fs stat query"),
        )
        .await
        .expect("fs stat view");
    let fs_stat: RebornFsStatResponse =
        serde_json::from_value(fs_stat.payload).expect("fs stat payload");
    assert_eq!(fs_stat.stat.path, "daily/today.md");

    let projects = services
        .query(
            caller(),
            PROJECTS_VIEW
                .query(RebornListProjectsRequest { limit: Some(10) }, None)
                .expect("projects query"),
        )
        .await
        .expect("projects view");
    let projects: RebornListProjectsResponse =
        serde_json::from_value(projects.payload).expect("projects payload");
    assert_eq!(projects.projects[0].project_id, "project-alpha");
    assert_eq!(projects.total_projects, 1);
    assert_eq!(projects.active_projects, 1);
    assert_eq!(projects.archived_projects, 0);

    let project = services
        .query(
            caller(),
            PROJECT_VIEW
                .query(
                    RebornGetProjectRequest {
                        project_id: "project-alpha".to_string(),
                    },
                    None,
                )
                .expect("project query"),
        )
        .await
        .expect("project view");
    let project: RebornProjectResponse =
        serde_json::from_value(project.payload).expect("project payload");
    assert_eq!(project.project.project_id, "project-alpha");

    let members = services
        .query(
            caller(),
            PROJECT_MEMBERS_VIEW
                .query(
                    RebornListMembersRequest {
                        project_id: "project-alpha".to_string(),
                    },
                    None,
                )
                .expect("project members query"),
        )
        .await
        .expect("project members view");
    let members: RebornListMembersResponse =
        serde_json::from_value(members.payload).expect("project members payload");
    assert_eq!(members.members[0].user_id, "user-beta");
}

#[tokio::test]
async fn project_mutations_are_available_as_product_capabilities() {
    let project_service = Arc::new(RecordingProjectService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_project_service(project_service.clone());

    let update_resolution = services
        .invoke(
            caller(),
            CapabilityId::new(PROJECT_UPDATE_CAPABILITY_ID).expect("capability id"),
            json!({ "project_id": "project-alpha", "name": "Renamed" }),
            ActivityId::new(),
        )
        .await
        .expect("project update capability");
    assert!(matches!(
        update_resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));
    assert_eq!(
        project_service.updated.lock().expect("lock")[0].project_id,
        "project-alpha"
    );

    let delete_resolution = services
        .invoke(
            caller(),
            CapabilityId::new(PROJECT_DELETE_CAPABILITY_ID).expect("capability id"),
            json!({ "project_id": "project-alpha" }),
            ActivityId::new(),
        )
        .await
        .expect("project delete capability");
    assert!(matches!(
        delete_resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));
    assert_eq!(
        project_service.deleted.lock().expect("lock").as_slice(),
        ["project-alpha"]
    );

    let add_member_resolution = services
        .invoke(
            caller(),
            CapabilityId::new(PROJECT_MEMBER_ADD_CAPABILITY_ID).expect("capability id"),
            serde_json::to_value(RebornAddMemberRequest {
                project_id: "project-alpha".to_string(),
                user_id: "user-beta".to_string(),
                role: RebornProjectRole::Viewer,
            })
            .expect("project member add input"),
            ActivityId::new(),
        )
        .await
        .expect("project member add capability");
    assert!(matches!(
        add_member_resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));
    assert_eq!(
        project_service.added_members.lock().expect("lock")[0].user_id,
        "user-beta"
    );

    let update_member_resolution = services
        .invoke(
            caller(),
            CapabilityId::new(PROJECT_MEMBER_UPDATE_CAPABILITY_ID).expect("capability id"),
            serde_json::to_value(RebornUpdateMemberRoleRequest {
                project_id: "project-alpha".to_string(),
                user_id: "user-beta".to_string(),
                role: RebornProjectRole::Editor,
            })
            .expect("project member update input"),
            ActivityId::new(),
        )
        .await
        .expect("project member update capability");
    assert!(matches!(
        update_member_resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));
    assert_eq!(
        project_service.updated_members.lock().expect("lock")[0].role,
        RebornProjectRole::Editor
    );

    let remove_member_resolution = services
        .invoke(
            caller(),
            CapabilityId::new(PROJECT_MEMBER_REMOVE_CAPABILITY_ID).expect("capability id"),
            serde_json::to_value(RebornRemoveMemberRequest {
                project_id: "project-alpha".to_string(),
                user_id: "user-beta".to_string(),
            })
            .expect("project member remove input"),
            ActivityId::new(),
        )
        .await
        .expect("project member remove capability");
    assert!(matches!(
        remove_member_resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));
    assert_eq!(
        project_service.removed_members.lock().expect("lock")[0].user_id,
        "user-beta"
    );
}

#[tokio::test]
async fn session_timeline_and_thread_delete_are_available_as_product_surface() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );
    create_thread_for(&services, caller(), "thread-session-product-surface").await;

    // safety: these are ProductSurface service query calls in a contract test;
    // no database transaction is involved.
    let global_auto_approve = services
        .query(
            caller(),
            GLOBAL_AUTO_APPROVE_VIEW
                .query(RebornGlobalAutoApproveRequest {}, None)
                .expect("global auto approve query"),
        )
        .await
        .expect("global auto approve view");
    let global_auto_approve: RebornGlobalAutoApproveResponse =
        serde_json::from_value(global_auto_approve.payload).expect("global auto approve payload");
    assert!(
        !global_auto_approve.enabled,
        "the default service reports global auto-approve disabled"
    );

    let timeline = services
        .query(
            caller(),
            TIMELINE_VIEW
                .query(
                    RebornTimelineRequest::new("thread-session-product-surface").set_limit(10),
                    None,
                )
                .expect("timeline query"),
        )
        .await
        .expect("timeline view");
    let timeline: RebornTimelineResponse =
        serde_json::from_value(timeline.payload).expect("timeline payload");
    assert_eq!(
        timeline.thread.thread_id.as_str(),
        "thread-session-product-surface"
    );
    assert!(timeline.messages.is_empty());

    let delete_resolution = services
        .invoke(
            caller(),
            CapabilityId::new(THREAD_DELETE_CAPABILITY_ID).expect("capability id"),
            serde_json::to_value(RebornDeleteThreadRequest {
                thread_id: "thread-session-product-surface".to_string(),
            })
            .expect("thread delete input"),
            ActivityId::new(),
        )
        .await
        .expect("thread delete capability");
    assert!(matches!(
        delete_resolution,
        Resolution::Done(outcome) if outcome.verdict.is_success()
    ));
}

#[tokio::test]
async fn browse_fs_authorizes_project_selector_and_fails_closed() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_filesystem_browser(Arc::new(EmptyFilesystemBrowser))
    .with_project_service(Arc::new(AuthorizingProjectService {
        allowed_project_id: "project-scoped".to_string(),
    }));

    let response = services
        .browse_fs_dir(
            caller_with_project(Some("project-alpha")),
            RebornFsListRequest {
                mount: FsMount::Memory,
                path: String::new(),
                project_id: Some(ProjectId::new("project-scoped").expect("project id")),
            },
        )
        .await;
    assert!(response.is_ok(), "authorized project selector must browse");

    let error = services
        .browse_fs_dir(
            caller_with_project(Some("project-alpha")),
            RebornFsListRequest {
                mount: FsMount::Memory,
                path: String::new(),
                project_id: Some(ProjectId::new("project-denied").expect("project id")),
            },
        )
        .await
        .expect_err("unauthorized project selector must fail closed");
    assert_eq!(error.code, ProductSurfaceErrorCode::NotFound);
}

#[tokio::test]
async fn create_thread_scopes_to_authorized_project() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_project_service(Arc::new(AuthorizingProjectService {
        allowed_project_id: "project-scoped".to_string(),
    }));

    // Caller's default scope is project-alpha; the request proposes a different,
    // authorized project, which must become the new thread's scope.
    services
        .create_thread(
            caller_with_project(Some("project-alpha")),
            serde_json::from_value::<ProductCreateThreadRequest>(json!({
                "client_action_id": "create-scoped",
                "requested_thread_id": "thread-scoped",
                "project_id": "project-scoped"
            }))
            .expect("request"),
        )
        .await
        .expect("authorized project create succeeds");

    let record = thread_service
        .read_thread_by_id(ThreadId::new("thread-scoped").expect("thread id"))
        .await
        .expect("created thread exists");
    assert_eq!(
        record.scope.project_id.as_ref().map(|id| id.as_str()),
        Some("project-scoped"),
        "new thread must adopt the authorized project scope"
    );
}

#[tokio::test]
async fn create_thread_rejects_unauthorized_project() {
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_project_service(Arc::new(AuthorizingProjectService {
        allowed_project_id: "project-allowed".to_string(),
    }));

    let err = services
        .create_thread(
            caller_with_project(Some("project-alpha")),
            serde_json::from_value::<ProductCreateThreadRequest>(json!({
                "client_action_id": "create-denied",
                "requested_thread_id": "thread-denied",
                "project_id": "project-forbidden"
            }))
            .expect("request"),
        )
        .await
        .expect_err("a project the caller cannot access must be rejected");

    // Fail closed on the deny→not-found contract: a project the caller can't
    // access collapses to NotFound/404 (no existence oracle), not some
    // unrelated internal error that `expect_err` alone would also accept.
    assert_eq!(err.code, ProductSurfaceErrorCode::NotFound);
    assert_eq!(err.status_code, 404);
}

#[tokio::test]
async fn create_thread_without_proposed_project_keeps_caller_scope() {
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let services = session_services(
        thread_service.clone(),
        Arc::new(FakeTurnCoordinator::default()),
    );

    // No proposed project (and no project service wired): behavior is unchanged —
    // the thread keeps the caller's default project scope.
    services
        .create_thread(
            caller_with_project(Some("project-alpha")),
            serde_json::from_value::<ProductCreateThreadRequest>(json!({
                "client_action_id": "create-default",
                "requested_thread_id": "thread-default"
            }))
            .expect("request"),
        )
        .await
        .expect("default create succeeds");

    let record = thread_service
        .read_thread_by_id(ThreadId::new("thread-default").expect("thread id"))
        .await
        .expect("created thread exists");
    assert_eq!(
        record.scope.project_id.as_ref().map(|id| id.as_str()),
        Some("project-alpha"),
        "without a proposed project the caller's scope is unchanged"
    );
}

#[test]
fn product_surface_descriptor_helpers_keep_view_and_capability_declarations_typed() {
    let query = THREADS_VIEW
        .query(
            ProductListThreadsRequest::default()
                .set_limit(25)
                .set_needs_approval(true),
            Some("cursor-1".to_string()),
        )
        .expect("thread query");

    assert_eq!(query.view_id, THREADS_VIEW.id);
    assert_eq!(query.params["limit"], 25);
    assert_eq!(query.params["needs_approval"], true);
    assert_eq!(query.cursor.as_deref(), Some("cursor-1"));
    assert!(THREADS_VIEW.descriptor().paginated);

    let response = THREADS_VIEW
        .decode_page(RebornViewPage {
            payload: serde_json::to_value(RebornListThreadsResponse {
                threads: Vec::new(),
                next_cursor: Some("cursor-2".to_string()),
            })
            .expect("thread response payload"),
            next_cursor: Some("cursor-2".to_string()),
        })
        .expect("typed thread response");

    assert!(response.threads.is_empty());
    assert_eq!(response.next_cursor.as_deref(), Some("cursor-2"));
}

#[test]
fn service_error_taxonomy_serializes_all_stable_wire_names() {
    let error = ProductSurfaceError {
        code: ProductSurfaceErrorCode::Conflict,
        kind: ProductSurfaceErrorKind::Busy,
        status_code: 409,
        retryable: false,
        field: None,
        validation_code: None,
    };

    let json = serde_json::to_value(&error).expect("error json");

    assert_eq!(json["code"], "conflict");
    assert_eq!(json["kind"], "busy");
    assert_eq!(json["status_code"], 409);
    assert_eq!(json["retryable"], false);

    let cases = [
        (ProductSurfaceErrorKind::Validation, "validation"),
        (ProductSurfaceErrorKind::Duplicate, "duplicate"),
        (ProductSurfaceErrorKind::Busy, "busy"),
        (
            ProductSurfaceErrorKind::ParticipantDenied,
            "participant_denied",
        ),
        (ProductSurfaceErrorKind::BlockedApproval, "blocked_approval"),
        (
            ProductSurfaceErrorKind::BlockedAuthentication,
            "blocked_authentication",
        ),
        (ProductSurfaceErrorKind::BlockedResource, "blocked_resource"),
        (
            ProductSurfaceErrorKind::ReplayUnavailable,
            "replay_unavailable",
        ),
        (
            ProductSurfaceErrorKind::TimelineUnavailable,
            "timeline_unavailable",
        ),
        (
            ProductSurfaceErrorKind::ServiceUnavailable,
            "service_unavailable",
        ),
        (ProductSurfaceErrorKind::NotFound, "not_found"),
        (ProductSurfaceErrorKind::Conflict, "conflict"),
        (ProductSurfaceErrorKind::Internal, "internal"),
    ];
    for (kind, expected) in cases {
        assert_eq!(
            serde_json::to_value(kind).expect("kind json"),
            serde_json::json!(expected),
            "{kind:?} must keep its stable WebUI wire name"
        );
    }
}

#[tokio::test]
async fn trace_reads_are_available_as_product_views() {
    let user_id = format!(
        "trace-query-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    );
    let caller = ProductSurfaceCaller::new(
        TenantId::new("tenant-alpha").expect("tenant"),
        UserId::new(user_id.as_str()).expect("user"),
        None,
        None,
    );
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    );

    let credits_page = services
        .query(
            caller.clone(),
            RebornViewQuery {
                view_id: TRACE_CREDITS_VIEW.id.to_string(),
                params: json!({}),
                cursor: None,
            },
        )
        .await
        .expect("trace credits view");
    let credits: RebornTraceCreditsResponse =
        serde_json::from_value(credits_page.payload).expect("trace credits payload");
    assert!(!credits.enrolled);
    assert_eq!(credits.submissions_total, 0);
    assert!(credits.note.contains("authoritative ledger is server-side"));

    let traces_page = services
        .query(
            caller,
            RebornViewQuery {
                view_id: TRACE_ACCOUNT_TRACES_VIEW.id.to_string(),
                params: json!({}),
                cursor: None,
            },
        )
        .await
        .expect("trace account traces view");
    let traces: RebornAccountTracesResponse =
        serde_json::from_value(traces_page.payload).expect("trace account traces payload");
    assert!(!traces.enrolled);
    assert!(traces.traces.is_empty());
}

#[tokio::test]
async fn skill_reads_are_available_as_product_views() {
    let skills = Arc::new(RecordingSkillsService::default());
    let services = session_services(
        Arc::new(InMemorySessionThreadService::default()),
        Arc::new(FakeTurnCoordinator::default()),
    )
    .with_skills_product_service(skills.clone());

    let list_page = services
        .query(
            caller(),
            RebornViewQuery {
                view_id: SKILLS_VIEW.id.to_string(),
                params: json!({}),
                cursor: None,
            },
        )
        .await
        .expect("skills view");
    let listed: RebornSkillListResponse =
        serde_json::from_value(list_page.payload).expect("skills payload");
    assert_eq!(listed.count, 1);
    assert_eq!(listed.skills[0].name, "local-skill");

    let search_page = services
        .query(
            caller(),
            RebornViewQuery {
                view_id: SKILL_SEARCH_VIEW.id.to_string(),
                params: json!({ "query": "registry" }),
                cursor: None,
            },
        )
        .await
        .expect("skill search view");
    let searched: RebornSkillSearchResponse =
        serde_json::from_value(search_page.payload).expect("skill search payload");
    assert_eq!(searched.registry_url, "https://skills.example.test");
    assert_eq!(
        skills.search_queries.lock().expect("lock").as_slice(),
        ["registry"]
    );

    let content_page = services
        .query(
            caller(),
            RebornViewQuery {
                view_id: SKILL_CONTENT_VIEW.id.to_string(),
                params: json!({ "name": "local-skill" }),
                cursor: None,
            },
        )
        .await
        .expect("skill content view");
    let content: RebornSkillContentResponse =
        serde_json::from_value(content_page.payload).expect("skill content payload");
    assert_eq!(content.name, "local-skill");
    assert_eq!(content.content, "# local-skill\n");
}

#[derive(Default)]
struct RecordingSkillsService {
    search_queries: Mutex<Vec<String>>,
}

#[async_trait]
impl SkillsProductService for RecordingSkillsService {
    async fn list_skills(
        &self,
        _caller: ProductSurfaceCaller,
    ) -> Result<RebornSkillListResponse, ProductSurfaceError> {
        Ok(RebornSkillListResponse {
            skills: vec![skill_info("local-skill")],
            count: 1,
            auto_activate_learned: true,
        })
    }

    async fn search_skills(
        &self,
        _caller: ProductSurfaceCaller,
        query: String,
    ) -> Result<RebornSkillSearchResponse, ProductSurfaceError> {
        self.search_queries.lock().expect("lock").push(query);
        Ok(RebornSkillSearchResponse {
            catalog: vec![json!({ "name": "registry-skill" })],
            installed: vec![skill_info("local-skill")],
            registry_url: "https://skills.example.test".to_string(),
            catalog_error: None,
        })
    }

    async fn read_skill_content(
        &self,
        _caller: ProductSurfaceCaller,
        name: String,
    ) -> Result<RebornSkillContentResponse, ProductSurfaceError> {
        Ok(RebornSkillContentResponse {
            content: format!("# {name}\n"),
            name,
        })
    }
}
