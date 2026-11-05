//! The Codex driver's use of the app-server protocol beyond a bare turn,
//! against a fake `codex app-server` that replays recorded JSON-RPC (shapes
//! from `codex app-server generate-ts`, codex-cli 0.166.0):
//!
//! - live turn detail on the bridge: reasoning summaries as thinking, reply
//!   text, the command row with its streaming output tail, the turn diff;
//! - `review/start` as a turn whose findings are the answer;
//! - `thread/revert` before the native turn of the run a client names, in
//!   the session's own workspace only, with the journal keeping the reverted
//!   rows (hidden, never deleted) and the intent recorded first;
//! - one holder per session: a prompt or action during a revert is `Busy`;
//! - `thread/compact/start`.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_cli_providers::bridge::{CliBridge, DEFAULT_APPROVAL_TIMEOUT, LiveEvent, ToolState};
use ironclaw_cli_providers::codex::{CodexConfig, CodexRuntime};
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use ironclaw_orchestrator::model::{Millis, RuntimeId, ThreadRecord, ThreadScope};
use ironclaw_orchestrator::runtime::*;
use ironclaw_product_contracts::orchestration::{
    ReviewTarget, ThreadAction, ThreadActionOutcome, ThreadActionRefusal,
};
use tokio::sync::broadcast;

#[derive(Default)]
struct Recorder {
    finished: Mutex<Vec<TurnOutcome>>,
}

#[async_trait]
impl TurnEventSink for Recorder {
    async fn turn_started(&self, _: &ThreadId, _: Option<String>, _: Millis) {}
    async fn turn_finished(
        &self,
        _: &ThreadId,
        _: Option<String>,
        outcome: TurnOutcome,
        _: Millis,
    ) {
        self.finished.lock().unwrap().push(outcome);
    }
}

/// Recorded app-server traffic. A turn streams a reasoning summary, runs
/// `cargo test` (two output chunks, exit 101), edits a file (turn diff) and
/// answers; a review answers with findings; revert and compaction reply as
/// 0.166.0 does.
const CODEX: &str = r#"#!/bin/sh
LOG=__LOG__
id_of() { printf '%s' "$1" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p'; }
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
emit() { printf '{"jsonrpc":"2.0","method":"%s","params":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$LOG"
  id=$(id_of "$line")
  case "$line" in
    *'"method":"initialize"'*) reply "$id" '{"userAgent":"codex/0.166.0"}' ;;
    *'"method":"initialized"'*) ;;
    *'"method":"thread/start"'*|*'"method":"thread/resume"'*) reply "$id" '{"thread":{"id":"cx-1"},"model":"gpt-5.5"}' ;;
    *'"method":"turn/start"'*)
      reply "$id" '{"turn":{"id":"tu-1","status":"inProgress"}}'
      emit item/reasoning/summaryTextDelta '{"threadId":"cx-1","turnId":"tu-1","itemId":"r1","summaryIndex":0,"delta":"**Running the tests**"}'
      emit item/started '{"threadId":"cx-1","turnId":"tu-1","item":{"type":"commandExecution","id":"c1","command":"cargo test","cwd":"/w","status":"inProgress","commandActions":[],"aggregatedOutput":null,"exitCode":null,"durationMs":null,"processId":null,"source":"agent","pluginId":null,"scriptPath":null}}'
      emit item/commandExecution/outputDelta '{"threadId":"cx-1","turnId":"tu-1","itemId":"c1","delta":"running 3 tests\n"}'
      sleep 0.6
      emit item/commandExecution/outputDelta '{"threadId":"cx-1","turnId":"tu-1","itemId":"c1","delta":"test a ... FAILED\n"}'
      emit item/completed '{"threadId":"cx-1","turnId":"tu-1","item":{"type":"commandExecution","id":"c1","command":"cargo test","cwd":"/w","status":"completed","commandActions":[],"aggregatedOutput":"running 3 tests\ntest a ... FAILED\n","exitCode":101,"durationMs":900,"processId":null,"source":"agent","pluginId":null,"scriptPath":null}}'
      emit turn/diff/updated '{"threadId":"cx-1","turnId":"tu-1","diff":"diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-a\n+b\n"}'
      emit item/agentMessage/delta '{"threadId":"cx-1","turnId":"tu-1","itemId":"m1","delta":"Fixed it."}'
      emit item/completed '{"threadId":"cx-1","turnId":"tu-1","item":{"type":"agentMessage","id":"m1","text":"Fixed it.","phase":null,"memoryCitation":null,"delivery":null,"questions":null}}'
      emit turn/completed '{"threadId":"cx-1","turn":{"id":"tu-1","status":"completed","error":null}}' ;;
    *'"instructions":"refuse"'*) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32600,"message":"no"}}\n' "$id" ;;
    *'"method":"review/start"'*)
      reply "$id" '{"turn":{"id":"tu-r","status":"inProgress"},"reviewThreadId":"cx-1"}'
      emit item/completed '{"threadId":"cx-1","turnId":"tu-r","item":{"type":"enteredReviewMode","id":"e1","review":"current changes"}}'
      emit item/completed '{"threadId":"cx-1","turnId":"tu-r","item":{"type":"exitedReviewMode","id":"x1","review":"One issue.\n\nReview comment:\n\n- [P1] Off by one — /w/src/a.rs:3-4\n  The loop skips the last element."}}'
      emit turn/completed '{"threadId":"cx-1","turn":{"id":"tu-r","status":"completed","error":null}}' ;;
    *'"method":"thread/turns/list"'*) reply "$id" '{"data":[{"id":"tu-r","status":"completed"}],"nextCursor":null,"backwardsCursor":null}' ;;
    *'"method":"thread/revert"'*)
      while [ -n "$GATE" ] && [ ! -f "$GATE" ]; do sleep 0.05; done
      reply "$id" '{"thread":{"id":"cx-1"},"turnsBackwardsCursor":null,"itemsBackwardsCursor":null}' ;;
    *'"method":"thread/compact/start"'*)
      reply "$id" '{}'
      emit item/completed '{"threadId":"cx-1","turnId":"tu-c","item":{"type":"contextCompaction","id":"cc"}}' ;;
  esac
