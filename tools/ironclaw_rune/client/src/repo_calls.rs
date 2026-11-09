//! A thread's repository and console on the backend's machine
//! (`/threads/{id}/git`, `/threads/{id}/console`), for when the backend runs
//! on another computer: the Changes, History and Console tabs call these
//! instead of git and a shell on this Mac.
//!
//! The backend picks the folder (the thread's own); nothing here sends a
//! path except repository-relative file paths. An older backend has no such
//! routes, and one without a host shell runs no console:
//! [`remote_unsupported`] tells those apart from real failures.

use serde_json::Value;

pub use ironclaw_product_contracts::thread_repo::{
    ConsoleLine, ConsoleLineKind, GitBranch, GitChange, GitChangeResult, GitCommit, GitCommitView,
    GitDiffScope, GitLogView, GitPatchView, GitStatusView, ThreadConsoleAction, ThreadConsoleView,
    ThreadGitAction,
};

use crate::api::{Client, V2, seg};
use crate::error::ClientError;
use crate::orchestration::decode;

/// The backend cannot do this for a client: it predates the routes (404,
/// 405), the token is not an operator's (403), or it runs no git/console
/// for clients. The UI keeps its "not available on <computer>" line.
pub fn remote_unsupported(error: &ClientError) -> bool {
    match error {
        ClientError::Api { status, .. } => matches!(status, 403 | 404 | 405 | 501),
        ClientError::Refused { code, .. } => matches!(
            code.as_str(),
            "git_unavailable" | "console_unavailable" | "command_streaming_unsupported"
        ),
        _ => false,
    }
}

fn scope_str(scope: GitDiffScope) -> &'static str {
    match scope {
        GitDiffScope::Working => "working",
        GitDiffScope::Staged => "staged",
        GitDiffScope::Unstaged => "unstaged",
        GitDiffScope::Turn => "turn",
    }
}

impl Client {
    fn git_path(thread_id: &str) -> String {
        format!("{V2}/threads/{}/git", seg(thread_id))
    }

    async fn git_read<T: serde::de::DeserializeOwned>(
        &self,
        thread_id: &str,
        query: &str,
    ) -> Result<T, ClientError> {
        let value: Value = self
            .get(&format!("{}?{query}", Self::git_path(thread_id)))
            .await?;
        decode(value)
    }

    /// Branch, ahead/behind and changed paths of the thread's repository.
    pub async fn thread_git_status(&self, thread_id: &str) -> Result<GitStatusView, ClientError> {
        self.git_read(thread_id, "view=status").await
    }

    /// A unified patch of `scope`, whole or for one repository-relative `path`.
    pub async fn thread_git_diff(
        &self,
        thread_id: &str,
        scope: GitDiffScope,
        path: Option<&str>,
    ) -> Result<GitPatchView, ClientError> {
        let mut query = format!("view=diff&scope={}", scope_str(scope));
        if let Some(path) = path {
            query.push_str(&format!("&path={}", seg(path)));
        }
        self.git_read(thread_id, &query).await
    }

    /// Recent commits of every branch, topo-ordered.
    pub async fn thread_git_log(
        &self,
        thread_id: &str,
        limit: u32,
    ) -> Result<GitLogView, ClientError> {
        self.git_read(thread_id, &format!("view=log&limit={limit}"))
            .await
    }

    /// One commit's message and own changes.
    pub async fn thread_git_commit(
        &self,
        thread_id: &str,
        sha: &str,
    ) -> Result<GitCommitView, ClientError> {
        self.git_read(thread_id, &format!("view=commit&sha={}", seg(sha)))
            .await
    }

    /// Stage, unstage, discard, checkpoint or undo the last turn; answers
    /// the status read back after the change.
    pub async fn thread_git_change(
        &self,
        thread_id: &str,
        action: &ThreadGitAction,
    ) -> Result<GitChangeResult, ClientError> {
        let value: Value = self.post(&Self::git_path(thread_id), action).await?;
        decode(value)
    }

    /// The console's lines after `after` and its command's state.
    pub async fn thread_console(
        &self,
        thread_id: &str,
        after: Option<u64>,
    ) -> Result<ThreadConsoleView, ClientError> {
        let mut path = format!("{V2}/threads/{}/console", seg(thread_id));
        if let Some(after) = after {
            path.push_str(&format!("?after={after}"));
        }
        let value: Value = self.get(&path).await?;
        decode(value)
    }

