//! Worktree isolation over real temporary git repositories, driven through
//! the model-facing tools (`t3_delegate`, `t3_thread`) and turn events.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use serde_json::{Value, json};

use super::{
    HookApproval, HookApprover, HookDecision, HostCommand, HostCommandOutput, HostCommandRunner,
    WorktreeHost, WorktreeState,
};
use crate::delegation::Target;
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

/// A runtime that ignores `TurnInput::work_dir` (like the CLIs that do not
/// support worktrees yet), advertising provider `plain`.
struct PlainRuntime;

#[async_trait]
impl TurnRuntime for PlainRuntime {
    fn id(&self) -> RuntimeId {
        RuntimeId::new("plain").unwrap()
    }
    async fn catalog(&self, _scope: &ThreadScope) -> Result<Vec<CatalogProvider>, RuntimeError> {
        Ok(vec![CatalogProvider {
            provider: "plain".into(),
            name: "Plain".into(),
            authenticated: true,
            current: false,
            models: vec!["m".into()],
            efforts: vec![],
            model_efforts: Default::default(),
            brand: None,
        }])
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::default()
    }
    async fn create_thread(&self, _r: CreateThread) -> Result<CreatedThread, RuntimeError> {
        Ok(CreatedThread {
            thread_id: ThreadId::new(format!("plain-{}", now_ms())).unwrap(),
            native_session_id: None,
            provider: None,
            model: None,
        })
    }
    async fn start_turn(
        &self,
        _t: &ThreadRecord,
        _input: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        Ok(TurnAdmission::Started { run_id: None })
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

/// The person behind the approval gate: answers with `answer`, records
/// every request.
struct Approver {
    answer: Mutex<HookDecision>,
    asked: Mutex<Vec<HookApproval>>,
}

#[async_trait]
impl HookApprover for Approver {
    async fn approve(&self, request: &HookApproval) -> HookDecision {
        self.asked.lock().unwrap().push(request.clone());
        self.answer.lock().unwrap().clone()
    }
}

struct Repo {
    dir: tempfile::TempDir,
    engine: Arc<Engine>,
    rt: Arc<DirRuntime>,
    approver: Arc<Approver>,
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
/// it, and worktrees under a separate folder. Nobody approves setup hooks
/// unless a test says so.
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
    engine.register_runtime(Arc::new(PlainRuntime));
    let root = dir.path().join("worktrees");
    let approver = Arc::new(Approver {
        answer: Mutex::new(HookDecision::Unavailable),
        asked: Mutex::new(Vec::new()),
    });
    engine.set_worktree_host(WorktreeHost {
        runner: Arc::new(ShellRunner),
        root: root.clone(),
        approver: approver.clone(),
    });
    engine
        .ensure_thread(&ThreadId::new("parent").unwrap(), &scope(), None)
        .await
        .unwrap();
    Repo {
        dir,
        engine,
        rt,
        approver,
        repo,
        root,
    }
}

impl Repo {
    async fn delegate(&self, task: &str) -> Value {
        self.delegate_from(
            "parent",
            json!({"task": task, "title": "Fix the parser", "isolation": "worktree"}),
        )
        .await
    }

    async fn delegate_from(&self, thread: &str, args: Value) -> Value {
        self.engine
            .call_tool("t3_delegate", &ctx(thread), &args)
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

    async fn path_of(&self, thread: &ThreadId) -> PathBuf {
        self.engine
            .store()
            .worktree(thread)
            .await
            .unwrap()
            .unwrap()
            .path
    }

    async fn log_of(&self, thread: &ThreadId) -> String {
        self.engine
            .store()
            .worktree(thread)
            .await
            .unwrap()
            .unwrap()
            .setup_log
            .unwrap_or_default()
    }

    fn turn_of(&self, thread: &ThreadId) -> (String, Option<PathBuf>) {
        let turns = self.rt.turns.lock().unwrap();
        let (_, text, dir) = turns.iter().find(|(t, _, _)| t == thread).unwrap();
        (text.clone(), dir.clone())
    }

    /// A marker path outside the repository.
    fn marker(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

fn child_of(view: &Value) -> ThreadId {
    ThreadId::new(view["childThreadId"].as_str().unwrap()).unwrap()
}

fn write_hook(repo: &Path, hook: &str, marker: &Path) {
    let path = repo.join(".git/hooks").join(hook);
    std::fs::write(&path, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    sh(repo, &format!("chmod +x '{}'", path.display()));
}

fn in_progress(dir: &Path) -> bool {
    ["MERGE_HEAD", "CHERRY_PICK_HEAD", "sequencer"]
        .iter()
        .any(|name| {
            let p = sh(dir, &format!("git rev-parse --git-path {name}"));
            dir.join(p).exists()
        })
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
    // A project without setup hooks asks nobody.
    assert!(r.approver.asked.lock().unwrap().is_empty());

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

/// Finding 5: nothing is committed for anyone. A dirty worktree is not
/// merged until its owner commits; then the branch lands.
#[tokio::test]
async fn work_is_kept_merged_only_once_committed_and_cleaned_up() {
    let r = repo(true).await;
    let view = r.delegate("add b").await;
    let child = child_of(&view);
    let path = r.path_of(&child).await;
    std::fs::write(path.join("b.txt"), "from child\n").unwrap();
    commit_all(&path, "child adds b");
    std::fs::write(path.join(".env"), "TOKEN=secret\n").unwrap();

    r.finish(&child).await;
    let result = r
        .task(view["taskId"].as_str().unwrap())
        .await
        .result
        .unwrap();
    assert!(result.contains("Worktree kept"), "{result}");
    assert!(result.contains("merge_worktree"), "{result}");
    let status = r
        .thread_tool(json!({"action": "worktree_status", "thread": child.as_str()}))
        .await;
    assert_eq!(status["state"], "kept", "{status}");
    assert_eq!(status["aheadOfBase"], 1);
    assert_eq!(
        (status["ahead"].clone(), status["behind"].clone()),
        (json!(1), json!(0))
    );
    assert_eq!(status["dirtyCount"], 1);
    assert_eq!(status["onBranch"], true);

    let refused = r
        .thread_tool(json!({"action": "merge_worktree", "thread": child.as_str()}))
        .await;
    assert_eq!(refused["error"], "worktree_dirty", "{refused}");
    assert!(refused["message"].as_str().unwrap().contains(".env"));
    // Nothing was committed on the branch, nothing reached the checkout.
    assert_eq!(sh(&path, "git status --porcelain"), "?? .env");
    assert!(!r.repo.join("b.txt").exists());

    std::fs::remove_file(path.join(".env")).unwrap();
    let merged = r
        .thread_tool(json!({"action": "merge_worktree", "thread": child.as_str()}))
        .await;
    assert_eq!(merged["ok"], true, "{merged}");
    assert_eq!(merged["commits"], 1);
    assert_eq!(
        std::fs::read_to_string(r.repo.join("b.txt")).unwrap(),
        "from child\n"
    );
    assert!(!r.repo.join(".env").exists());
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
    let path = r.path_of(&child).await;
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
    assert!(!in_progress(&r.repo));
    assert_eq!(
        std::fs::read_to_string(r.repo.join("a.txt")).unwrap(),
        "parent\n"
    );
    let b = r.engine.store().worktree(&child).await.unwrap().unwrap();
    assert_eq!(b.state, WorktreeState::Kept);
}

/// Finding 5 (removal): a dirty worktree is never removed and nothing is
/// committed for it; committed unmerged work keeps its branch and an
/// archive ref.
#[tokio::test]
async fn removing_refuses_leftovers_and_keeps_unmerged_work() {
    let r = repo(true).await;
    let view = r.delegate("draft").await;
    let child = child_of(&view);
    let b = r.engine.store().worktree(&child).await.unwrap().unwrap();
    std::fs::write(b.path.join("d.txt"), "draft\n").unwrap();
    r.finish(&child).await;

    let refused = r
        .thread_tool(json!({"action": "remove_worktree", "thread": child.as_str()}))
        .await;
    assert_eq!(refused["error"], "worktree_dirty", "{refused}");
    assert!(b.path.join("d.txt").exists());
    assert_eq!(
        sh(&r.repo, &format!("git rev-list --count {}", b.branch)),
        "1"
    );

    commit_all(&b.path, "draft d");
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

/// Finding 2: a child that detaches HEAD and commits there keeps that
/// commit reachable when its worktree is removed; one that went back to
/// the base after committing on its branch is kept at settle, not removed.
#[tokio::test]
async fn a_head_off_its_branch_never_loses_commits() {
    let r = repo(true).await;
    let view = r.delegate("detached").await;
    let child = child_of(&view);
    let b = r.engine.store().worktree(&child).await.unwrap().unwrap();
    sh(&b.path, "git checkout -q --detach");
    std::fs::write(b.path.join("e.txt"), "detached work\n").unwrap();
    commit_all(&b.path, "detached commit");
    let tip = sh(&b.path, "git rev-parse HEAD");
    r.finish(&child).await;
    let status = r
        .thread_tool(json!({"action": "worktree_status", "thread": child.as_str()}))
        .await;
    assert_eq!(status["onBranch"], false, "{status}");

    // Merge takes only the branch, so it refuses rather than drop the commit.
    let merged = r
        .thread_tool(json!({"action": "merge_worktree", "thread": child.as_str()}))
        .await;
    assert_eq!(merged["error"], "worktree_head_moved", "{merged}");

    let removed = r
        .thread_tool(json!({"action": "remove_worktree", "thread": child.as_str()}))
        .await;
    assert_eq!(removed["state"], "removed", "{removed}");
    let archive = format!("{}-head", b.branch.archive_ref());
    assert_eq!(sh(&r.repo, &format!("git rev-parse {archive}")), tip);
    assert_eq!(
        sh(&r.repo, &format!("git show {tip}:e.txt")),
        "detached work"
    );

    // Committed on the branch, then back to the base: settle keeps it.
    let view = r.delegate("back to base").await;
    let child = child_of(&view);
    let b = r.engine.store().worktree(&child).await.unwrap().unwrap();
    std::fs::write(b.path.join("f.txt"), "on branch\n").unwrap();
    commit_all(&b.path, "branch commit");
    sh(
        &b.path,
        &format!("git checkout -q --detach {}", b.base_commit),
    );
    r.finish(&child).await;
    let after = r.engine.store().worktree(&child).await.unwrap().unwrap();
    assert_eq!(after.state, WorktreeState::Kept);
    assert!(b.path.exists());
    assert_eq!(
        sh(
            &r.repo,
            &format!("git rev-list --count {}..{}", b.base_commit, b.branch)
        ),
        "1"
    );
}

/// Finding 3: a squash never commits the checkout's own staged work.
#[tokio::test]
async fn a_dirty_target_is_refused_before_any_integration() {
    let r = repo(true).await;
    let view = r.delegate("squash me").await;
    let child = child_of(&view);
    let path = r.path_of(&child).await;
    std::fs::write(path.join("b.txt"), "child\n").unwrap();
    commit_all(&path, "child b");
    r.finish(&child).await;
    std::fs::write(r.repo.join("private.txt"), "mine\n").unwrap();
    sh(&r.repo, "git add private.txt");
    let head = sh(&r.repo, "git rev-parse HEAD");

    for strategy in ["squash", "merge", "cherry_pick"] {
        let out = r
            .thread_tool(
                json!({"action": "merge_worktree", "thread": child.as_str(), "strategy": strategy}),
            )
            .await;
        assert_eq!(out["error"], "target_dirty", "{strategy}: {out}");
        assert!(out["message"].as_str().unwrap().contains("private.txt"));
    }
    assert_eq!(sh(&r.repo, "git rev-parse HEAD"), head);
    assert_eq!(sh(&r.repo, "git status --porcelain"), "A  private.txt");
    assert!(!r.repo.join("b.txt").exists());
}

/// Finding 4: an integration that stops without conflicts (a cherry-pick
/// that becomes empty) is rolled back and verified, not left half done.
#[tokio::test]
async fn an_unfinished_cherry_pick_is_rolled_back() {
    let r = repo(true).await;
    let view = r.delegate("pick").await;
    let child = child_of(&view);
    let path = r.path_of(&child).await;
    std::fs::write(path.join("b.txt"), "child\n").unwrap();
    commit_all(&path, "child b");
    sh(&path, &format!("{GIT} commit -q --allow-empty -m empty"));
    r.finish(&child).await;
    let head = sh(&r.repo, "git rev-parse HEAD");

    let out = r
        .thread_tool(json!({"action": "merge_worktree", "thread": child.as_str(), "strategy": "cherry_pick"}))
        .await;
    assert_eq!(out["error"], "merge_refused", "{out}");
    assert!(
        out["message"].as_str().unwrap().contains("rolled back"),
        "{out}"
    );
    assert_eq!(sh(&r.repo, "git rev-parse HEAD"), head);
    assert!(!in_progress(&r.repo));
    assert_eq!(sh(&r.repo, "git status --porcelain"), "");
    assert!(!r.repo.join("b.txt").exists());
    let b = r.engine.store().worktree(&child).await.unwrap().unwrap();
    assert_eq!(b.state, WorktreeState::Kept);
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

fn setup_file(repo: &Path, body: &str) {
    std::fs::create_dir_all(repo.join(".zlo")).unwrap();
    std::fs::write(repo.join(".zlo/worktree.toml"), body).unwrap();
}

/// Finding 1: a repository's setup hooks run only after the person
/// approved their exact content; settle runs the approved snapshot, never
/// the child's edited copy; changed content asks again.
#[tokio::test]
async fn setup_hooks_run_only_once_approved_and_only_as_approved() {
    let r = repo(true).await;
    let created = r.marker("created");
    setup_file(
        &r.repo,
        &format!(
            "on_create = [\"touch '{}'\", \"echo setup-ran in $(basename $PWD)\"]\non_settle = \"echo settled\"\n",
            created.display()
        ),
    );
    commit_all(&r.repo, "setup");

    // Nobody can approve: nothing runs, and the log says why.
    let view = r.delegate("go").await;
    let child = child_of(&view);
    assert!(!created.exists(), "unapproved on_create must not run");
    let asked = r.approver.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].thread.as_str(), "parent");
    assert!(asked[0].script.contains("touch"), "{}", asked[0].script);
    assert!(r.log_of(&child).await.contains("not run"));
    r.finish(&child).await;
    assert!(!r.log_of(&child).await.contains("[on_settle] $"));

    // Denied: still nothing.
    *r.approver.answer.lock().unwrap() = HookDecision::Denied("no".into());
    let child = child_of(&r.delegate("go again").await);
    assert!(!created.exists());
    assert!(r.log_of(&child).await.contains("declined: no"));
    r.finish(&child).await;

    // Approved: runs, and the same content is not asked again.
    *r.approver.answer.lock().unwrap() = HookDecision::Approved;
    let child = child_of(&r.delegate("go approved").await);
    assert!(created.exists());
    assert!(r.log_of(&child).await.contains("setup-ran in"));
    let (brief, _) = r.turn_of(&child);
    assert!(brief.contains("setup-ran"), "{brief}");
    let path = r.path_of(&child).await;
    // The child rewrites the hooks in its tree; settle ignores that copy.
    let pwned = r.marker("pwned");
    setup_file(
        &path,
        &format!("on_settle = \"touch '{}'\"\n", pwned.display()),
    );
    commit_all(&path, "child edits hooks");
    r.finish(&child).await;
    assert!(!pwned.exists(), "settle must run the approved snapshot");
    assert!(
        r.log_of(&child)
            .await
            .contains("[on_settle] $ echo settled")
    );
    let asked_before = r.approver.asked.lock().unwrap().len();
    *r.approver.answer.lock().unwrap() = HookDecision::Unavailable;
    let child = child_of(&r.delegate("trusted now").await);
    assert_eq!(r.approver.asked.lock().unwrap().len(), asked_before);
    assert!(r.log_of(&child).await.contains("setup-ran in"));
    r.finish(&child).await;

    // Changed content asks again.
    std::fs::remove_file(&created).unwrap();
    setup_file(
        &r.repo,
        &format!("on_create = \"touch '{}' && echo v2\"\n", created.display()),
    );
    commit_all(&r.repo, "setup v2");
    let child = child_of(&r.delegate("changed").await);
    assert_eq!(r.approver.asked.lock().unwrap().len(), asked_before + 1);
    assert!(!created.exists());
    r.finish(&child).await;
}

/// Finding 1 (git hooks): orchestration git never runs the repository's
/// hooks — not on `worktree add`, not on merge.
#[tokio::test]
async fn git_hooks_never_run_during_orchestration() {
    let r = repo(true).await;
    let checkout = r.marker("post-checkout");
    let merge = r.marker("post-merge");
    let commit = r.marker("post-commit");
    write_hook(&r.repo, "post-checkout", &checkout);
    write_hook(&r.repo, "post-merge", &merge);
    write_hook(&r.repo, "post-commit", &commit);
    let child = child_of(&r.delegate("hooks").await);
    assert!(!checkout.exists(), "post-checkout ran on worktree add");
    let path = r.path_of(&child).await;
    std::fs::write(path.join("b.txt"), "b\n").unwrap();
    sh(
        &path,
        &format!("git add -A && {GIT} -c core.hooksPath=/dev/null commit -q -m b"),
    );
    r.finish(&child).await;
    for strategy in ["merge", "squash"] {
        if strategy == "squash" {
            let child = child_of(&r.delegate("hooks squash").await);
            let path = r.path_of(&child).await;
            std::fs::write(path.join("c.txt"), "c\n").unwrap();
            sh(
                &path,
                &format!("git add -A && {GIT} -c core.hooksPath=/dev/null commit -q -m c"),
            );
            r.finish(&child).await;
            let out = r
                .thread_tool(json!({"action": "merge_worktree", "thread": child.as_str(), "strategy": strategy}))
                .await;
            assert_eq!(out["ok"], true, "{out}");
        } else {
            let out = r
                .thread_tool(json!({"action": "merge_worktree", "thread": child.as_str(), "strategy": strategy}))
                .await;
            assert_eq!(out["ok"], true, "{out}");
        }
    }
    assert!(!merge.exists(), "post-merge ran");
    assert!(!commit.exists(), "post-commit ran");
    assert!(!checkout.exists());
}

/// Finding 6: a lifecycle step holds the worktree against turn admission,
/// and two concurrent starts leave exactly one binding.
#[tokio::test]
async fn lifecycle_steps_exclude_turns_and_each_other() {
    let r = repo(true).await;
    let parent = ThreadId::new("parent").unwrap();
    let thread = r.engine.store().thread(&parent).await.unwrap().unwrap();
    let (a, b) = tokio::join!(
        r.engine.start_worktree(&thread, Some(r.repo.clone())),
        r.engine.start_worktree(&thread, Some(r.repo.clone())),
    );
    assert_eq!(
        usize::from(a.is_ok()) + usize::from(b.is_ok()),
        1,
        "{a:?} / {b:?}"
    );
    let live = r.engine.store().live_worktree_branches().await.unwrap();
    assert_eq!(live.len(), 1);
    assert_eq!(
        sh(
            &r.repo,
            "git worktree list --porcelain | grep -c '^worktree '"
        ),
        "2"
    );

    let op = r.engine.reserve_worktree(&parent).await.unwrap();
    assert!(
        !r.engine
            .deliver(&parent, "blocked".into(), TurnOrigin::UserPrompt, false)
            .await
            .unwrap(),
        "no turn starts while a lifecycle step holds the worktree"
    );
    let second = r.engine.reserve_worktree(&parent).await;
    assert_eq!(second.err().map(|e| e.code()), Some("thread_busy"));
    drop(op);
    assert!(
        r.engine
            .deliver(&parent, "now".into(), TurnOrigin::UserPrompt, false)
            .await
            .unwrap()
    );
    // A running thread cannot be merged or removed under it.
    let refused = r.thread_tool(json!({"action": "remove_worktree"})).await;
    assert_eq!(refused["error"], "thread_busy", "{refused}");
}

/// Finding 8: a thread in a worktree cannot switch to a runtime that
/// ignores worktrees, and such a child is never started from it.
#[tokio::test]
async fn a_worktree_thread_stays_on_runtimes_that_honor_it() {
    let r = repo(true).await;
    let parent = ThreadId::new("parent").unwrap();
    let thread = r.engine.store().thread(&parent).await.unwrap().unwrap();
    r.engine
        .start_worktree(&thread, Some(r.repo.clone()))
        .await
        .unwrap();
    let switched = r
        .engine
        .set_route(
            &thread,
            &Target {
                provider: Some("plain".into()),
                ..Target::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(switched.code(), "worktree_unavailable");
    let shared = r
        .delegate_from(
            "parent",
            json!({"task": "elsewhere", "target": {"provider": "plain"}}),
        )
        .await;
    assert_eq!(shared["error"], "worktree_unavailable", "{shared}");
    let isolated = r
        .delegate_from(
            "parent",
            json!({"task": "elsewhere", "isolation": "worktree", "target": {"provider": "plain"}}),
        )
        .await;
    assert_eq!(isolated["error"], "worktree_unavailable", "{isolated}");
}

/// Finding 9: a default (shared) child of a worktree thread works in that
/// worktree, never in the original checkout, and does not own its cleanup.
#[tokio::test]
async fn a_shared_child_of_a_worktree_thread_works_in_that_worktree() {
    let r = repo(true).await;
    let child = child_of(&r.delegate("isolated").await);
    let path = r.path_of(&child).await;
    let grandchild = child_of(
        &r.delegate_from(child.as_str(), json!({"task": "help in the same tree"}))
            .await,
    );
    assert_eq!(r.turn_of(&grandchild).1.as_deref(), Some(path.as_path()));
    assert!(
        r.engine
            .store()
            .worktree(&grandchild)
            .await
            .unwrap()
            .is_none()
    );
    // The grandchild still runs: its parent's worktree is not settled under it.
    r.finish(&child).await;
    let b = r.engine.store().worktree(&child).await.unwrap().unwrap();
    assert_eq!(b.state, WorktreeState::Active);
    assert!(path.exists());
    r.finish(&grandchild).await;
    assert!(
        r.engine
            .store()
            .worktree(&grandchild)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn shared_isolation_stays_the_default() {
    let r = repo(true).await;
    let view = r.delegate_from("parent", json!({"task": "look"})).await;
    let child = child_of(&view);
    assert!(r.engine.store().worktree(&child).await.unwrap().is_none());
    assert_eq!(r.turn_of(&child).1, None);
    assert!(view.get("worktree").is_none());
}

#[tokio::test]
async fn a_user_thread_can_start_in_a_new_worktree() {
    let r = repo(true).await;
    let parent = ThreadId::new("parent").unwrap();
    let thread = r.engine.store().thread(&parent).await.unwrap().unwrap();
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
            .deliver(&parent, "hello".into(), TurnOrigin::UserPrompt, false)
            .await
            .unwrap()
    );
    let dir = r.turn_of(&parent).1;
    assert_eq!(
        dir.as_deref().map(|d| d.to_string_lossy().into_owned()),
        view["path"].as_str().map(str::to_string)
    );
}