done
"#;

fn runtime(dir: &Path, recorder: &Arc<Recorder>, bridge: Arc<CliBridge>) -> CodexRuntime {
    gated_runtime(dir, recorder, bridge, None)
}

/// With `gate`, `thread/revert` answers only once that file exists.
fn gated_runtime(
    dir: &Path,
    recorder: &Arc<Recorder>,
    bridge: Arc<CliBridge>,
    gate: Option<&Path>,
) -> CodexRuntime {
    let gate = gate
        .map(|g| format!("GATE='{}'", g.display()))
        .unwrap_or_default();
    let body = CODEX
        .replace("__LOG__", &dir.join("codex.log").display().to_string())
        .replacen("LOG=", &format!("{gate}\nLOG="), 1);
    // Codex works in its own repository (`work`).
    git_repo(&dir.join("work"));
    let binary = dir.join("codex");
    std::fs::write(&binary, body).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("auth.json"), "{}").unwrap();
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(recorder) as Weak<dyn TurnEventSink>;
    CodexRuntime::new(
        CodexConfig {
            runtime_id: "codex".into(),
            display_name: "Codex CLI".into(),
            binary,
            work_dir: dir.join("work"),
            state_dir: dir.join("state"),
            login_file: dir.join("auth.json"),
            reasoning_effort: None,
        },
        sink,
        Some(bridge),
    )
    .unwrap()
}

async fn thread(rt: &CodexRuntime) -> ThreadRecord {
    let scope = ThreadScope {
        tenant_id: TenantId::new("t").unwrap(),
        user_id: UserId::new("u").unwrap(),
        agent_id: None,
        project_id: None,
    };
    let created = rt
        .create_thread(CreateThread {
            scope: scope.clone(),
            title: None,
            provider: None,
            model: Some("gpt-5.5".into()),
            idempotency_key: "k".into(),
        })
        .await
        .unwrap();
    let mut record = ThreadRecord::new(created.thread_id, scope, RuntimeId::new("codex").unwrap());
    record.native_session_id = created.native_session_id;
    record.model = created.model;
    record
}