    /// Start, stop or clear the console's command.
    pub async fn thread_console_control(
        &self,
        thread_id: &str,
        action: &ThreadConsoleAction,
    ) -> Result<ThreadConsoleView, ClientError> {
        let path = format!("{V2}/threads/{}/console", seg(thread_id));
        let value: Value = self.post(&path, action).await?;
        decode(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{Connection, Token};
    use crate::test_http::{serve, serve_routes};

    fn connect(url: String) -> Client {
        Client::new(Connection {
            base_url: url,
            token: Token::new("icd_dev_tok").unwrap(),
            token_source: "test".into(),
        })
        .unwrap()
    }

    const STATUS: &str = r#"{"repo_root":"/srv/app","branch":{"name":"main","ahead":1,"behind":0},
        "changes":[{"path":"a.rs","index":" ","worktree":"M"}],"truncated":false,"checkpoint":false}"#;

    #[tokio::test]
    async fn git_reads_and_changes_hit_the_thread_routes() {
        let (base, seen) = serve_routes(vec![
            ("/git?view=status", 200, STATUS),
            ("/git?view=diff", 200, r#"{"patch":"diff --git a/a.rs b/a.rs\n","truncated":false}"#),
            ("POST", 200, r#"{"status":{"repo_root":"/srv/app","branch":{"name":"main","ahead":0,"behind":0},"changes":[],"truncated":false,"checkpoint":false}}"#),
        ])
        .await;
        let client = connect(base);
        let status = client.thread_git_status("th 1").await.unwrap();
        assert_eq!(status.repo_root, "/srv/app");
        assert_eq!(status.changes[0].worktree, 'M');
        let diff = client
            .thread_git_diff("th 1", GitDiffScope::Staged, Some("src/a b.rs"))
            .await
            .unwrap();
        assert!(diff.patch.starts_with("diff --git"));
        let changed = client
            .thread_git_change(
                "th 1",
                &ThreadGitAction::Stage {
                    paths: vec!["a.rs".into()],
                },
            )
            .await
            .unwrap();
        assert!(changed.status.changes.is_empty());
        let seen = seen.lock().unwrap();
        assert!(
            seen[0].line.contains("/threads/th%201/git?view=status"),
            "{}",
            seen[0].line
        );
        assert!(
            seen[1]
                .line
                .contains("view=diff&scope=staged&path=src%2Fa%20b.rs"),
            "{}",
            seen[1].line
        );
        assert!(seen[2].line.starts_with("POST "));
        assert_eq!(
            serde_json::from_str::<Value>(&seen[2].body).unwrap(),
            serde_json::json!({"action": "stage", "paths": ["a.rs"]})
        );
        assert_eq!(seen[0].auth.as_deref(), Some("Bearer icd_dev_tok"));
    }

    #[tokio::test]
    async fn an_older_backend_reads_as_unsupported() {
        let (base, _) = serve(404, r#"{"error":"not_found"}"#).await;
        let client = connect(base);
        let err = client.thread_git_status("t1").await.unwrap_err();
        assert!(remote_unsupported(&err));
        let err = client.thread_console("t1", None).await.unwrap_err();
        assert!(remote_unsupported(&err));
    }

    #[tokio::test]
    async fn correctable_refusals_keep_their_code() {
        let (base, _) = serve(
            200,
            r#"{"ok":false,"error":"path_outside_repository","message":"no"}"#,
        )
        .await;
        let client = connect(base);
        let err = client
            .thread_git_change(
                "t1",
                &ThreadGitAction::Discard {
                    path: "../x".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ClientError::Refused { code, .. } if code == "path_outside_repository")
        );
        assert!(!remote_unsupported(&err), "a refusal is not an old backend");
        let (base, _) = serve(
            200,
            r#"{"ok":false,"error":"console_unavailable","message":"no shell"}"#,
        )
        .await;
        let client = connect(base);
        let err = client
            .thread_console_control(
                "t1",
                &ThreadConsoleAction::Start {
                    command: "ls".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(remote_unsupported(&err));
    }

    #[tokio::test]
    async fn console_polls_after_its_cursor() {
        let (base, seen) = serve(
            200,
            r#"{"available":true,"running":true,"command":"ls","lines":[{"seq":4,"kind":"out","text":"a"}],"next":4,"dropped":0}"#,
        )
        .await;
        let client = connect(base);
        let view = client.thread_console("t1", Some(3)).await.unwrap();
        assert_eq!(view.next, 4);
        assert_eq!(view.lines[0].kind, ConsoleLineKind::Out);
        assert!(seen.lock().unwrap()[0].line.contains("/console?after=3"));
    }
}
