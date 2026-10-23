//! Typed calls for the non-chat pages: notifications, automations, projects,
//! and the workspace file browser. Request and response bodies are the
//! gateway's own contract types.

use std::time::Duration;

use ironclaw_product_contracts::notification_inbox::{
    ProductListNotificationsResponse, ProductNotificationMutationResponse,
};
use ironclaw_product_contracts::product_wire::{
    RebornAutomationMutationResponse, RebornListAutomationsResponse,
};
use ironclaw_product_contracts::workspace_views::{
    FsMount, RebornCreateProjectRequest, RebornFsListResponse, RebornFsMountsResponse,
    RebornFsStatResponse, RebornListProjectsResponse, RebornProjectResponse, RebornProjectState,
    RebornUpdateProjectRequest,
};
use reqwest::Method;

use crate::api::{Client, V2, seg};
use crate::error::ClientError;

/// Files larger than this are listed but not previewed.
pub const PREVIEW_LIMIT_BYTES: u64 = 1024 * 1024;

/// What the file pane shows for one file.
#[derive(Debug, Clone)]
pub enum FilePreview {
    Text { text: String, markdown: bool },
    Binary { mime_type: String, size_bytes: u64 },
    TooLarge { size_bytes: u64 },
}

/// Automation actions that share one response shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationAction {
    RunNow,
    Pause,
    Resume,
    Delete,
}

fn mount_query(mount: FsMount, path: &str) -> String {
    let mount = match mount {
        FsMount::Memory => "memory",
        FsMount::Workspace => "workspace",
        FsMount::Skills => "skills",
    };
    format!("mount={mount}&path={}", seg(path))
}

impl Client {
    /// Command routes without an input reject any body, even `null`.
    async fn post_empty<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, ClientError> {
        self.send(self.request(Method::POST, path)).await
    }

    // --- notifications ---

    pub async fn list_notifications(
        &self,
        limit: u32,
    ) -> Result<ProductListNotificationsResponse, ClientError> {
        self.get(&format!("{V2}/notifications?limit={limit}")).await
    }

    pub async fn mark_notification_read(
        &self,
        id: &str,
    ) -> Result<ProductNotificationMutationResponse, ClientError> {
        self.post_empty(&format!("{V2}/notifications/{}/read", seg(id)))
            .await
    }

    pub async fn mark_all_notifications_read(
        &self,
    ) -> Result<ProductNotificationMutationResponse, ClientError> {
        self.post_empty(&format!("{V2}/notifications/read-all"))
            .await
    }

    pub async fn archive_notification(
        &self,
        id: &str,
    ) -> Result<ProductNotificationMutationResponse, ClientError> {
        self.post_empty(&format!("{V2}/notifications/{}/archive", seg(id)))
            .await
    }

    // --- automations ---

    pub async fn list_automations(&self) -> Result<RebornListAutomationsResponse, ClientError> {
        self.get(&format!(
            "{V2}/automations?run_limit=5&include_completed=true"
        ))
        .await
    }

    pub async fn automation_action(
        &self,
        id: &str,
        action: AutomationAction,
    ) -> Result<RebornAutomationMutationResponse, ClientError> {
        let base = format!("{V2}/automations/{}", seg(id));
        match action {
            AutomationAction::RunNow => self.post_empty(&format!("{base}/run")).await,
            AutomationAction::Pause => self.post_empty(&format!("{base}/pause")).await,
            AutomationAction::Resume => self.post_empty(&format!("{base}/resume")).await,
            AutomationAction::Delete => self.send(self.request(Method::DELETE, &base)).await,
        }
    }

    // --- projects (behind the deployment's `reborn_projects` feature) ---

    pub async fn list_projects(&self) -> Result<RebornListProjectsResponse, ClientError> {
        self.get(&format!("{V2}/projects?limit=200")).await
    }

    pub async fn create_project(
        &self,
        name: &str,
        description: &str,
    ) -> Result<RebornProjectResponse, ClientError> {
        let body = RebornCreateProjectRequest {
            name: name.to_string(),
            description: description.to_string(),
            icon: None,
            color: None,
            metadata: None,
        };
        self.post(&format!("{V2}/projects"), &body).await
    }

    pub async fn set_project_state(
        &self,
        id: &str,
        state: RebornProjectState,
    ) -> Result<RebornProjectResponse, ClientError> {
        let body = RebornUpdateProjectRequest {
            project_id: id.to_string(),
            name: None,
            description: None,
            icon: None,
            color: None,
            metadata: None,
            state: Some(state),
        };
        self.post(&format!("{V2}/projects/{}", seg(id)), &body)
            .await
    }

    // --- workspace file browser (read-only) ---

    pub async fn fs_mounts(&self) -> Result<RebornFsMountsResponse, ClientError> {
        self.get(&format!("{V2}/fs/mounts")).await
    }

    pub async fn fs_list(
        &self,
        mount: FsMount,
        path: &str,
    ) -> Result<RebornFsListResponse, ClientError> {
        self.get(&format!("{V2}/fs/list?{}", mount_query(mount, path)))
            .await
    }

    /// Stat first so a large or binary file is never downloaded for preview.
    pub async fn fs_preview(&self, mount: FsMount, path: &str) -> Result<FilePreview, ClientError> {
        let query = mount_query(mount, path);
        let stat: RebornFsStatResponse = self.get(&format!("{V2}/fs/stat?{query}")).await?;
        let stat = stat.stat;
        if stat.size_bytes > PREVIEW_LIMIT_BYTES {
            return Ok(FilePreview::TooLarge {
                size_bytes: stat.size_bytes,
            });
        }
        let resp = self
            .request(Method::GET, &format!("{V2}/fs/content?{query}"))
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(ClientError::from_status(
                status.as_u16(),
                &String::from_utf8_lossy(&bytes),
            ));
        }
        // The gateway's mime guess is extension-based; valid UTF-8 is the
        // better test for "can a person read this".
        Ok(match String::from_utf8(bytes.to_vec()) {
            Ok(text) => FilePreview::Text {
                markdown: path.to_ascii_lowercase().ends_with(".md"),
                text,
            },
            Err(_) => FilePreview::Binary {
                mime_type: stat.mime_type,
                size_bytes: stat.size_bytes,
            },
        })
    }
}