fn input() -> TurnInput {
    TurnInput {
        text: "fix the tests".into(),
        origin: TurnOrigin::UserPrompt,
        model: None,
        goal: None,
        effort: None,
        profile: None,
        files: Vec::new(),
    }
}

/// Every event until the turn ends.
async fn events_until_end(rx: &mut broadcast::Receiver<LiveEvent>) -> Vec<LiveEvent> {
    let mut out = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("turn ended in time")
            .expect("bridge open");
        let ended = matches!(event, LiveEvent::Ended { .. });
        out.push(event);
        if ended {
            return out;
        }
    }
}

fn log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("codex.log")).unwrap()
}

#[tokio::test]
async fn a_turn_streams_thinking_text_command_output_and_its_diff() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
    let rt = runtime(dir.path(), &recorder, Arc::clone(&bridge));
    let thread = thread(&rt).await;
    let mut rx = bridge.subscribe();

    rt.start_turn(&thread, input()).await.unwrap();
    let events = events_until_end(&mut rx).await;

    assert!(
        events.iter().any(|e| matches!(e,
            LiveEvent::Thinking { body, .. } if body == "**Running the tests**")),
        "{events:?}"
    );
    assert!(events.iter().any(|e| matches!(e,
        LiveEvent::Text { body, .. } if body == "Fixed it.")));
    let rows: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            LiveEvent::Tool { tool, .. } => Some(tool.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(rows[0].tool_name, "shell");
    assert_eq!(rows[0].subject.as_deref(), Some("cargo test"));
    assert_eq!(rows[0].state, ToolState::Running);
    let last = rows.last().unwrap();
    assert_eq!(
        (last.invocation.as_str(), last.state),
        (rows[0].invocation.as_str(), ToolState::Failed),
        "exit 101 fails the row"
    );
    // The first chunk streams while the command runs; the end sends the rest.
    let tails: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            LiveEvent::ToolOutput {
                invocation, tail, ..
            } if *invocation == rows[0].invocation => Some(tail.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tails.first(), Some(&"running 3 tests\n"), "{events:?}");
    assert_eq!(tails.last(), Some(&"running 3 tests\ntest a ... FAILED\n"));
    assert!(events.iter().any(|e| matches!(e,
        LiveEvent::TurnDiff { diff, truncated: false, .. } if diff.contains("+++ b/src/a.rs"))));

    // The durable record is unchanged: the answer and a `shell()` call.
    let history = rt.history(&thread, 10).await.unwrap();
    assert_eq!(history.last().unwrap().text, "Fixed it.");
    assert_eq!(
        history.last().unwrap().tool_calls,
        vec!["shell()".to_string()]
    );
}

fn git_repo(dir: &Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["init", "-q"])
        .status()
        .unwrap()
        .success();
    assert!(ok, "git init");
    std::fs::canonicalize(dir).unwrap().display().to_string()
}

fn review(repo: &str, target: ReviewTarget) -> ThreadAction {
    ThreadAction::Review {
        target,
        repo: repo.to_string(),
    }
}

fn revert(run: &str, repo: &str) -> ThreadAction {
    ThreadAction::RevertTurn {
        run_id: run.to_string(),
        repo: repo.to_string(),
    }
}

fn started(admission: TurnAdmission) -> String {
    match admission {
        TurnAdmission::Started { run_id: Some(run) } => run,
        other => panic!("not started: {other:?}"),
    }
}

