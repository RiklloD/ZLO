//! Generic multi-mount filesystem-browse read port for the WebUI v2 service.
//!
//! Where [`ProjectFilesystemReader`](super::project_fs::ProjectFilesystemReader)
//! surfaces a single thread's project workspace, this port surfaces the agent's
//! internal filesystem as a standalone, caller-scoped, **read-only** explorer:
//! the persistent memory store, the project working directory (which also holds
//! agent-produced attachments), and the skills tree. It is the backend the
//! WebUI "Workspace / Files" page navigates.
//!
//! Design notes:
//!
//! - **Mount, not thread.** The browse scope is derived by the service from the
//!   authenticated caller (tenant/user/agent/project). An optional typed
//!   project selector is authorized through the project service before it is
//!   adopted; it never directly supplies a filesystem scope. A [`FsMount`] selects *which* virtual mount to read;
//!   paths are mount-relative (`""`/`"/"` is the mount root) so a host or
//!   virtual path is never serialized across the boundary.
//! - **Substrate-free.** Like the project-fs port, this re-uses the coarse
//!   [`ProjectFsEntry`]/[`ProjectFsStat`]/[`ProjectFsFile`]/[`ProjectFsError`]
//!   shapes and knows nothing about `ironclaw_filesystem`. The alias→target
//!   mapping, path confinement, and sensitive-name filtering live in the host
//!   composition impl.
//! - **Read-only.** No `put`/`write` — this is a navigation + preview/download
//!   surface only. The agent's own tools and the memory write tools remain the
//!   sole mutation path.

use super::*;

use async_trait::async_trait;
use ironclaw_host_api::resource::ResourceScope;

use ironclaw_product_contracts::workspace_views::{
    ProjectFsEntry, ProjectFsError, ProjectFsFile, ProjectFsStat,
};

pub use ironclaw_product_contracts::workspace_views::{
    FsMount, RebornFsListRequest, RebornFsListResponse, RebornFsMountInfo, RebornFsMountsRequest,
    RebornFsMountsResponse, RebornFsReadRequest, RebornFsStatRequest, RebornFsStatResponse,
};

/// Read-only navigation + download access to the agent's internal filesystem
/// across multiple logical mounts.
///
/// Every method takes a [`ResourceScope`] the service has already derived from
/// the authenticated caller and authorized; mutations are intentionally absent.
/// Entry/stat paths are mount-relative — the same value passes back to
/// [`Self::read_file`]/[`Self::stat`].
#[async_trait]
pub trait FilesystemBrowseReader: Send + Sync {
    /// The mounts this composition can actually serve. The service filters
    /// requests against this set so an unwired mount yields a clean
    /// "not found" rather than a backend error.
    fn available_mounts(&self) -> Vec<FsMount>;

    /// List the entries directly under `path` (mount-relative) on `mount`.
    async fn list_dir(
        &self,
        scope: &ResourceScope,
        mount: FsMount,
        path: &str,
    ) -> Result<Vec<ProjectFsEntry>, ProjectFsError>;

    /// Read the bytes of the regular file at `path` on `mount`, with metadata.
    async fn read_file(
        &self,
        scope: &ResourceScope,
        mount: FsMount,
        path: &str,
    ) -> Result<ProjectFsFile, ProjectFsError>;

    /// Return metadata for `path` on `mount` without reading its bytes.
    async fn stat(
        &self,
        scope: &ResourceScope,
        mount: FsMount,
        path: &str,
    ) -> Result<ProjectFsStat, ProjectFsError>;
}

pub const FS_READ_COMMAND_ID: &str = "fs.read";

pub const FS_READ_COMMAND: ProductSurfaceCommandDescriptor<RebornFsReadRequest, ProjectFsFile> =
    ProductSurfaceCommandDescriptor::new(FS_READ_COMMAND_ID);

pub const ATTACHMENT_READ_COMMAND_ID: &str = "attachment.read";

pub const ATTACHMENT_READ_COMMAND: ProductSurfaceCommandDescriptor<
    RebornAttachmentRequest,
    RebornAttachmentBytes,
