//! A thread's repository and console for a remote client, over real
//! temporary git repositories, driven through the engine's public calls
//! (`thread_git`, `thread_git_change`, `thread_console*`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use ironclaw_product_contracts::thread_repo::*;
use serde_json::Value;

use crate::engine::Engine;
use crate::error::OrchestratorError;
use crate::model::*;
use crate::runtime::*;
use crate::store::Store;
use crate::worktree::{
    HookApproval, HookApprover, HookDecision, HostCommand, HostCommandEvent, HostCommandOutput,
    HostCommandProcess, HostCommandRunner, WorktreeHost,
};

/// `sh -c` in its own process group, like the host process port: streamed
/// output, kill ends the group.
struct ShellRunner;

struct ShellProcess {
    child: tokio::process::Child,
    pid: Option<u32>,
    rx: tokio::sync::mpsc::Receiver<HostCommandEvent>,
    done: bool,
}

#[async_trait]
impl HostCommandProcess for ShellProcess {
    async fn next(&mut self) -> Option<HostCommandEvent> {
        if self.done {
            return None;
        }
        if let Some(event) = self.rx.recv().await {
            return Some(event);
        }
        self.done = true;
        let code = self.child.wait().await.ok().and_then(|s| s.code());
        self.pid = None;
        Some(HostCommandEvent::Exited {
            code,
            timed_out: false,
        })
    }
    fn kill(&mut self) {
        if let Some(pid) = self.pid {
            unsafe {
                libc_kill(-(pid as i32), 9);
            }
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for ShellProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

unsafe extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

#[async_trait]
impl HostCommandRunner for ShellRunner {
    async fn run(&self, c: HostCommand) -> Result<HostCommandOutput, OrchestratorError> {
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(format!("({}) 2>&1", c.line))
            .current_dir(&c.workdir)
            .output()
            .await
            .map_err(|e| OrchestratorError::request("command_failed", e.to_string()))?;
        Ok(HostCommandOutput {
            exit_code: i64::from(out.status.code().unwrap_or(-1)),
            output: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
    }

    async fn spawn(
        &self,
        c: HostCommand,
    ) -> Result<Box<dyn HostCommandProcess>, OrchestratorError> {
        use tokio::io::AsyncReadExt;
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&c.line)
            .current_dir(&c.workdir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| OrchestratorError::request("command_failed", e.to_string()))?;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let mut out = child.stdout.take().unwrap();
        let mut err = child.stderr.take().unwrap();
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            while let Ok(n) = out.read(&mut buf).await {
                if n == 0
                    || tx
                        .send(HostCommandEvent::Stdout(buf[..n].to_vec()))
                        .await
                        .is_err()
                {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            let mut buf = vec![0u8; 8192];
            while let Ok(n) = err.read(&mut buf).await {
                if n == 0
                    || tx2
                        .send(HostCommandEvent::Stderr(buf[..n].to_vec()))
                        .await
                        .is_err()
                {
                    break;
                }
            }
        });
        Ok(Box::new(ShellProcess {
            pid: child.id(),
            child,
            rx,
            done: false,
        }))
    }
}

struct NoApprover;

#[async_trait]
impl HookApprover for NoApprover {
    async fn approve(&self, _r: &HookApproval) -> HookDecision {
        HookDecision::Unavailable
    }
}

/// A runtime whose agents work in `home`.
struct HomeRuntime {
    home: PathBuf,
}

#[async_trait]
impl TurnRuntime for HomeRuntime {
    fn id(&self) -> RuntimeId {
        RuntimeId::new("fake").unwrap()
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::default()
    }
    fn default_work_dir(&self) -> Option<PathBuf> {
        Some(self.home.clone())
    }
    async fn create_thread(&self, _r: CreateThread) -> Result<CreatedThread, RuntimeError> {
        Err(RuntimeError::Unsupported("create"))
    }
    async fn start_turn(
        &self,
        _t: &ThreadRecord,
        _input: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        Err(RuntimeError::Unsupported("turn"))
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

fn scope() -> ThreadScope {
    ThreadScope {
        tenant_id: TenantId::new("t1").unwrap(),
        user_id: UserId::new("u1").unwrap(),
        agent_id: None,
        project_id: None,
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    engine: Arc<Engine>,
    repo: PathBuf,
    thread: ThreadRecord,
}

/// A repository with one commit; the thread works in it. The console is
/// on when `console`.
async fn fixture(console: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("project");
    std::fs::create_dir_all(&repo).unwrap();
    sh(&repo, "git init -q -b main");
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    sh(&repo, &format!("git add -A && {GIT} commit -q -m first"));
    let repo = std::fs::canonicalize(&repo).unwrap();
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
    engine.register_runtime(Arc::new(HomeRuntime { home: repo.clone() }));
    engine.set_worktree_host(WorktreeHost {
        runner: Arc::new(ShellRunner),
        root: dir.path().join("worktrees"),
        approver: Arc::new(NoApprover),
    });
    if console {
        engine.set_console_runner(Arc::new(ShellRunner));
    }
    let thread = engine
        .ensure_thread(&ThreadId::new("chat").unwrap(), &scope(), None)
        .await
        .unwrap();
    Fixture {
        _dir: dir,
        engine,
        repo,
        thread,
    }
}

impl Fixture {
    async fn read(&self, read: ThreadGitRead) -> Value {
        self.engine.thread_git(&self.thread, read).await.unwrap()
    }
    async fn change(&self, action: ThreadGitAction) -> Result<Value, OrchestratorError> {
        self.engine.thread_git_change(&self.thread, action).await
    }
    async fn status(&self) -> GitStatusView {
        serde_json::from_value(self.read(ThreadGitRead::Status {}).await).unwrap()
    }
}

#[tokio::test]
async fn status_diff_stage_and_unstage_read_back() {
    let f = fixture(false).await;
    std::fs::write(f.repo.join("a.txt"), "two\n").unwrap();
    std::fs::write(f.repo.join("new.txt"), "fresh\n").unwrap();
    let status = f.status().await;
    assert_eq!(status.repo_root, f.repo.to_string_lossy());
    assert_eq!(status.branch.name, "main");
    let mut paths: Vec<_> = status
        .changes
        .iter()
        .map(|c| (c.path.as_str(), c.index, c.worktree))
        .collect();
    paths.sort();
    assert_eq!(paths, vec![("a.txt", ' ', 'M'), ("new.txt", '?', '?')]);

    let working: GitPatchView = serde_json::from_value(
        f.read(ThreadGitRead::Diff {
            scope: GitDiffScope::Working,
            path: None,
        })
        .await,
    )
    .unwrap();
    assert!(working.patch.contains("-one\n+two"), "{}", working.patch);
    assert!(
        working.patch.contains("+fresh"),
        "new files are patches too"
    );
    assert!(!working.truncated);

    // Stage → the read-back status shows it staged.
    let staged: GitChangeResult = serde_json::from_value(
        f.change(ThreadGitAction::Stage {
            paths: vec!["a.txt".into()],
        })
        .await
        .unwrap(),
    )
    .unwrap();
    let a = staged
        .status
        .changes
        .iter()
        .find(|c| c.path == "a.txt")
        .unwrap();
    assert_eq!((a.index, a.worktree), ('M', ' '));
    let cached: GitPatchView = serde_json::from_value(
        f.read(ThreadGitRead::Diff {
            scope: GitDiffScope::Staged,
            path: Some("a.txt".into()),
        })
        .await,
    )
    .unwrap();
    assert!(cached.patch.contains("+two"));
    assert_eq!(sh(&f.repo, "git diff --cached --name-only"), "a.txt");

    let unstaged: GitChangeResult = serde_json::from_value(
        f.change(ThreadGitAction::Unstage {
            paths: vec!["a.txt".into()],
        })
        .await
        .unwrap(),
    )
    .unwrap();
    let a = unstaged
        .status
        .changes
        .iter()
        .find(|c| c.path == "a.txt")
        .unwrap();
    assert_eq!((a.index, a.worktree), (' ', 'M'));

    // Discard: a modified file comes back, a new file is deleted.
    for path in ["a.txt", "new.txt"] {
        f.change(ThreadGitAction::Discard { path: path.into() })
            .await
            .unwrap();
    }
    assert!(f.status().await.changes.is_empty());
    assert_eq!(
        std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(!f.repo.join("new.txt").exists());
}

#[tokio::test]
async fn paths_outside_the_thread_repository_are_refused_before_git_runs() {
    let f = fixture(false).await;
    let outside = f.repo.parent().unwrap().join("secret.txt");
    std::fs::write(&outside, "keep\n").unwrap();
    for path in [
        "../secret.txt",
        outside.to_str().unwrap(),
        "a/../../secret.txt",
    ] {
        for action in [
            ThreadGitAction::Discard { path: path.into() },
            ThreadGitAction::Stage {
                paths: vec![path.into()],
            },
        ] {
            let err = f.change(action).await.unwrap_err();
            assert_eq!(err.code(), "path_outside_repository", "{path}");
        }
        let err = f
            .engine
            .thread_git(
                &f.thread,
                ThreadGitRead::Diff {
                    scope: GitDiffScope::Working,
                    path: Some(path.into()),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), "path_outside_repository");
    }
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep\n");
    // A symlink out of the repository is not followed by discard.
    std::os::unix::fs::symlink(&outside, f.repo.join("link")).unwrap();
    f.change(ThreadGitAction::Discard {
        path: "link".into(),
    })
    .await
    .unwrap();
    assert!(!f.repo.join("link").exists());
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep\n");
}

#[tokio::test]
async fn hooks_and_fsmonitor_never_run() {
    let f = fixture(false).await;
    let marker = f.repo.parent().unwrap().join("marker");
    let hook = format!("#!/bin/sh\necho ran >> '{}'\n", marker.display());
    let hooks = f.repo.join(".git/hooks");
    for name in [
        "post-index-change",
        "reference-transaction",
        "post-checkout",
    ] {
        let path = hooks.join(name);
        std::fs::write(&path, &hook).unwrap();
        sh(&f.repo, &format!("chmod +x '{}'", path.display()));
    }
    let monitor = f.repo.parent().unwrap().join("fsmonitor.sh");
    std::fs::write(&monitor, &hook).unwrap();
    sh(&f.repo, &format!("chmod +x '{}'", monitor.display()));
    sh(
        &f.repo,
        &format!("git config core.fsmonitor '{}'", monitor.display()),
    );
    // The hook does run for plain git: the fixture is armed.
    sh(
        &f.repo,
        "touch armed && git add armed && git reset -q armed",
    );
    assert!(marker.exists(), "plain git runs the hook");
    std::fs::remove_file(&marker).unwrap();

    std::fs::write(f.repo.join("a.txt"), "two\n").unwrap();
    f.status().await;
    f.change(ThreadGitAction::Stage {
        paths: vec!["a.txt".into()],
    })
    .await
    .unwrap();
    f.change(ThreadGitAction::Checkpoint {}).await.unwrap();
    f.change(ThreadGitAction::Discard {
        path: "a.txt".into(),
    })
    .await
    .unwrap();
    assert!(!marker.exists(), "a hook or fsmonitor ran");
}

#[tokio::test]
async fn oversized_diffs_are_capped_on_a_line() {
    let f = fixture(false).await;
    let big = "x".repeat(99) + "\n";
    std::fs::write(
        f.repo.join("a.txt"),
        big.repeat(GIT_PATCH_MAX_BYTES / 100 + 2_000),
    )
    .unwrap();
    let view: GitPatchView = serde_json::from_value(
        f.read(ThreadGitRead::Diff {
            scope: GitDiffScope::Working,
            path: None,
        })
        .await,
    )
    .unwrap();
    assert!(view.truncated);
    assert!(view.patch.len() <= GIT_PATCH_MAX_BYTES);
    assert!(view.patch.ends_with('\n'));
}

#[tokio::test]
async fn checkpoint_turn_diff_undo_and_history() {
    let f = fixture(false).await;
    std::fs::write(f.repo.join("before.txt"), "untracked before\n").unwrap();
    let cp: GitChangeResult =
        serde_json::from_value(f.change(ThreadGitAction::Checkpoint {}).await.unwrap()).unwrap();
    assert!(cp.status.checkpoint);
    // The "turn": edits a file and creates one.
    std::fs::write(f.repo.join("a.txt"), "turn\n").unwrap();
    std::fs::write(f.repo.join("made.txt"), "by the turn\n").unwrap();
    // The turn also stages the file that was untracked before it.
    sh(&f.repo, "git add before.txt");
    let turn: GitPatchView = serde_json::from_value(
        f.read(ThreadGitRead::Diff {
            scope: GitDiffScope::Turn,
            path: None,
        })
        .await,
    )
    .unwrap();
    assert!(turn.patch.contains("+turn"));
    assert!(turn.patch.contains("made.txt"));
    let undone: GitChangeResult =
        serde_json::from_value(f.change(ThreadGitAction::UndoTurn {}).await.unwrap()).unwrap();
    assert_eq!(undone.note.as_deref(), Some("Restored 2 files."));
    assert_eq!(
        std::fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "one\n"
    );
    assert!(!f.repo.join("made.txt").exists());
    // Staged during the turn, but untracked before it: kept, with its bytes.
    assert_eq!(
        std::fs::read_to_string(f.repo.join("before.txt")).unwrap(),
        "untracked before\n"
    );
    let kept: GitChangeResult =
        serde_json::from_value(f.change(ThreadGitAction::KeepTurn {}).await.unwrap()).unwrap();
    assert!(!kept.status.checkpoint);

    sh(
        &f.repo,
        &format!("git add -A && {GIT} commit -q -m 'second one'"),
    );
    let log: GitLogView =
        serde_json::from_value(f.read(ThreadGitRead::Log { limit: Some(10) }).await).unwrap();
    assert_eq!(log.commits.len(), 2);
    assert_eq!(log.commits[0].subject, "second one");
    let commit: GitCommitView = serde_json::from_value(
        f.read(ThreadGitRead::Commit {
            sha: log.commits[0].sha.clone(),
        })
        .await,
    )
    .unwrap();
    assert_eq!(commit.message, "second one");
    assert!(commit.patch.contains("before.txt"));
    let bad = f
        .engine
        .thread_git(
            &f.thread,
            ThreadGitRead::Commit {
                sha: "--output=/tmp/x".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(bad.is_correctable());
}

#[tokio::test]
async fn a_thread_without_a_repository_says_so() {
    let f = fixture(false).await;
    sh(&f.repo, "rm -rf .git");
    let err = f
        .engine
        .thread_git(&f.thread, ThreadGitRead::Status {})
        .await
        .unwrap_err();
    assert_eq!(err.code(), "not_a_git_repository");
}

async fn console_until(
    f: &Fixture,
    done: impl Fn(&ThreadConsoleView) -> bool,
) -> ThreadConsoleView {
    for _ in 0..200 {
        let view = f.engine.thread_console(&f.thread, None).await.unwrap();
        if done(&view) {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("console never got there");
}

#[tokio::test]
async fn console_runs_one_command_streams_it_and_cancel_kills_its_tree() {
    let f = fixture(true).await;
    let start = |command: &str| ThreadConsoleAction::Start {
        command: command.into(),
    };
    let view = f
        .engine
        .thread_console_control(&f.thread, start("pwd; echo oops >&2; exit 4"))
        .await
        .unwrap();
    assert!(view.available);
    assert_eq!(view.folder.as_deref(), Some(f.repo.to_str().unwrap()));
    let done = console_until(&f, |v| !v.running).await;
    let texts: Vec<_> = done
        .lines
        .iter()
        .map(|l| (l.kind, l.text.as_str()))
        .collect();
    assert_eq!(
        texts[0],
        (ConsoleLineKind::Command, "pwd; echo oops >&2; exit 4")
    );
    assert!(texts.contains(&(ConsoleLineKind::Out, f.repo.to_str().unwrap())));
    assert!(texts.contains(&(ConsoleLineKind::Err, "oops")));
    assert_eq!(done.exit_code, Some(4));
    // The cursor returns only newer lines.
    let after = f
        .engine
        .thread_console(&f.thread, Some(done.next))
        .await
        .unwrap();
    assert!(after.lines.is_empty());

    // One command at a time; cancel kills what it started.
    let pid_file = f.repo.join("child.pid");
    f.engine
        .thread_console_control(
            &f.thread,
            start(&format!(
                "sleep 60 & echo $! > '{}'; echo started; wait",
                pid_file.display()
            )),
        )
        .await
        .unwrap();
    console_until(&f, |v| v.lines.iter().any(|l| l.text == "started")).await;
    let busy = f
        .engine
        .thread_console_control(&f.thread, start("echo no"))
        .await
        .unwrap_err();
    assert_eq!(busy.code(), "console_busy");
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_string();
    f.engine
        .thread_console_control(&f.thread, ThreadConsoleAction::Cancel {})
        .await
        .unwrap();
    let stopped = console_until(&f, |v| !v.running).await;
    assert_eq!(stopped.lines.last().unwrap().text, "Stopped.");
    assert_eq!(stopped.exit_code, None);
    let mut gone = false;
    for _ in 0..50 {
        let state = sh(&f.repo, &format!("ps -o stat= -p {pid} || true"));
        if state.is_empty() || state.starts_with('Z') {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(gone, "the background child survived cancel");
}

#[tokio::test]
async fn without_a_console_runner_the_console_is_unavailable() {
    let f = fixture(false).await;
    let view = f.engine.thread_console(&f.thread, None).await.unwrap();
    assert!(!view.available);
    let err = f
        .engine
        .thread_console_control(
            &f.thread,
            ThreadConsoleAction::Start {
                command: "echo hi".into(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "console_unavailable");
    assert!(err.is_correctable());
}

#[tokio::test]
async fn shutdown_stops_running_console_commands() {
    let f = fixture(true).await;
    f.engine
        .thread_console_control(
            &f.thread,
            ThreadConsoleAction::Start {
                command: "sleep 60".into(),
            },
        )
        .await
        .unwrap();
    console_until(&f, |v| v.running).await;
    f.engine.prepare_for_shutdown().await.unwrap();
    let stopped = console_until(&f, |v| !v.running).await;
    assert_eq!(stopped.lines.last().unwrap().text, "Stopped.");
}