/// Codex works in repo A; the app shows repo B. Review and revert are
/// refused before Codex is asked, naming the root Codex works in.
#[tokio::test]
async fn review_and_revert_are_refused_for_another_repository() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
    let rt = runtime(dir.path(), &recorder, Arc::clone(&bridge));
    let repo_a = std::fs::canonicalize(dir.path().join("work"))
        .unwrap()
        .display()
        .to_string();
    let repo_b = git_repo(&dir.path().join("other"));
    let thread = thread(&rt).await;
    let run = started(rt.start_turn(&thread, input()).await.unwrap());
    wait_finished(&recorder, 1).await;

    let mismatch = ThreadActionOutcome::Refused {
        refusal: ThreadActionRefusal::WorkspaceMismatch {
            runtime_root: repo_a.clone(),
        },
    };
    let reviewed = rt
        .thread_action(&thread, review(&repo_b, ReviewTarget::UncommittedChanges))
        .await
        .unwrap();
    assert_eq!(reviewed, mismatch);
    let reverted = rt
        .thread_action(&thread, revert(&run, &repo_b))
        .await
        .unwrap();
    assert_eq!(reverted, mismatch);
    let l = log(dir.path());
    assert!(
        !l.contains(r#""method":"review/start""#) && !l.contains(r#""method":"thread/revert""#),
        "{l}"
    );
    // The same calls naming repo A go through.
    let ok = rt
        .thread_action(&thread, revert(&run, &repo_a))
        .await
        .unwrap();
    assert!(matches!(ok, ThreadActionOutcome::Done { .. }), "{ok:?}");
}

