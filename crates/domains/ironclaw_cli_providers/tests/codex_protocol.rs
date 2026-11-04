//! The Codex driver's use of the app-server protocol beyond a bare turn,
//! against a fake `codex app-server` that replays recorded JSON-RPC (shapes
//! from `codex app-server generate-ts`, codex-cli 0.166.0):
//!
//! - live turn detail on the bridge: reasoning summaries as thinking, reply
//!   text, the command row with its streaming output tail, the turn diff;
//! - `review/start` as a turn whose findings are the answer;
//! - `thread/revert` before the last turn, with the journal keeping the
//!   reverted rows (hidden, never deleted);
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
use ironclaw_product_contracts::orchestration::{ReviewTarget, ThreadAction, ThreadActionOutcome};
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
    *'"method":"review/start"'*)
      reply "$id" '{"turn":{"id":"tu-r","status":"inProgress"},"reviewThreadId":"cx-1"}'
      emit item/completed '{"threadId":"cx-1","turnId":"tu-r","item":{"type":"enteredReviewMode","id":"e1","review":"current changes"}}'
      emit item/completed '{"threadId":"cx-1","turnId":"tu-r","item":{"type":"exitedReviewMode","id":"x1","review":"One issue.\n\nReview comment:\n\n- [P1] Off by one — /w/src/a.rs:3-4\n  The loop skips the last element."}}'
      emit turn/completed '{"threadId":"cx-1","turn":{"id":"tu-r","status":"completed","error":null}}' ;;
    *'"method":"thread/turns/list"'*) reply "$id" '{"data":[{"id":"tu-r","status":"completed"}],"nextCursor":null,"backwardsCursor":null}' ;;
    *'"method":"thread/revert"'*) reply "$id" '{"thread":{"id":"cx-1"},"turnsBackwardsCursor":null,"itemsBackwardsCursor":null}' ;;
    *'"method":"thread/compact/start"'*)
      reply "$id" '{}'
      emit item/completed '{"threadId":"cx-1","turnId":"tu-c","item":{"type":"contextCompaction","id":"cc"}}' ;;
  esac
done
"#;

fn runtime(dir: &Path, recorder: &Arc<Recorder>, bridge: Arc<CliBridge>) -> CodexRuntime {
    let body = CODEX.replace("__LOG__", &dir.join("codex.log").display().to_string());
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

#[tokio::test]
async fn review_revert_and_compact_use_codex_natively() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
    let rt = runtime(dir.path(), &recorder, Arc::clone(&bridge));
    let thread = thread(&rt).await;

    // Nothing to revert or compact before the session exists.
    let early = rt
        .thread_action(&thread, ThreadAction::RevertLastTurn)
        .await
        .unwrap();
    assert!(matches!(early, ThreadActionOutcome::Done { .. }));
    assert!(
        !dir.path().join("codex.log").exists(),
        "no app-server spawned"
    );

    rt.start_turn(&thread, input()).await.unwrap();
    wait_finished(&recorder, 1).await;

    // Review: a turn whose answer is the findings.
    let mut rx = bridge.subscribe();
    let started = rt
        .thread_action(
            &thread,
            ThreadAction::Review {
                target: ReviewTarget::UncommittedChanges,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        started,
        ThreadActionOutcome::Started { run_id: Some(_) }
    ));
    let events = events_until_end(&mut rx).await;
    wait_finished(&recorder, 2).await;
    let review = recorder.finished.lock().unwrap()[1].clone();
    assert_eq!(review.status, TurnEndStatus::Completed);
    assert!(review.final_text.unwrap().contains("[P1] Off by one"));
    assert!(
        events.iter().any(|e| matches!(e,
        LiveEvent::Review { text, .. } if text.contains("/w/src/a.rs:3-4"))),
        "{events:?}"
    );
    let l = log(dir.path());
    assert!(
        l.contains(r#""method":"review/start""#)
            && l.contains(r#""target":{"type":"uncommittedChanges"}"#)
            && l.contains(r#""delivery":"inline""#),
        "{l}"
    );

    // Revert: Codex drops its last turn; the journal hides, never deletes.
    let reverted = rt
        .thread_action(&thread, ThreadAction::RevertLastTurn)
        .await
        .unwrap();
    assert!(
        matches!(reverted, ThreadActionOutcome::Done { .. }),
        "{reverted:?}"
    );
    assert!(
        log(dir.path()).contains(
            r#""method":"thread/revert","params":{"beforeTurnId":"tu-r","threadId":"cx-1"}"#
        ) || log(dir.path()).contains(r#""beforeTurnId":"tu-r""#)
    );
    let history = rt.history(&thread, 50).await.unwrap();
    let shown: Vec<&str> = history
        .iter()
        .filter(|m| !m.hidden)
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(shown, ["fix the tests", "Fixed it."]);
    assert!(
        history
            .iter()
            .any(|m| m.hidden && m.text.contains("[P1] Off by one")),
        "the reverted review stays in the journal"
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

async fn wait_finished(recorder: &Recorder, n: usize) {
    for _ in 0..500 {
        if recorder.finished.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("turn {n} never finished");
}
