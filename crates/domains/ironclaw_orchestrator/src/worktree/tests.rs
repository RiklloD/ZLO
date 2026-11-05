//! Worktree isolation over real temporary git repositories, driven through
//! the model-facing tools (`t3_delegate`, `t3_thread`) and turn events.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use serde_json::{Value, json};

use super::{HostCommand, HostCommandOutput, HostCommandRunner, WorktreeHost, WorktreeState};
use crate::engine::{CallContext, Engine};
use crate::error::OrchestratorError;
use crate::model::*;
use crate::runtime::*;
use crate::store::Store;

/// Runs commands with `sh -c` like the host process port does.
struct ShellRunner;

#[async_trait]
impl HostCommandRunner for ShellRunner {
    async fn run(&self, c: HostCommand) -> Result<HostCommandOutput, OrchestratorError> {
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(format!("({}) 2>&1", c.line))
            .current_dir(&c.workdir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .await
            .map_err(|e| OrchestratorError::request("command_failed", e.to_string()))?;
        Ok(HostCommandOutput {
            exit_code: i64::from(out.status.code().unwrap_or(-1)),
            output: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
    }
}

/// A CLI-like runtime whose agents work in `home` unless a turn names a
/// worktree. Records every turn's text and working directory.
struct DirRuntime {
    home: PathBuf,
    turns: Mutex<Vec<(ThreadId, String, Option<PathBuf>)>>,
    created: Mutex<u32>,
}

#[async_trait]
impl TurnRuntime for DirRuntime {
    fn id(&self) -> RuntimeId {
        RuntimeId::new("fake").unwrap()
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::default()
    }
    fn supports_work_dir(&self) -> bool {
        true
    }
    fn default_work_dir(&self) -> Option<PathBuf> {
        Some(self.home.clone())
    }
    async fn create_thread(&self, _r: CreateThread) -> Result<CreatedThread, RuntimeError> {
        let mut n = self.created.lock().unwrap();
        *n += 1;
        Ok(CreatedThread {
            thread_id: ThreadId::new(format!("child-{n}")).unwrap(),
            native_session_id: None,
            provider: None,
            model: None,
        })
    }
    async fn start_turn(
        &self,
        t: &ThreadRecord,
        input: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        self.turns
            .lock()
            .unwrap()
            .push((t.thread_id.clone(), input.text, input.work_dir));
        Ok(TurnAdmission::Started {
            run_id: Some(format!("run-{}", now_ms())),
        })
    }
    async fn steer(
        &self,
        _t: &ThreadRecord,
        _text: &str,
        _h: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        Ok(SteerOutcome::Steered)
    }
    async fn cancel(&self, _t: &ThreadRecord, _reason: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn history(
        &self,
        _t: &ThreadRecord,
        _limit: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        Ok(Vec::new())
    }
}

struct Repo {
    _dir: tempfile::TempDir,
    engine: Arc<Engine>,
    rt: Arc<DirRuntime>,
    repo: PathBuf,
    root: PathBuf,
}

fn sh(dir: &Path, line: &str) -> String {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(line)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{line}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const GIT: &str = "git -c user.name=t -c user.email=t@example.com";

fn commit_all(dir: &Path, message: &str) {
    sh(
        dir,
        &format!("git add -A && {GIT} commit -q -m '{message}'"),
    );
}

fn scope() -> ThreadScope {
    ThreadScope {
        tenant_id: TenantId::new("t1").unwrap(),
        user_id: UserId::new("u1").unwrap(),
        agent_id: None,
        project_id: None,
    }
}

fn ctx(thread: &str) -> CallContext {
    CallContext {
        thread_id: ThreadId::new(thread).unwrap(),
        scope: scope(),
        provider: None,
        model: None,
    }
}

/// A repository with one commit, an engine whose parent thread works in
/// it, and worktrees under a separate folder.
async fn repo(git: bool) -> Repo {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("project");
    std::fs::create_dir_all(&repo).unwrap();
    if git {
        sh(&repo, "git init -q -b main");
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        commit_all(&repo, "first");
    }
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open(Store::new(runtime), RuntimeId::new("fake").unwrap())
        .await
        .unwrap();
    let rt = Arc::new(DirRuntime {
        home: repo.clone(),
        turns: Mutex::new(Vec::new()),
        created: Mutex::new(0),
    });
    engine.register_runtime(rt.clone());
    let root = dir.path().join("worktrees");
    engine.set_worktree_host(WorktreeHost {
        runner: Arc::new(ShellRunner),
        root: root.clone(),
    });
    engine
        .ensure_thread(&ThreadId::new("parent").unwrap(), &scope(), None)
        .await
        .unwrap();
    Repo {
        _dir: dir,
        engine,
        rt,
        repo,
        root,
    }
}

impl Repo {
    async fn delegate(&self, task: &str) -> Value {
        self.engine
            .call_tool(
                "t3_delegate",
                &ctx("parent"),
                &json!({"task": task, "title": "Fix the parser", "isolation": "worktree"}),
            )
            .await
    }

    async fn thread_tool(&self, args: Value) -> Value {
        self.engine
            .call_tool("t3_thread", &ctx("parent"), &args)
            .await
    }

    async fn finish(&self, child: &ThreadId) {
        let t = self.engine.store().thread(child).await.unwrap().unwrap();
        self.engine
            .turn_finished(
                child,
                t.active_run_id.clone(),
                TurnOutcome::completed(Some("done".into())),
                now_ms(),
            )
            .await;
    }

    async fn task(&self, id: &str) -> TaskRecord {
        self.engine
            .store()
            .task(&TaskId::new(id).unwrap())
            .await
            .unwrap()
            .unwrap()
    }

    fn turn_of(&self, thread: &ThreadId) -> (String, Option<PathBuf>) {
        let turns = self.rt.turns.lock().unwrap();
        let (_, text, dir) = turns.iter().find(|(t, _, _)| t == thread).unwrap();
        (text.clone(), dir.clone())
    }
}

fn child_of(view: &Value) -> ThreadId {
    ThreadId::new(view["childThreadId"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn a_child_works_in_its_own_worktree_and_a_clean_finish_removes_it() {
    let r = repo(true).await;
    // Uncommitted parent work stays out of the child's tree.
    std::fs::write(r.repo.join("a.txt"), "parent edit\n").unwrap();
    std::fs::write(r.repo.join("wip.txt"), "draft\n").unwrap();
    let view = r.delegate("fix it").await;
    assert_eq!(view["status"], "running", "{view}");
    let child = child_of(&view);
    let binding = r.engine.store().worktree(&child).await.unwrap().unwrap();
    assert_eq!(binding.state, WorktreeState::Active);
    assert!(binding.path.starts_with(&r.root));
    assert!(binding.branch.as_str().starts_with("zlo/fix-the-parser-"));
    assert_eq!(view["worktree"]["branch"], binding.branch.as_str());
    let chips = r.engine.store().live_worktree_branches().await.unwrap();
    assert_eq!(chips.get(child.as_str()), Some(&binding.branch.to_string()));

    let (brief, dir) = r.turn_of(&child);
    assert_eq!(dir.as_deref(), Some(binding.path.as_path()));
    assert!(brief.contains(binding.branch.as_str()), "{brief}");
    assert!(brief.contains("uncommitted edits are not in it"), "{brief}");
    assert_eq!(
        std::fs::read_to_string(binding.path.join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(!binding.path.join("wip.txt").exists());
    assert_eq!(
        sh(&binding.path, "git branch --show-current"),
        binding.branch.as_str()
    );

    r.finish(&child).await;
    let task = r.task(view["taskId"].as_str().unwrap()).await;
    assert_eq!(task.status, TaskStatus::Completed);
    assert!(task.result.unwrap().contains("no changes"));
    let after = r.engine.store().worktree(&child).await.unwrap().unwrap();
    assert_eq!(after.state, WorktreeState::Removed);
    let chips = r.engine.store().live_worktree_branches().await.unwrap();
    assert!(!chips.contains_key(child.as_str()));
    assert!(!binding.path.exists());
    assert_eq!(
        sh(&r.repo, &format!("git branch --list {}", binding.branch)),
        ""
    );
    // The parent's uncommitted work is untouched.
    assert_eq!(
        std::fs::read_to_string(r.repo.join("a.txt")).unwrap(),
        "parent edit\n"
    );
}

#[tokio::test]
async fn work_is_kept_then_merged_back_and_cleaned_up() {
    let r = repo(true).await;
    let view = r.delegate("add b").await;
    let child = child_of(&view);
    let path = r
        .engine
        .store()
        .worktree(&child)
        .await
        .unwrap()
        .unwrap()
        .path;
    std::fs::write(path.join("b.txt"), "from child\n").unwrap();
    commit_all(&path, "child adds b");
    std::fs::write(path.join("c.txt"), "left uncommitted\n").unwrap();

    r.finish(&child).await;
    let task = r.task(view["taskId"].as_str().unwrap()).await;
    let result = task.result.unwrap();
    assert!(result.contains("Worktree kept"), "{result}");
    assert!(result.contains("merge_worktree"), "{result}");
    let status = r
        .thread_tool(json!({"action": "worktree_status", "thread": child.as_str()}))
        .await;
    assert_eq!(status["state"], "kept", "{status}");
    assert_eq!(status["aheadOfBase"], 1);
    assert_eq!(status["ahead"], 1);
    assert_eq!(status["behind"], 0);
    assert_eq!(status["dirtyCount"], 1);

    let merged = r
        .thread_tool(json!({"action": "merge_worktree", "thread": child.as_str()}))
        .await;
    assert_eq!(merged["ok"], true, "{merged}");
    assert_eq!(
        merged["commits"], 2,
        "the uncommitted file was committed first"
    );
    assert_eq!(
        std::fs::read_to_string(r.repo.join("b.txt")).unwrap(),
        "from child\n"
    );
    assert!(r.repo.join("c.txt").exists());
    assert_eq!(sh(&r.repo, "git status --porcelain"), "");

    let removed = r
        .thread_tool(json!({"action": "remove_worktree", "thread": child.as_str()}))
        .await;
    assert_eq!(removed["state"], "removed", "{removed}");
    assert_eq!(removed["branchKept"], false);
    assert!(!path.exists());
}

#[tokio::test]
async fn conflicts_are_reported_and_the_checkout_is_left_as_it_was() {
    let r = repo(true).await;
    let view = r.delegate("edit a").await;
    let child = child_of(&view);
    let path = r
        .engine
        .store()
        .worktree(&child)
        .await
        .unwrap()
        .unwrap()
        .path;
    std::fs::write(path.join("a.txt"), "child\n").unwrap();
    commit_all(&path, "child edits a");
    std::fs::write(r.repo.join("a.txt"), "parent\n").unwrap();
    commit_all(&r.repo, "parent edits a");
    let head = sh(&r.repo, "git rev-parse HEAD");
    r.finish(&child).await;

    let merged = r
        .thread_tool(json!({"action": "merge_worktree", "thread": child.as_str()}))
        .await;
    assert_eq!(merged["ok"], false);
    assert_eq!(merged["error"], "merge_conflict", "{merged}");
    assert!(merged["message"].as_str().unwrap().contains("a.txt"));
    assert_eq!(sh(&r.repo, "git rev-parse HEAD"), head);
    assert_eq!(sh(&r.repo, "git status --porcelain"), "");
    assert_eq!(
        std::fs::read_to_string(r.repo.join("a.txt")).unwrap(),
        "parent\n"
    );
    let b = r.engine.store().worktree(&child).await.unwrap().unwrap();
    assert_eq!(b.state, WorktreeState::Kept);
}

#[tokio::test]
async fn removing_keeps_unmerged_work_on_its_branch_and_an_archive_ref() {
    let r = repo(true).await;
    let view = r.delegate("draft").await;
    let child = child_of(&view);
    let b = r.engine.store().worktree(&child).await.unwrap().unwrap();
    std::fs::write(b.path.join("d.txt"), "draft\n").unwrap();
    r.finish(&child).await;

    let removed = r
        .thread_tool(json!({"action": "remove_worktree", "thread": child.as_str()}))
        .await;
    assert_eq!(removed["branchKept"], true, "{removed}");
    let archive = format!("refs/zlo/archive/{}", b.branch);
    assert_eq!(removed["archiveRef"], archive.as_str());
    assert!(!b.path.exists());
    let tip = sh(&r.repo, &format!("git rev-parse {archive}"));
    assert_eq!(tip, sh(&r.repo, &format!("git rev-parse {}", b.branch)));
    assert_eq!(sh(&r.repo, &format!("git show {tip}:d.txt")), "draft");
}

#[tokio::test]
async fn a_folder_outside_git_is_a_model_visible_refusal() {
    let r = repo(false).await;
    let view = r.delegate("anything").await;
    assert_eq!(view["ok"], false, "{view}");
    assert_eq!(view["error"], "not_a_git_repository");
    let tasks = r
        .engine
        .task_list(&ThreadId::new("parent").unwrap())
        .await
        .unwrap();
    assert_eq!(tasks["tasks"].as_array().unwrap().len(), 0);
    assert!(r.rt.turns.lock().unwrap().is_empty());
}

#[tokio::test]
async fn project_setup_runs_on_create_and_on_settle() {
    let r = repo(true).await;
    std::fs::create_dir_all(r.repo.join(".zlo")).unwrap();
    std::fs::write(
        r.repo.join(".zlo/worktree.toml"),
        "on_create = [\"echo setup-ran in $(basename $PWD)\"]\non_settle = \"echo settled\"\n",
    )
    .unwrap();
    commit_all(&r.repo, "setup");
    let view = r.delegate("go").await;
    let child = child_of(&view);
    let b = r.engine.store().worktree(&child).await.unwrap().unwrap();
    let log = b.setup_log.unwrap();
    assert!(log.contains("setup-ran in"), "{log}");
    assert!(log.contains("[exit 0]"), "{log}");
    let (brief, _) = r.turn_of(&child);
    assert!(brief.contains("setup-ran"), "{brief}");

    r.finish(&child).await;
    let log = r
        .engine
        .store()
        .worktree(&child)
        .await
        .unwrap()
        .unwrap()
        .setup_log
        .unwrap();
    assert!(log.contains("[on_settle] $ echo settled"), "{log}");
}

#[tokio::test]
async fn shared_isolation_stays_the_default() {
    let r = repo(true).await;
    let view = r
        .engine
        .call_tool("t3_delegate", &ctx("parent"), &json!({"task": "look"}))
        .await;
    let child = child_of(&view);
    assert!(r.engine.store().worktree(&child).await.unwrap().is_none());
    assert_eq!(r.turn_of(&child).1, None);
    assert!(view.get("worktree").is_none());
}

#[tokio::test]
async fn a_user_thread_can_start_in_a_new_worktree() {
    let r = repo(true).await;
    let thread = r
        .engine
        .store()
        .thread(&ThreadId::new("parent").unwrap())
        .await
        .unwrap()
        .unwrap();
    let view = r
        .engine
        .start_worktree(&thread, Some(r.repo.clone()))
        .await
        .unwrap();
    assert_eq!(view["state"], "active");
    assert_eq!(view["exists"], true);
    let again = r.engine.start_worktree(&thread, None).await.unwrap_err();
    assert_eq!(again.code(), "worktree_exists");
    assert!(
        r.engine
            .deliver(
                &ThreadId::new("parent").unwrap(),
                "hello".into(),
                TurnOrigin::UserPrompt,
                false,
            )
            .await
            .unwrap()
    );
    let dir = r.turn_of(&ThreadId::new("parent").unwrap()).1;
    assert_eq!(
        dir.as_deref().map(|d| d.to_string_lossy().into_owned()),
        view["path"].as_str().map(str::to_string)
    );
}