/// edit (run 1) -> review (run 2) -> undo of run 1's checkpoint: Codex is
/// asked to revert before run 1's own turn, and both runs' rows are hidden
/// (kept in the journal). A review Codex refused journals nothing; reverting
/// again or an unknown run is refused.
#[tokio::test]
async fn revert_targets_the_named_runs_turn_and_hides_its_rows_by_run() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
    let rt = runtime(dir.path(), &recorder, Arc::clone(&bridge));
    let repo = std::fs::canonicalize(dir.path().join("work"))
        .unwrap()
        .display()
        .to_string();
    let thread = thread(&rt).await;

    // Nothing to revert before the session exists: no app-server spawned.
    let early = rt
        .thread_action(&thread, revert("r-none", &repo))
        .await
        .unwrap();
    assert_eq!(
        early,
        ThreadActionOutcome::Refused {
            refusal: ThreadActionRefusal::UnknownTurn
        }
    );
    assert!(!dir.path().join("codex.log").exists());

    let edit = started(rt.start_turn(&thread, input()).await.unwrap());
    wait_finished(&recorder, 1).await;

    // A review Codex refuses leaves no row behind.
    let refused = rt
        .thread_action(
            &thread,
            review(
                &repo,
                ReviewTarget::Custom {
                    instructions: "refuse".into(),
                },
            ),
        )
        .await
        .unwrap();
    assert!(matches!(refused, ThreadActionOutcome::Started { .. }));
    wait_finished(&recorder, 2).await;
    assert_eq!(
        recorder.finished.lock().unwrap()[1].status,
        TurnEndStatus::Failed
    );

    let mut rx = bridge.subscribe();
    let started_review = rt
        .thread_action(&thread, review(&repo, ReviewTarget::UncommittedChanges))
        .await
        .unwrap();
    assert!(matches!(
        started_review,
        ThreadActionOutcome::Started { run_id: Some(_) }
    ));
    let events = events_until_end(&mut rx).await;
    wait_finished(&recorder, 3).await;
    let review_out = recorder.finished.lock().unwrap()[2].clone();
    assert!(review_out.final_text.unwrap().contains("[P1] Off by one"));
    assert!(events.iter().any(|e| matches!(e,
        LiveEvent::Review { text, .. } if text.contains("Off by one"))));

    let history = rt.history(&thread, 50).await.unwrap();
    let texts: Vec<&str> = history.iter().map(|m| m.text.as_str()).collect();
    assert!(!texts.iter().any(|t| t.contains("refuse")), "{texts:?}");

    let done = rt
        .thread_action(&thread, revert(&edit, &repo))
        .await
        .unwrap();
    assert!(matches!(done, ThreadActionOutcome::Done { .. }), "{done:?}");
    let l = log(dir.path());
    assert!(l.contains(r#""beforeTurnId":"tu-1""#), "{l}");
    assert!(
        !l.contains("thread/turns/list"),
        "the run names the turn: {l}"
    );

    let history = rt.history(&thread, 50).await.unwrap();
    assert!(history.iter().all(|m| m.hidden), "{history:?}");
    assert!(
        history.iter().any(|m| m.text.contains("[P1] Off by one")),
        "reverted rows stay in the journal"
    );
    assert_eq!(
        rt.thread_action(&thread, revert(&edit, &repo))
            .await
            .unwrap(),
        ThreadActionOutcome::Refused {
            refusal: ThreadActionRefusal::AlreadyReverted
        }
    );
    assert_eq!(
        rt.thread_action(&thread, revert("r-unknown", &repo))
            .await
            .unwrap(),
        ThreadActionOutcome::Refused {
            refusal: ThreadActionRefusal::UnknownTurn
        }
    );

    // Compact: done once Codex reports the compaction item.
    let compacted = rt
        .thread_action(&thread, ThreadAction::Compact)
        .await
        .unwrap();
    assert!(
        matches!(compacted, ThreadActionOutcome::Done { .. }),
        "{compacted:?}"
    );
    assert!(log(dir.path()).contains(r#""method":"thread/compact/start""#));
    let history = rt.history(&thread, 50).await.unwrap();
    assert_eq!(history.last().unwrap().text, "Context compacted.");
}

/// While a revert holds the session, a prompt and another action are Busy;
/// once it is done, the prompt starts.
#[tokio::test]
async fn a_revert_holds_the_session_until_codex_answers() {
    let dir = tempfile::tempdir().unwrap();
    let gate = dir.path().join("gate");
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
    let rt = Arc::new(gated_runtime(
        dir.path(),
        &recorder,
        Arc::clone(&bridge),
        Some(&gate),
    ));
    let repo = std::fs::canonicalize(dir.path().join("work"))
        .unwrap()
        .display()
        .to_string();
    let thread = thread(&rt).await;
    let run = started(rt.start_turn(&thread, input()).await.unwrap());
    wait_finished(&recorder, 1).await;

    let reverting = {
        let (rt, thread, action) = (Arc::clone(&rt), thread.clone(), revert(&run, &repo));
        tokio::spawn(async move { rt.thread_action(&thread, action).await })
    };
    for _ in 0..500 {
        if dir.path().join("codex.log").exists()
            && log(dir.path()).contains(r#""method":"thread/revert""#)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(log(dir.path()).contains(r#""method":"thread/revert""#));
    assert_eq!(
        rt.start_turn(&thread, input()).await.unwrap(),
        TurnAdmission::Busy
    );
    assert_eq!(
        rt.thread_action(&thread, ThreadAction::Compact)
            .await
            .unwrap(),
        ThreadActionOutcome::Busy
    );
    std::fs::write(&gate, "").unwrap();
    let done = reverting.await.unwrap().unwrap();
    assert!(matches!(done, ThreadActionOutcome::Done { .. }), "{done:?}");
    assert!(matches!(
        rt.start_turn(&thread, input()).await.unwrap(),
        TurnAdmission::Started { .. }
    ));
    wait_finished(&recorder, 2).await;
}

/// A revert whose intent cannot be recorded never reaches Codex.
#[tokio::test]
async fn a_revert_that_cannot_be_recorded_is_not_performed() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
    let rt = runtime(dir.path(), &recorder, Arc::clone(&bridge));
    let repo = std::fs::canonicalize(dir.path().join("work"))
        .unwrap()
        .display()
        .to_string();
    let thread = thread(&rt).await;
    let run = started(rt.start_turn(&thread, input()).await.unwrap());
    wait_finished(&recorder, 1).await;

    let record = dir.path().join("state").join("sessions").join(format!(
        "{}.jsonl",
        thread.native_session_id.as_deref().unwrap()
    ));
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o444)).unwrap();
    let result = rt.thread_action(&thread, revert(&run, &repo)).await;
    assert!(result.is_err(), "{result:?}");
    assert!(!log(dir.path()).contains(r#""method":"thread/revert""#));
    std::fs::set_permissions(&record, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(
        rt.history(&thread, 10)
            .await
            .unwrap()
            .iter()
            .all(|m| !m.hidden)
    );
}

async fn wait_finished(recorder: &Recorder, n: usize) {
    for _ in 0..500 {
        if recorder.finished.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("turn {n} never finished");
}
