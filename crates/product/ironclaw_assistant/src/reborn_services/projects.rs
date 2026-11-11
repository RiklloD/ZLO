//! Project wire vocabulary for the WebUI v2 service.
//!
//! The DTOs are re-exported from `ironclaw_product_contracts::workspace_views`
//! as part of this crate's public product API (the ~120-DTO re-export set the
//! §11.2.4 import-path rule deliberately leaves ungoverned).
//!
//! The **port** is not here. `trait ProjectService` and `ProjectServiceError`
//! moved to `ironclaw_product_contracts::project_service` on 2026-08-05
//! (PROPOSAL §12.13 D-P) so their implementing adapter could follow the records
//! it gates into `ironclaw_identity::projects`, and the port-location rule
//! forbids a second import path for a contracts-owned trait — so callers name
//! `ironclaw_product_contracts::project_service::{ProjectService,
//! ProjectServiceError}` directly rather than reaching them through this crate.

use super::*;

pub use ironclaw_product_contracts::workspace_views::{
    ProjectCaller, RebornAddMemberRequest, RebornCreateProjectRequest, RebornDeleteProjectRequest,
    RebornGetProjectRequest, RebornListMembersRequest, RebornListMembersResponse,
    RebornListProjectsRequest, RebornListProjectsResponse, RebornProjectInfo,
    RebornProjectMemberInfo, RebornProjectMemberStatus, RebornProjectResponse, RebornProjectRole,
    RebornProjectState, RebornRemoveMemberRequest, RebornUpdateMemberRoleRequest,
    RebornUpdateProjectRequest,
};

pub const PROJECT_UPDATE_CAPABILITY_ID: &str = "builtin.project_update";

pub const PROJECT_UPDATE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(PROJECT_UPDATE_CAPABILITY_ID);

pub const PROJECT_DELETE_CAPABILITY_ID: &str = "builtin.project_delete";

pub const PROJECT_DELETE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(PROJECT_DELETE_CAPABILITY_ID);

pub const PROJECT_MEMBER_ADD_CAPABILITY_ID: &str = "builtin.project_member_add";

pub const PROJECT_MEMBER_ADD_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(PROJECT_MEMBER_ADD_CAPABILITY_ID);

pub const PROJECT_MEMBER_UPDATE_CAPABILITY_ID: &str = "builtin.project_member_update";

pub const PROJECT_MEMBER_UPDATE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(PROJECT_MEMBER_UPDATE_CAPABILITY_ID);

pub const PROJECT_MEMBER_REMOVE_CAPABILITY_ID: &str = "builtin.project_member_remove";

pub const PROJECT_MEMBER_REMOVE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(PROJECT_MEMBER_REMOVE_CAPABILITY_ID);

pub const PROJECT_CREATE_COMMAND_ID: &str = "project.create";

pub const PROJECT_CREATE_COMMAND: ProductSurfaceCommandDescriptor<
    RebornCreateProjectRequest,
    RebornProjectResponse,
> = ProductSurfaceCommandDescriptor::new(PROJECT_CREATE_COMMAND_ID);

pub const PROJECT_FS_READ_COMMAND_ID: &str = "project.fs.read";

pub const PROJECT_FS_READ_COMMAND: ProductSurfaceCommandDescriptor<
    RebornProjectFsReadRequest,
    ProjectFsFile,
> = ProductSurfaceCommandDescriptor::new(PROJECT_FS_READ_COMMAND_ID);

pub const PROJECT_FS_LIST_VIEW: ProductView<
    RebornProjectFsListRequest,
    RebornProjectFsListResponse,
> = ProductView::unpaginated("project_fs_list");

pub const PROJECT_FS_STAT_VIEW: ProductView<
    RebornProjectFsStatRequest,
    RebornProjectFsStatResponse,
> = ProductView::unpaginated("project_fs_stat");

pub const PROJECTS_VIEW: ProductView<RebornListProjectsRequest, RebornListProjectsResponse> =
    ProductView::unpaginated("projects");

pub const PROJECT_VIEW: ProductView<RebornGetProjectRequest, RebornProjectResponse> =
    ProductView::unpaginated("project");

pub const PROJECT_MEMBERS_VIEW: ProductView<RebornListMembersRequest, RebornListMembersResponse> =
    ProductView::unpaginated("project_members");