> = ProductSurfaceCommandDescriptor::new(ATTACHMENT_READ_COMMAND_ID);

pub const FS_MOUNTS_VIEW: ProductView<RebornFsMountsRequest, RebornFsMountsResponse> =
    ProductView::unpaginated("fs_mounts");

pub const FS_LIST_VIEW: ProductView<RebornFsListRequest, RebornFsListResponse> =
    ProductView::unpaginated("fs_list");

pub const FS_STAT_VIEW: ProductView<RebornFsStatRequest, RebornFsStatResponse> =
    ProductView::unpaginated("fs_stat");

/// Derive the read-only browse scope from the authenticated caller.
///
/// The standalone filesystem viewer is not thread-bound, so the scope comes
/// straight from the trusted caller identity (tenant/user/agent/project) — never
/// from the request body. A fresh `invocation_id` is minted per call; the
/// scoped filesystem namespaces storage by tenant/user/agent/project, so this
/// addresses the same mount the agent's own tools wrote through.
pub(super) fn caller_browse_scope(caller: &ProductSurfaceCaller) -> ResourceScope {
    ResourceScope {
        tenant_id: caller.tenant_id.clone(),
        user_id: caller.user_id.clone(),
        agent_id: caller.agent_id.clone(),
        project_id: caller.project_id.clone(),
        mission_id: None,
        thread_id: None,
        invocation_id: InvocationId::new(),
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub(super) async fn list_fs_mounts(
        &self,
        _caller: ProductSurfaceCaller,
    ) -> Result<RebornFsMountsResponse, ProductSurfaceError> {
        // No wired browser is not an error: the UI renders an empty viewer.
        let mounts = self
            .filesystem_browser
            .as_ref()
            .map(|browser| {
                browser
                    .available_mounts()
                    .into_iter()
                    .map(|mount| RebornFsMountInfo {
                        mount,
                        label: mount.label().to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(RebornFsMountsResponse { mounts })
    }

    pub async fn browse_fs_dir(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornFsListRequest,
    ) -> Result<RebornFsListResponse, ProductSurfaceError> {
        let browser = self.require_filesystem_browser(request.mount)?;
        let scope = self
            .authorize_browse_scope(caller, request.project_id)
            .await?;
        // dispatch-exempt: read-only, caller-scoped internal-filesystem listing
        // through the service's own port — not an in-turn mutating tool call.
        let entries = browser
            .list_dir(&scope, request.mount, &request.path)
            .await
            .map_err(map_project_fs_error)?;
        Ok(RebornFsListResponse {
            mount: request.mount,
            path: request.path,
            entries,
        })
    }

    pub(super) async fn stat_fs_path(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornFsStatRequest,
    ) -> Result<RebornFsStatResponse, ProductSurfaceError> {
        let browser = self.require_filesystem_browser(request.mount)?;
        let scope = self
            .authorize_browse_scope(caller, request.project_id)
            .await?;
        // dispatch-exempt: read-only, caller-scoped internal-filesystem stat.
        let stat = browser
            .stat(&scope, request.mount, &request.path)
            .await
            .map_err(map_project_fs_error)?;
        Ok(RebornFsStatResponse { stat })
    }

    pub async fn read_fs_file(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornFsReadRequest,
    ) -> Result<ProjectFsFile, ProductSurfaceError> {
        let browser = self.require_filesystem_browser(request.mount)?;
        let scope = self
            .authorize_browse_scope(caller, request.project_id)
            .await?;
        // dispatch-exempt: read-only, caller-scoped internal-filesystem download.
        browser
            .read_file(&scope, request.mount, &request.path)
            .await
            .map_err(map_project_fs_error)
    }

    pub async fn read_attachment(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornAttachmentRequest,
    ) -> Result<RebornAttachmentBytes, ProductSurfaceError> {
        let thread_id = parse_thread_id_field("thread_id", request.thread_id)?;
        let message_id = ThreadMessageId::parse(&request.message_id).map_err(|_| {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::NotFound, 404, false)
        })?;
        let scope = caller.turn_scope(thread_id);

        // Resolve the thread the same way the timeline does (including the
        // automation-trigger fallback) and read the bytes back through the
        // scope the history actually lives under — for a trigger-fired thread
        // that is the creator's scope, not the caller's session scope, so the
        // reader addresses the right project mount.
        //
        // This loads the whole thread history to find one ref, so it is
        // O(messages) per fetch. Acceptable for now: the cost equals the
        // timeline load already incurred when the thread is open, and the
        // browser caches each attachment (private max-age plus the resolved
        // data/blob URL), so it is one fetch per attachment per session. A
        // single-message fast path would need a new scope-validated "load one
        // message *record* by id" service method — `load_context_messages`
        // projects to `ContextMessage`, which carries only image refs (no
        // filename, no non-image kinds), so it can't resolve an arbitrary
        // attachment. Left as a follow-up rather than widening the thread
        // service contract here.
        let (thread_scope, history) = self
            .resolve_thread_history_for_caller(caller, &scope)
            .await?;

        // The (message, attachment-id) pair is required: an attachment id is
        // only unique within its message. Resolve the ref server-side so the
        // browser never supplies the storage path and the Content-Type is
        // authoritative.
        let attachment = history
            .messages
            .iter()
            .find(|message| message.message_id == message_id)
            .and_then(|message| {
                message
                    .attachments
                    .iter()
                    .find(|attachment| attachment.id == request.attachment_id)
            })
            .ok_or_else(|| {
                ProductSurfaceError::from_status(ProductSurfaceErrorCode::NotFound, 404, false)
            })?;

        let storage_key = attachment.storage_key.as_deref().ok_or_else(|| {
            // An attachment that never landed has no bytes to serve.
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::NotFound, 404, false)
        })?;

        // The ref landed (it has a storage_key) but no read port is wired: that
        // is a composition fault, not an absent file. Surface a retryable 503
        // rather than a 404 that would make real bytes look gone. (In the
        // shipped composition the reader and lander are wired together, so this
        // only trips a misconfigured custom host.)
        let Some(reader) = self.inbound_attachment_reader.as_ref() else {
            // Not retryable: a missing port won't appear on a retry, it needs
            // composition wiring.
            return Err(ProductSurfaceError::service_unavailable(false));
        };

        let bytes = reader.read(&thread_scope, storage_key).await?;
        Ok(RebornAttachmentBytes {
            mime_type: attachment.mime_type.clone(),
            filename: attachment.filename.clone(),
            bytes,
        })
    }

    /// Resolve the wired browse reader and verify it serves the requested
    /// mount. An unwired reader is a 503 (composition fault, retryable-false);
    /// a known-but-unserved mount is a 404 so probing an unavailable mount
    /// cannot distinguish "wrong path" from "not wired".
    pub(super) fn require_filesystem_browser(
        &self,
        mount: FsMount,
    ) -> Result<&Arc<dyn FilesystemBrowseReader>, ProductSurfaceError> {
        let browser = self
            .filesystem_browser
            .as_ref()
            .ok_or_else(|| ProductSurfaceError::service_unavailable(false))?;
        if !browser.available_mounts().contains(&mount) {
            return Err(ProductSurfaceError::from_status(
                ProductSurfaceErrorCode::NotFound,
                404,
                false,
            ));
        }
        Ok(browser)
    }

    /// Resolve the one authorized scope used by all standalone browse reads.
    /// An omitted selector preserves the caller's existing project scope.
    pub(super) async fn authorize_browse_scope(
        &self,
        caller: ProductSurfaceCaller,
        project_id: Option<ProjectId>,
    ) -> Result<ResourceScope, ProductSurfaceError> {
        let caller = match project_id {
            Some(project_id) => self.authorize_project_caller(caller, project_id).await?,
            None => caller,
        };
        Ok(caller_browse_scope(&caller))
    }
}
