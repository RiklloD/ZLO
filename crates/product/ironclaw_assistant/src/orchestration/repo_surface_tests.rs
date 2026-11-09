//! A thread's repository and console through the orchestrating surface:
//! operator callers only, the caller's own threads only, the read-back
//! status after a change, and the console's typed "unavailable".

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::ids::{ActivityId, CapabilityId, TenantId, ThreadId, UserId};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::model::{RuntimeId, ThreadRecord};
use ironclaw_orchestrator::runtime::*;
use ironclaw_orchestrator::worktree::{
    HookApproval, HookApprover, HookDecision, HostCommand, HostCommandEvent, HostCommandOutput,
    HostCommandProcess, HostCommandRunner, WorktreeHost,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use ironclaw_product_contracts::thread_repo::*;
use serde_json::{Value, json};

use super::surface::{OrchestratedProductSurface, scope_of};

struct Inner;

#[async_trait]
impl ProductSurface for Inner {
    async fn invoke(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
    async fn query(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
    async fn stream_events(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
}

/// Runs `sh -c` to completion, then replays its output as a stream.
struct ReplayRunner;

struct Replay(Vec<HostCommandEvent>);

#[async_trait]
impl HostCommandProcess for Replay {
    async fn next(&mut self) -> Option<HostCommandEvent> {
        (!self.0.is_empty()).then(|| self.0.remove(0))
    }
    fn kill(&mut self) {}
}

fn shell(c: &HostCommand) -> std::process::Output {
    std::process::Command::new("sh")
        .arg("-c")
        .arg(&c.line)
        .current_dir(&c.workdir)
        .output()
        .unwrap()
}

#[async_trait]
impl HostCommandRunner for ReplayRunner {
    async fn run(&self, c: HostCommand) -> Result<HostCommandOutput, OrchestratorError> {
        let out = shell(&c);
        Ok(HostCommandOutput {
            exit_code: i64::from(out.status.code().unwrap_or(-1)),
            output: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
    }
    async fn spawn(
        &self,
        c: HostCommand,
    ) -> Result<Box<dyn HostCommandProcess>, OrchestratorError> {
        let out = shell(&c);
        Ok(Box::new(Replay(vec![
            HostCommandEvent::Stdout(out.stdout),
            HostCommandEvent::Stderr(out.stderr),
            HostCommandEvent::Exited {
                code: out.status.code(),
                timed_out: false,
            },
        ])))
    }
}

struct NoApprover;

#[async_trait]
impl HookApprover for NoApprover {
    async fn approve(&self, _: &HookApproval) -> HookDecision {
        HookDecision::Unavailable
    }
}

struct HomeRuntime(PathBuf);

#[async_trait]
impl TurnRuntime for HomeRuntime {
    fn id(&self) -> RuntimeId {
        RuntimeId::new("home").unwrap()
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::default()
    }
    fn default_work_dir(&self) -> Option<PathBuf> {
        Some(self.0.clone())
    }
    async fn create_thread(&self, _: CreateThread) -> Result<CreatedThread, RuntimeError> {
        Err(RuntimeError::Unsupported("create"))
    }
    async fn start_turn(
        &self,
        _: &ThreadRecord,
        _: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        Err(RuntimeError::Unsupported("turn"))
    }
    async fn steer(
        &self,
        _: &ThreadRecord,
        _: &str,
        _: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        Ok(SteerOutcome::Steered)
    }
    async fn cancel(&self, _: &ThreadRecord, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn history(
        &self,
        _: &ThreadRecord,
        _: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        Ok(Vec::new())
    }
}

fn caller(user: &str, operator: bool) -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new(user).unwrap(),
        None,
        None,
    )
    .with_operator_config(operator)
}

fn sh(dir: &Path, line: &str) {
    let ok = std::process::Command::new("sh")
        .args(["-c", line])
        .current_dir(dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "{line}");
}

struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    surface: OrchestratedProductSurface,
}

async fn fixture(console: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("project");
    std::fs::create_dir_all(&repo).unwrap();
    sh(
        &repo,
        "git init -q -b main && echo one > a.txt && git add a.txt",
    );
    sh(
        &repo,
        "git -c user.name=t -c user.email=t@example.com commit -q -m first",
    );
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    engine.register_runtime(Arc::new(HomeRuntime(repo.clone())));
    engine.set_worktree_host(WorktreeHost {
        runner: Arc::new(ReplayRunner),
        root: dir.path().join("worktrees"),
        approver: Arc::new(NoApprover),
    });
    if console {
        engine.set_console_runner(Arc::new(ReplayRunner));
    }
    let home = RuntimeId::new("home").unwrap();
    for (thread, user) in [("mine", "owner"), ("theirs", "other")] {
        engine
            .ensure_thread(
                &ThreadId::new(thread).unwrap(),
                &scope_of(&caller(user, true)),
                Some(&home),
            )
            .await
            .unwrap();
    }
    Fixture {
        _dir: dir,
        repo,
        surface: OrchestratedProductSurface::new(Arc::new(Inner), engine, None),
    }
}

impl Fixture {
    async fn view(
        &self,
        who: ProductSurfaceCaller,
        view: &str,
        input: Value,
    ) -> Result<Value, ProductSurfaceError> {
        let request = ProductSurfaceQueryRequest {
            view_id: view.to_string(),
            input,
            cursor: None,
            limit: None,
        };
        let page = self.surface.query(who, request).await?;
        Ok(page.items.into_iter().next().unwrap_or(Value::Null))
    }

    async fn invoke(
        &self,
        who: ProductSurfaceCaller,
        operation: &str,
        input: Value,
    ) -> Result<Value, ProductSurfaceError> {
        let request = ProductSurfaceInvokeRequest {
            operation_id: CapabilityId::new(operation).unwrap(),
            input,
            activity_id: ActivityId::new(),
        };
        Ok(self.surface.invoke(who, request).await?.output)
    }
}

#[tokio::test]
async fn git_reads_and_changes_are_for_operators_on_their_own_threads() {
    let f = fixture(false).await;
    std::fs::write(f.repo.join("a.txt"), "two\n").unwrap();
    let status = json!({"thread_id": "mine", "view": "status"});

    // A caller without operator authority is refused before anything runs.
    let refused = f
        .view(caller("owner", false), THREAD_GIT_VIEW.id, status.clone())
        .await
        .unwrap_err();
    assert_eq!(refused.http_parts().status_code, 403);
    let refused = f
        .invoke(
            caller("owner", false),
            THREAD_GIT_CHANGE_COMMAND_ID,
            json!({"thread_id": "mine", "action": "stage", "paths": ["a.txt"]}),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.http_parts().status_code, 403);
    assert!(
        std::process::Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .current_dir(&f.repo)
            .status()
            .unwrap()
            .success(),
        "nothing was staged"
    );

    // Someone else's thread reads as missing.
    let missing = f
        .view(
            caller("owner", true),
            THREAD_GIT_VIEW.id,
            json!({"thread_id": "theirs", "view": "status"}),
        )
        .await
        .unwrap_err();
    assert_eq!(missing.http_parts().status_code, 404);

    // The owner, as operator: status, then stage with its read-back.
    let view = f
        .view(caller("owner", true), THREAD_GIT_VIEW.id, status)
        .await
        .unwrap();
    let view: GitStatusView = serde_json::from_value(view).unwrap();
    assert_eq!(view.changes.len(), 1);
    assert_eq!(view.changes[0].worktree, 'M');
    let staged = f
        .invoke(
            caller("owner", true),
            THREAD_GIT_CHANGE_COMMAND_ID,
            json!({"thread_id": "mine", "action": "stage", "paths": ["a.txt"]}),
        )
        .await
        .unwrap();
    let staged: GitChangeResult = serde_json::from_value(staged).unwrap();
    assert_eq!(staged.status.changes[0].index, 'M');
    assert_eq!(staged.status.changes[0].worktree, ' ');

    // A path outside the repository is a correctable refusal, not a fault.
    let outside = f
        .invoke(
            caller("owner", true),
            THREAD_GIT_CHANGE_COMMAND_ID,
            json!({"thread_id": "mine", "action": "discard", "path": "../x"}),
        )
        .await
        .unwrap();
    assert_eq!(outside["error"], "path_outside_repository", "{outside}");
    // A request cannot name its own folder.
    let named = f
        .invoke(
            caller("owner", true),
            THREAD_GIT_CHANGE_COMMAND_ID,
            json!({"thread_id": "mine", "action": "checkpoint", "repo": "/"}),
        )
        .await
        .unwrap_err();
    assert_eq!(named.http_parts().status_code, 400);
}

#[tokio::test]
async fn the_console_is_operator_only_and_says_when_it_is_unavailable() {
    let f = fixture(false).await;
    let view = json!({"thread_id": "mine"});
    let refused = f
        .view(caller("owner", false), THREAD_CONSOLE_VIEW.id, view.clone())
        .await
        .unwrap_err();
    assert_eq!(refused.http_parts().status_code, 403);
    let off = f
        .view(caller("owner", true), THREAD_CONSOLE_VIEW.id, view)
        .await
        .unwrap();
    assert_eq!(off["available"], false, "{off}");
    let start = f
        .invoke(
            caller("owner", true),
            THREAD_CONSOLE_CONTROL_COMMAND_ID,
            json!({"thread_id": "mine", "action": "start", "command": "touch ran"}),
        )
        .await
        .unwrap();
    assert_eq!(start["error"], "console_unavailable", "{start}");
    assert!(!f.repo.join("ran").exists());
}

#[tokio::test]
async fn the_console_runs_a_command_in_the_thread_folder() {
    let f = fixture(true).await;
    let refused = f
        .invoke(
            caller("owner", false),
            THREAD_CONSOLE_CONTROL_COMMAND_ID,
            json!({"thread_id": "mine", "action": "start", "command": "touch ran"}),
        )
        .await
        .unwrap_err();
    assert_eq!(refused.http_parts().status_code, 403);
    assert!(!f.repo.join("ran").exists());
    f.invoke(
        caller("owner", true),
        THREAD_CONSOLE_CONTROL_COMMAND_ID,
        json!({"thread_id": "mine", "action": "start", "command": "touch ran; echo made"}),
    )
    .await
    .unwrap();
    let mut seen = Value::Null;
    for _ in 0..100 {
        seen = f
            .view(
                caller("owner", true),
                THREAD_CONSOLE_VIEW.id,
                json!({"thread_id": "mine"}),
            )
            .await
            .unwrap();
        if seen["running"] == false {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let view: ThreadConsoleView = serde_json::from_value(seen).unwrap();
    assert!(view.lines.iter().any(|l| l.text == "made"), "{view:?}");
    assert_eq!(view.exit_code, Some(0));
    assert!(f.repo.join("ran").exists());
}