/// Map a project-filesystem read error to the sanitized service error taxonomy.
/// No host paths or backend strings cross this boundary — only coarse
/// transport/status shape.
pub(super) fn map_project_fs_error(error: ProjectFsError) -> ProductSurfaceError {
    match error {
        ProjectFsError::NotFound => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::NotFound, 404, false)
        }
        ProjectFsError::NotAFile | ProjectFsError::NotADirectory | ProjectFsError::InvalidPath => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::InvalidRequest, 400, false)
        }
        ProjectFsError::Denied => participant_denied(),
        ProjectFsError::TooLarge { .. } => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::InvalidRequest, 413, false)
        }
        ProjectFsError::Unavailable => ProductSurfaceError::service_unavailable(true),
        ProjectFsError::Internal => ProductSurfaceError::internal(),
    }
}

pub(super) fn project_caller(caller: &ProductSurfaceCaller) -> ProjectCaller {
    ProjectCaller {
        tenant_id: caller.tenant_id.clone(),
        user_id: caller.user_id.clone(),
    }
}

pub(super) fn map_project_service_error(error: ProjectServiceError) -> ProductSurfaceError {
    match error {
        ProjectServiceError::NotFound => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::NotFound, 404, false)
        }
        ProjectServiceError::Denied => participant_denied(),
        ProjectServiceError::InvalidInput { field } => {
            let mut error = ProductSurfaceError::from_status(
                ProductSurfaceErrorCode::InvalidRequest,
                400,
                false,
            );
            error.field = Some(field);
            error
        }
        ProjectServiceError::Conflict => {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::Conflict, 409, false)
        }
        ProjectServiceError::Unavailable => ProductSurfaceError::service_unavailable(true),
        ProjectServiceError::Internal => ProductSurfaceError::internal(),
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub(super) async fn list_project_dir(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornProjectFsListRequest,
    ) -> Result<RebornProjectFsListResponse, ProductSurfaceError> {
        let reader = self.require_project_filesystem()?;
        let thread_scope = self
            .authorize_project_fs_access(caller, request.thread_id)
            .await?;
        // dispatch-exempt: read-only, already-authorized workspace listing through
        // the service's own port — not an in-turn mutating tool call, so it does
        // not route through ToolDispatcher.
        let entries = reader
            .list_dir(&thread_scope, &request.path)
            .await
            .map_err(map_project_fs_error)?;
        Ok(RebornProjectFsListResponse { entries })
    }

    pub(super) async fn stat_project_path(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornProjectFsStatRequest,
    ) -> Result<RebornProjectFsStatResponse, ProductSurfaceError> {
        let reader = self.require_project_filesystem()?;
        let thread_scope = self
            .authorize_project_fs_access(caller, request.thread_id)
            .await?;
        // dispatch-exempt: read-only, already-authorized workspace stat through
        // the service's own port — not an in-turn mutating tool call.
        let stat = reader
            .stat(&thread_scope, &request.path)
            .await
            .map_err(map_project_fs_error)?;
        Ok(RebornProjectFsStatResponse { stat })
    }

    pub async fn read_project_file(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornProjectFsReadRequest,
    ) -> Result<ProjectFsFile, ProductSurfaceError> {
        let reader = self.require_project_filesystem()?;
        let thread_scope = self
            .authorize_project_fs_access(caller, request.thread_id)
            .await?;
        // dispatch-exempt: read-only, already-authorized workspace file download
        // through the service's own port — not an in-turn mutating tool call.
        let file = reader
            .read_file(&thread_scope, &request.path)
            .await
            .map_err(map_project_fs_error)?;
        Ok(ProjectFsFile {
            path: file.path.as_str().to_string(),
            filename: file.filename,
            mime_type: file.mime_type,
            size_bytes: file.bytes.len() as u64,
            bytes: file.bytes,
        })
    }

    pub(super) async fn list_projects(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornListProjectsRequest,
    ) -> Result<RebornListProjectsResponse, ProductSurfaceError> {
        let service = self.require_project_service()?;
        service
            .list_projects(project_caller(&caller), request)
            .await
            .map_err(map_project_service_error)
    }

    pub async fn create_project(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornCreateProjectRequest,
    ) -> Result<RebornProjectResponse, ProductSurfaceError> {
        let service = self.require_project_service()?;
        service
            .create_project(project_caller(&caller), request)
            .await
            .map_err(map_project_service_error)
    }

    pub(super) async fn get_project(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornGetProjectRequest,
    ) -> Result<RebornProjectResponse, ProductSurfaceError> {
        let service = self.require_project_service()?;
        service
            .get_project(project_caller(&caller), request)
            .await
            .map_err(map_project_service_error)
    }

    pub(super) async fn update_project(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornUpdateProjectRequest,
    ) -> Result<RebornProjectResponse, ProductSurfaceError> {
        let service = self.require_project_service()?;
        service
            .update_project(project_caller(&caller), request)
            .await
            .map_err(map_project_service_error)
    }

    pub(super) async fn delete_project(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornDeleteProjectRequest,
    ) -> Result<(), ProductSurfaceError> {
        let service = self.require_project_service()?;
        service
            .delete_project(project_caller(&caller), request)
            .await
            .map_err(map_project_service_error)
    }

    pub(super) async fn list_project_members(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornListMembersRequest,
    ) -> Result<RebornListMembersResponse, ProductSurfaceError> {
        let service = self.require_project_service()?;
        service
            .list_members(project_caller(&caller), request)
            .await
            .map_err(map_project_service_error)
    }

    pub(super) async fn add_project_member(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornAddMemberRequest,
    ) -> Result<RebornProjectMemberInfo, ProductSurfaceError> {
        let service = self.require_project_service()?;
        service
            .add_member(project_caller(&caller), request)
            .await
            .map_err(map_project_service_error)
    }

    pub(super) async fn update_project_member_role(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornUpdateMemberRoleRequest,
    ) -> Result<RebornProjectMemberInfo, ProductSurfaceError> {
        let service = self.require_project_service()?;
        service
            .update_member_role(project_caller(&caller), request)
            .await
            .map_err(map_project_service_error)
    }

    pub(super) async fn remove_project_member(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornRemoveMemberRequest,
    ) -> Result<(), ProductSurfaceError> {
        let service = self.require_project_service()?;
        service
            .remove_member(project_caller(&caller), request)
            .await
            .map_err(map_project_service_error)
    }

    pub(super) fn require_project_filesystem(
        &self,
    ) -> Result<&Arc<dyn ProjectFilesystemReader>, ProductSurfaceError> {
        self.project_filesystem
            .as_ref()
            .ok_or_else(|| ProductSurfaceError::service_unavailable(false))
    }

    pub(super) fn require_project_service(
        &self,
    ) -> Result<&Arc<dyn ProjectService>, ProductSurfaceError> {
        self.project_service
            .as_ref()
            .ok_or_else(|| ProductSurfaceError::service_unavailable(false))
    }

    /// Authorize a browser-proposed project for a new thread and, on success,
    /// adopt it as the caller's scope for that thread only.
    ///
    /// The project must never be trusted from the request body alone: the
    /// proposed id is authorized through the same access-controlled
    /// [`get_project`](ProductSurface::get_project) read the project detail
    /// route uses (`Ok` only when the caller can access the project, otherwise a
    /// not-found/denied error). Without a proposed project the caller's default
    /// scope is returned unchanged.
    pub(super) async fn authorize_create_thread_project(
        &self,
        caller: ProductSurfaceCaller,
        requested_project_id: Option<String>,
    ) -> Result<ProductSurfaceCaller, ProductSurfaceError> {
        let Some(raw) = requested_project_id else {
            return Ok(caller);
        };
        let project_id = ProjectId::new(raw).map_err(|error| {
            // Carry the cause to the server log before mapping to the
            // sanitized validation error (.claude/rules/error-handling.md —
            // never `map_err(|_| …)` on a parse/validation failure).
            tracing::debug!(?error, "create_thread received an invalid project_id");
            ProductSurfaceError::validation("project_id", ProductSurfaceValidationCode::InvalidId)
        })?;
        self.authorize_project_caller(caller, project_id).await
    }

    /// Authorize a project selector through the project service and adopt it
    /// only after the access probe succeeds.
    pub(super) async fn authorize_project_caller(
        &self,
        mut caller: ProductSurfaceCaller,
        project_id: ProjectId,
    ) -> Result<ProductSurfaceCaller, ProductSurfaceError> {
        self.get_project(
            caller.clone(),
            RebornGetProjectRequest {
                project_id: project_id.as_str().to_string(),
            },
        )
        .await?;
        caller.project_id = Some(project_id);
        Ok(caller)
    }

    /// Verify the caller may access the thread and return the project-scoped
    /// [`ThreadScope`] its workspace files resolve under. Reuses the same
    /// ownership + automation-trigger fallback probe as event streaming, so a
    /// caller sharing (tenant, agent, project) cannot read another user's
    /// project files by guessing a thread id.
    pub(super) async fn authorize_project_fs_access(
        &self,
        caller: ProductSurfaceCaller,
        thread_id: String,
    ) -> Result<ThreadScope, ProductSurfaceError> {
        let thread_id = parse_thread_id_field("thread_id", thread_id)?;
        let actor = caller.actor();
        let access = self
            .resolve_thread_access_for_caller(caller.clone(), caller.turn_scope(thread_id), &actor)
            .await?;
        thread_scope_from_turn_scope(&access.scope, Some(access.run_actor.user_id.clone()))
    }
}
