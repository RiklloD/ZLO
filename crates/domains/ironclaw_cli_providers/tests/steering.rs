//! Mid-turn steering on the RPC runtimes (Codex, Muse Code, Antigravity on
//! `agy acp`), against fake agent scripts, matched to T3 Code:
//! - a steer before the turn can take it is held and delivered in order
//!   once it can (never a separate later turn), or reported unread;
//! - a steer at a pending approval declines it, then steers;
//! - a steer is delivered at most once, even when its answer is late;
//! - a steer's transcript row lands inside its run, after the reply it cut.
#![deny(clippy::let_underscore_must_use)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_cli_providers::antigravity::launch::AgentLaunch;
use ironclaw_cli_providers::antigravity::{AntigravityConfig, AntigravityRuntime};
use ironclaw_cli_providers::bridge::{CliBridge, GateAnswer, LiveEvent};
use ironclaw_cli_providers::codex::{CodexConfig, CodexRuntime};
use ironclaw_cli_providers::muse::{MuseConfig, MuseRuntime};
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use ironclaw_orchestrator::model::{Millis, ThreadRecord, ThreadScope};
use ironclaw_orchestrator::runtime::*;

#[derive(Default)]
struct Recorder {
    started: Mutex<Vec<String>>,
    finished: Mutex<Vec<TurnOutcome>>,
    unread: Mutex<Vec<String>>,
    /// `steer_updated` reports: (steer id, state).
    updates: Mutex<Vec<(String, SteerReadState)>>,
}

#[async_trait]
impl TurnEventSink for Recorder {
    async fn turn_started(&self, _: &ThreadId, run: Option<String>, _: Millis) {
        self.started.lock().unwrap().extend(run);
    }
    async fn turn_finished(
        &self,
        _: &ThreadId,
        _: Option<String>,
        outcome: TurnOutcome,
        _: Millis,
    ) {
        self.finished.lock().unwrap().push(outcome);
    }
    async fn steers_unread(
        &self,
        _: &ThreadId,
        _: &str,
        steers: Vec<UnreadSteer>,
    ) -> SteerRecovery {
        assert!(
            self.finished.lock().unwrap().is_empty(),
            "reported before the turn ends"
        );
        self.unread
            .lock()
            .unwrap()
            .extend(steers.into_iter().map(|s| s.text));
        SteerRecovery::default()
    }
    async fn steer_updated(
        &self,
        _: &ThreadId,
        _: &str,
        steer_id: &SteerId,
        state: SteerReadState,
        _: Millis,
    ) {
        self.updates
            .lock()
            .unwrap()
            .push((steer_id.as_str().to_string(), state));
    }
}

/// Steer under a known engine id (what the engine's admission does).
async fn steer_as(
    rt: &dyn TurnRuntime,
    thread: &ThreadRecord,
    id: &str,
    text: &str,
) -> SteerOutcome {
    let request = SteerRequest {
        id: SteerId::new(id).unwrap(),
        text: text.to_string(),
        how: SteerHow::Steer,
    };
    rt.steer_message(thread, &request).await.unwrap()
}

impl Recorder {
    /// Ids the runtime reported unconfirmed (the engine's notice).
    fn unconfirmed(&self) -> Vec<String> {
        self.updates
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, state)| *state == SteerReadState::Unconfirmed)
            .map(|(id, _)| id.clone())
            .collect()
    }
    fn finished(&self) -> Vec<TurnOutcome> {
        self.finished.lock().unwrap().clone()
    }
    fn unread(&self) -> Vec<String> {
        self.unread.lock().unwrap().clone()
    }
    fn run(&self) -> String {
        self.started.lock().unwrap()[0].clone()
    }
}

fn sink(recorder: &Arc<Recorder>) -> Weak<dyn TurnEventSink> {
    Arc::downgrade(recorder) as Weak<dyn TurnEventSink>
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn scope() -> ThreadScope {
    ThreadScope {
        tenant_id: TenantId::new("t").unwrap(),
        user_id: UserId::new("u").unwrap(),
        agent_id: None,
        project_id: None,
    }
}

async fn thread(rt: &dyn TurnRuntime) -> ThreadRecord {
    let created = rt
        .create_thread(CreateThread {
            scope: scope(),
            title: None,
            provider: None,
            model: Some("default".into()),
            idempotency_key: "k".into(),
        })
        .await
        .unwrap();
    let mut record = ThreadRecord::new(created.thread_id, scope(), rt.id());
    record.native_session_id = created.native_session_id;
    record.model = created.model;
    record
}

fn input() -> TurnInput {
    TurnInput {
        text: "hi".into(),
        origin: TurnOrigin::DelegatedBrief,
        model: None,
        goal: None,
        effort: None,
        profile: None,
        files: Vec::new(),
        work_dir: None,
    }
}

async fn wait_for(cond: impl Fn() -> bool) {
    for _ in 0..400 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("condition not reached");
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// `(role, text)` of each history row, and whether all carry `run`.
async fn rows(rt: &dyn TurnRuntime, thread: &ThreadRecord, run: &str) -> (Vec<String>, bool) {
    let history = rt.history(thread, 50).await.unwrap();
    let all_in_run = history.iter().all(|m| m.run_id.as_deref() == Some(run));
    let rows = history
        .iter()
        .map(|m| format!("{:?}:{}", m.role, m.text))
        .collect();
    (rows, all_in_run)
}

/// The orchestrator store the host records approvals in (Review, Sol 6:
/// with it, a decline is a durable write before the agent hears it, which
/// a steer must wait for).
async fn ledger(dir: &Path, bridge: &CliBridge) -> Arc<ironclaw_orchestrator::engine::Engine> {
    let db = Arc::new(
        libsql::Builder::new_local(dir.join("orch.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = ironclaw_orchestrator::engine::Engine::open_libsql(runtime, "steer")
        .await
        .unwrap();
    bridge.record_to(engine.store().clone());
    engine
}

async fn wait_gate(events: &mut tokio::sync::broadcast::Receiver<LiveEvent>) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("gate")
            .unwrap();
        if matches!(event, LiveEvent::GateOpened { .. }) {
            return;
        }
    }
}

// ── Codex ───────────────────────────────────────────────────────────────────

/// `__MODE__`:
/// - `early`: `turn/start` is answered once `go` exists; two steers end it;
/// - `never`: `turn/start` is refused once `go` exists;
/// - `cut`: the reply so far is one finished message and a command, then
///   `said`; the steer gets a new message and ends the turn;
/// - `slow`: `turn/steer` is never answered; the turn ends once `end` exists;
/// - `slowend`: `turn/steer` is never answered and the turn ends at once;
/// - `gate`: an approval waits; the steer ends the turn;
/// - `heldend`: like `early`, but no steer is answered and the first one
///   ends the turn at once;
/// - `gatehold`: like `early`, then an approval is asked at once; no steer
///   is answered; the approval's answer ends the turn;
/// - `heldstop`: like `early`; no steer is answered; `turn/interrupt` ends
///   the turn;
/// - `ask`: a `requestUserInput` question waits; the steer ends the turn.
const CODEX: &str = r#"#!/bin/sh
LOG=__DIR__/codex.log
DIR=__DIR__
MODE=__MODE__
id_of() { printf '%s' "$1" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p'; }
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
emit() { printf '{"jsonrpc":"2.0","method":"%s","params":%s}\n' "$1" "$2"; }
said() { emit item/completed "{\"threadId\":\"cx-1\",\"turnId\":\"tu-1\",\"item\":{\"type\":\"agentMessage\",\"id\":\"$1\",\"text\":\"$2\"}}"; }
done_turn() { emit turn/completed '{"threadId":"cx-1","turn":{"id":"tu-1","status":"completed","error":null}}'; }
STEERS=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$LOG"
  id=$(id_of "$line")
  case "$line" in
    *'"decision":'*) if [ "$MODE" = gatehold ]; then done_turn; fi ;;
    *'"method":"initialize"'*) reply "$id" '{"userAgent":"codex"}' ;;
    *'"method":"initialized"'*) ;;
    *'"method":"thread/start"'*|*'"method":"thread/resume"'*) reply "$id" '{"thread":{"id":"cx-1"},"model":"gpt-b"}' ;;
    *'"method":"turn/start"'*)
      touch "$DIR/starting"
      if [ "$MODE" = early ] || [ "$MODE" = never ] || [ "$MODE" = heldend ] || [ "$MODE" = gatehold ] || [ "$MODE" = heldstop ] || [ "$MODE" = burst ]; then
        while [ ! -e "$DIR/go" ]; do sleep 0.05; done
      fi
      # Output already sent when the turn id comes back.
      if [ "$MODE" = burst ]; then said m1 "First"; said m2 "Second"; fi
      if [ "$MODE" = never ]; then
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32600,"message":"busy"}}\n' "$id"
        continue
      fi
      reply "$id" '{"turn":{"id":"tu-1","status":"inProgress"}}'
      if [ "$MODE" = cut ]; then
        emit item/agentMessage/delta '{"threadId":"cx-1","turnId":"tu-1","itemId":"m1","delta":"Looking"}'
        said m1 "Looking at it"
        emit item/completed '{"threadId":"cx-1","turnId":"tu-1","item":{"type":"commandExecution","id":"c1","command":"ls"}}'
      fi
      if [ "$MODE" = gate ] || [ "$MODE" = gatehold ]; then
        printf '%s\n' '{"jsonrpc":"2.0","id":99,"method":"item/commandExecution/requestApproval","params":{"threadId":"cx-1","turnId":"tu-1","itemId":"c1","command":"rm -rf build","kind":"command","startedAtMs":0}}'
      fi
      if [ "$MODE" = ask ]; then
        printf '%s\n' '{"jsonrpc":"2.0","id":98,"method":"item/tool/requestUserInput","params":{"threadId":"cx-1","turnId":"tu-1","itemId":"q1","questions":[{"id":"db","header":"DB","question":"Which database?","options":[{"label":"postgres","description":"pg"},{"label":"sqlite","description":"file"}]}]}}'
      fi
      touch "$DIR/said"
      # Stops reading its input (alive), then exits.
      if [ "$MODE" = deaf ]; then sleep 2; exit 0; fi ;;
    *'"method":"turn/interrupt"'*)
      reply "$id" '{}'
      emit turn/completed '{"threadId":"cx-1","turn":{"id":"tu-1","status":"interrupted","error":null}}' ;;
    *'"method":"turn/steer"'*)
      STEERS=$((STEERS+1))
      case "$MODE" in
        slow) touch "$DIR/steered"; while [ ! -e "$DIR/end" ]; do sleep 0.05; done; said m2 "Done"; done_turn ;;
        slowend|heldend) done_turn ;;
        gatehold|heldstop) ;;
        early) reply "$id" '{"turnId":"tu-1"}'; if [ $STEERS -eq 2 ]; then said m2 "Done"; done_turn; fi ;;
        cut) reply "$id" '{"turnId":"tu-1"}'; said m2 "Short"; done_turn ;;
        burst) reply "$id" '{"turnId":"tu-1"}'; said m3 "Short"; done_turn ;;
        *) reply "$id" '{"turnId":"tu-1"}'; said m2 "Done"; done_turn ;;
      esac ;;
  esac
done
"#;

fn codex(
    dir: &Path,
    mode: &str,
    recorder: &Arc<Recorder>,
    bridge: Option<Arc<CliBridge>>,
) -> CodexRuntime {
    let body = CODEX
        .replace("__DIR__", &dir.display().to_string())
        .replace("__MODE__", mode);
    std::fs::write(dir.join("auth.json"), "{}").unwrap();
    CodexRuntime::new(
        CodexConfig {
            runtime_id: "codex".into(),
            display_name: "Codex CLI".into(),
            binary: script(dir, "codex", &body),
            work_dir: dir.join("work"),
            state_dir: dir.join("state"),
            login_file: dir.join("auth.json"),
            reasoning_effort: None,
        },
        sink(recorder),
        bridge,
    )
    .unwrap()
}

/// Steers sent while `turn/start` is unanswered join the turn in order once
/// it has an id, instead of becoming a later turn.
#[tokio::test]
async fn codex_steers_before_the_turn_starts_join_it_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = codex(dir.path(), "early", &recorder, None);
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("starting").exists()).await;
    for text in ["early-one", "early-two"] {
        assert_eq!(
            rt.steer(&thread, text, SteerHow::Steer).await.unwrap(),
            SteerOutcome::Steered
        );
    }
    std::fs::write(dir.path().join("go"), "").unwrap();
    wait_for(|| recorder.finished().len() == 1).await;
    assert_eq!(recorder.finished()[0].status, TurnEndStatus::Completed);
    assert!(recorder.unread().is_empty());
    let log = read(&dir.path().join("codex.log"));
    let start = log.find(r#""method":"turn/start""#).unwrap();
    let one = log.find("early-one").unwrap();
    let two = log.find("early-two").unwrap();
    assert!(start < one && one < two, "{log}");
    let (rows, in_run) = rows(&rt, &thread, &recorder.run()).await;
    assert_eq!(
        rows,
        [
            "User:hi",
            "User:early-one",
            "User:early-two",
            "Assistant:Done"
        ]
    );
    assert!(in_run);
}

/// The turn never starts: the held steer comes back unread for the engine
/// to queue (it was answered `Steered`, so nothing else would keep it).
#[tokio::test]
async fn codex_steer_held_for_a_turn_that_never_starts_comes_back_unread() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = codex(dir.path(), "never", &recorder, None);
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("starting").exists()).await;
    assert_eq!(
        rt.steer(&thread, "early", SteerHow::Steer).await.unwrap(),
        SteerOutcome::Steered
    );
    std::fs::write(dir.path().join("go"), "").unwrap();
    wait_for(|| recorder.finished().len() == 1).await;
    assert_eq!(recorder.finished()[0].status, TurnEndStatus::Failed);
    assert_eq!(recorder.unread(), ["early"]);
    assert!(!read(&dir.path().join("codex.log")).contains("turn/steer"));
    // After the turn: gone, the engine starts the next turn with it.
    assert_eq!(
        rt.steer(&thread, "late", SteerHow::Steer).await.unwrap(),
        SteerOutcome::TurnEnded
    );
}

/// The reply before the steer is its own entry, then the steer, then the
/// rest: the steer row no longer lands before the whole reply.
#[tokio::test]
async fn codex_journals_the_reply_before_the_steer_ahead_of_its_row() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = codex(dir.path(), "cut", &recorder, None);
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("said").exists()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        rt.steer(&thread, "shorter please", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Steered
    );
    wait_for(|| recorder.finished().len() == 1).await;
    let history = rt.history(&thread, 50).await.unwrap();
    let shape: Vec<(String, String, usize)> = history
        .iter()
        .map(|m| (format!("{:?}", m.role), m.text.clone(), m.tool_calls.len()))
        .collect();
    assert_eq!(
        shape,
        [
            ("User".into(), "hi".into(), 0),
            ("Assistant".into(), "Looking at it".into(), 1),
            ("User".into(), "shorter please".into(), 0),
            ("Assistant".into(), "Short".into(), 0),
        ]
    );
    assert!(
        history
            .iter()
            .all(|m| m.run_id.as_deref() == Some(recorder.run().as_str()))
    );
    assert_eq!(recorder.finished()[0].final_text.as_deref(), Some("Short"));
}

/// An unanswered `turn/steer` while the turn still runs counts as delivered
/// (it is on the wire; queueing it as well could deliver it twice). One
/// whose turn ended without answering is refused, so the engine queues it.
#[tokio::test]
async fn codex_an_unanswered_steer_is_delivered_once_or_queued_once() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt =
        codex(dir.path(), "slow", &recorder, None).with_steer_timeout(Duration::from_millis(300));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("said").exists()).await;
    // Past the start: the turn is live, so the steer is not held.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        steer_as(&rt, &thread, "s-slow", "slow answer").await,
        SteerOutcome::Steered
    );
    std::fs::write(dir.path().join("end"), "").unwrap();
    wait_for(|| recorder.finished().len() == 1).await;
    // Written, never answered, turn over: no evidence it was read (Sol 3),
    // so it is unconfirmed: the engine shows the notice, never requeued.
    assert!(recorder.unread().is_empty());
    assert_eq!(recorder.unconfirmed(), ["s-slow"]);
    let (rows, _) = rows(&rt, &thread, &recorder.run()).await;
    assert_eq!(rows, ["User:hi", "User:slow answer", "Assistant:Done"]);
    let history = rt.history(&thread, 50).await.unwrap();
    assert_eq!(
        history[1].steer_id.as_deref(),
        Some("s-slow"),
        "the row is marked"
    );
}

/// Review (Sol, 2): the same unanswered `turn/steer`, but its turn ends
/// first. Codex may or may not have read it: never counted delivered, never
/// requeued (it could be delivered twice), but kept in the run with a
/// "Steer delivery not confirmed" notice for the person to decide.
#[tokio::test]
async fn codex_an_unanswered_steer_whose_turn_ended_is_unconfirmed() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = codex(dir.path(), "slowend", &recorder, None)
        .with_steer_timeout(Duration::from_millis(500));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("said").exists()).await;
    // Past the start: the turn is live, so the steer is not held.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        steer_as(&rt, &thread, "s-late", "too late").await,
        SteerOutcome::Steered,
        "not refused: it may have been read"
    );
    wait_for(|| recorder.finished().len() == 1).await;
    assert!(recorder.unread().is_empty(), "never requeued");
    assert_eq!(recorder.unconfirmed(), ["s-late"], "the engine's notice");
    let (rows, in_run) = rows(&rt, &thread, &recorder.run()).await;
    assert_eq!(rows, ["User:hi", "User:too late"]);
    assert!(in_run);
}

/// Review (Sol, 2): held steers sent at the start whose answers never come
/// while the turn completes: the completion is read (the turn ends in
/// seconds, not one steer timeout after another), and each steer is either
/// unconfirmed (in the run, with the notice) or back for the engine.
#[tokio::test]
async fn codex_a_completion_during_the_startup_steers_ends_the_turn_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt =
        codex(dir.path(), "heldend", &recorder, None).with_steer_timeout(Duration::from_secs(20));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("starting").exists()).await;
    for text in ["held-one", "held-two"] {
        assert_eq!(
            steer_as(&rt, &thread, text, text).await,
            SteerOutcome::Steered
        );
    }
    let started = std::time::Instant::now();
    std::fs::write(dir.path().join("go"), "").unwrap();
    wait_for(|| recorder.finished().len() == 1).await;
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "the turn's end waited on its steers: {:?}",
        started.elapsed()
    );
    let unread = recorder.unread();
    let unconfirmed = recorder.unconfirmed();
    for text in ["held-one", "held-two"] {
        let noticed = unconfirmed.iter().any(|id| id == text);
        assert!(
            noticed ^ unread.iter().any(|u| u == text),
            "{text}: {unconfirmed:?} {unread:?}"
        );
    }
    assert!(unconfirmed.iter().any(|id| id == "held-one"));
}

/// Review (Sol, 2): while a startup steer waits for its answer, the turn
/// still serves the agent: an approval it asks opens at once.
#[tokio::test]
async fn codex_an_approval_during_a_startup_steer_opens_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(30));
    let rt = codex(dir.path(), "gatehold", &recorder, Some(Arc::clone(&bridge)))
        .with_steer_timeout(Duration::from_secs(20));
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("starting").exists()).await;
    assert_eq!(
        rt.steer(&thread, "held", SteerHow::Steer).await.unwrap(),
        SteerOutcome::Steered
    );
    let started = std::time::Instant::now();
    std::fs::write(dir.path().join("go"), "").unwrap();
    wait_gate(&mut events).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the approval waited on the steer: {:?}",
        started.elapsed()
    );
    let gate = bridge.snapshot(&thread.thread_id).unwrap().gates[0].clone();
    assert!(bridge.resolve_gate(&thread.thread_id, &gate.gate_ref, GateAnswer::Approve));
    wait_for(|| recorder.finished().len() == 1).await;
}

/// Review (Sol, 2): Stop while a startup steer waits for its answer
/// interrupts the native turn (its id is published at once) instead of
/// waiting for the steer and then killing the server.
#[tokio::test]
async fn codex_stop_during_a_startup_steer_interrupts_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt =
        codex(dir.path(), "heldstop", &recorder, None).with_steer_timeout(Duration::from_secs(20));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("starting").exists()).await;
    assert_eq!(
        rt.steer(&thread, "held", SteerHow::Steer).await.unwrap(),
        SteerOutcome::Steered
    );
    std::fs::write(dir.path().join("go"), "").unwrap();
    wait_for(|| read(&dir.path().join("codex.log")).contains("turn/steer")).await;
    rt.cancel(&thread, "stop").await.unwrap();
    let log = read(&dir.path().join("codex.log"));
    assert!(log.contains(r#""method":"turn/interrupt""#), "{log}");
    assert_eq!(recorder.finished()[0].status, TurnEndStatus::Cancelled);
}

/// Review (Sol, 6): a steer while a question waits answers it (Codex's
/// reply cannot carry free text, so the text goes as the steer, once), and
/// the question never gets the approval-denial sentence.
#[tokio::test]
async fn codex_a_steer_at_a_pending_question_answers_it_and_steers_once() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let rt = codex(dir.path(), "ask", &recorder, Some(Arc::clone(&bridge)));
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("question")
            .unwrap();
        if matches!(event, LiveEvent::QuestionOpened { .. }) {
            break;
        }
    }
    assert_eq!(
        rt.steer(&thread, "use sqlite", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Steered
    );
    wait_for(|| recorder.finished().len() == 1).await;
    let log = read(&dir.path().join("codex.log"));
    let answered = log
        .lines()
        .position(|l| l.contains(r#""id":98"#) && l.contains("result"))
        .expect("question answered");
    let steer = log
        .lines()
        .position(|l| l.contains("turn/steer"))
        .expect("steered");
    assert!(answered < steer, "{log}");
    assert!(!log.contains("declined this call"), "{log}");
    assert_eq!(log.matches("use sqlite").count(), 1, "once: {log}");
}

/// A steer at a pending approval declines it first (Codex's reply carries
/// no message), then steers the words in.
#[tokio::test]
async fn codex_a_steer_at_a_pending_approval_declines_it_then_steers() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let _ledger = ledger(dir.path(), &bridge).await;
    let rt = codex(dir.path(), "gate", &recorder, Some(Arc::clone(&bridge)));
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    wait_gate(&mut events).await;
    assert_eq!(
        rt.steer(&thread, "keep the build folder", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Steered
    );
    wait_for(|| recorder.finished().len() == 1).await;
    let log = read(&dir.path().join("codex.log"));
    let decline = log.find(r#""decision":"decline""#).expect("declined");
    let steer = log.find(r#""method":"turn/steer""#).expect("steered");
    assert!(decline < steer, "{log}");
    assert_eq!(
        log.matches("keep the build folder").count(),
        1,
        "once: {log}"
    );
}

// ── Muse Code ───────────────────────────────────────────────────────────────

/// `__MODE__`: `early` answers `turn/start` once `go` exists and a steer
/// ends the turn; `gate` asks for approval, and a steer ends the turn;
/// `cut` finishes one message before the steer (then `said`), and the
/// steer's answer streams another at once; `gatehold` is `early` whose
/// steer is never answered while an approval is asked; its decision ends
/// the turn.
const MUSE: &str = r#"#!/bin/sh
LOG=__DIR__/muse.log
DIR=__DIR__
MODE=__MODE__
id_of() { printf '%s' "$1" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p'; }
field() { printf '%s' "$1" | sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p"; }
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
emit() { printf '{"jsonrpc":"2.0","method":"%s","params":%s}\n' "$1" "$2"; }
STEERS=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$LOG"
  id=$(id_of "$line")
  case "$line" in
    *'"method":"initialize"'*) reply "$id" '{"schema":{"version":1},"grantedCapabilities":[]}' ;;
    *'"method":"initialized"'*) ;;
    *'"method":"usage/read"'*) reply "$id" '{}' ;;
    *'"method":"session/start"'*|*'"method":"session/resume"'*)
      SID=$(field "$line" sessionId)
      reply "$id" "{\"session\":{\"sessionId\":\"$SID\",\"modelId\":\"muse-spark-1.3\"}}" ;;
    *'"method":"session/setApprovalMode"'*|*'"method":"session/setModel"'*) reply "$id" '{}' ;;
    *'"method":"approval/decide"'*)
      # The decision's answer comes late; the host keeps reading.
      if [ "$MODE" = gateslow ]; then ( sleep 1; reply "$id" '{}' ) & continue; fi
      reply "$id" '{}'
      if [ "$MODE" = gatehold ]; then emit turn/completed "{\"sessionId\":\"$SID\",\"turnId\":\"$TID\",\"terminal\":\"completed\"}"; fi ;;
    *'"method":"turn/steer"'*)
      [ "$MODE" = gatehold ] && continue
      if [ "$MODE" = wrongturn ]; then reply "$id" '{"turnId":"another-turn"}'; continue; fi
      STEERS=$((STEERS+1))
      if [ "$MODE" = gateslow ] && [ $STEERS -lt 2 ]; then reply "$id" "{\"turnId\":\"$TID\"}"; continue; fi
      reply "$id" "{\"turnId\":\"$TID\"}"
      emit item/completed "{\"sessionId\":\"$SID\",\"item\":{\"itemId\":\"m1\",\"turnId\":\"$TID\",\"kind\":\"agentMessage\",\"status\":\"completed\",\"revision\":2,\"text\":\"Done\"}}"
      emit turn/completed "{\"sessionId\":\"$SID\",\"turnId\":\"$TID\",\"terminal\":\"completed\"}" ;;
    *'"method":"turn/start"'*)
      TID=$(field "$line" commandId)
      touch "$DIR/starting"
      if [ "$MODE" = early ] || [ "$MODE" = gatehold ]; then while [ ! -e "$DIR/go" ]; do sleep 0.05; done; fi
      reply "$id" "{\"turnId\":\"$TID\",\"disposition\":\"started\"}"
      emit turn/started "{\"sessionId\":\"$SID\",\"turnId\":\"$TID\"}"
      if [ "$MODE" = cut ]; then
        emit item/completed "{\"sessionId\":\"$SID\",\"item\":{\"itemId\":\"m0\",\"turnId\":\"$TID\",\"kind\":\"agentMessage\",\"status\":\"completed\",\"revision\":1,\"text\":\"Looking at it\"}}"
        touch "$DIR/said"
      fi
      if [ "$MODE" = wrongturn ]; then touch "$DIR/said"; fi
      if [ "$MODE" = gate ] || [ "$MODE" = gatehold ] || [ "$MODE" = gateslow ]; then
        emit approval/requested "{\"sessionId\":\"$SID\",\"approvalId\":\"a1\",\"turnId\":\"$TID\",\"status\":\"pending\",\"toolName\":\"bash\",\"currentRequirementId\":\"r1\",\"subject\":{\"command\":\"rm -rf build\"},\"availableChoices\":[{\"choiceId\":\"ok\",\"decision\":\"approved\",\"scope\":\"once\"},{\"choiceId\":\"no\",\"decision\":\"denied\",\"scope\":\"once\"}]}"
      fi ;;
  esac
done
"#;

fn muse_with_timeout(
    dir: &Path,
    mode: &str,
    recorder: &Arc<Recorder>,
    bridge: Option<Arc<CliBridge>>,
    timeout: Duration,
) -> MuseRuntime {
    muse(dir, mode, recorder, bridge).with_steer_timeout(timeout)
}

fn muse(
    dir: &Path,
    mode: &str,
    recorder: &Arc<Recorder>,
    bridge: Option<Arc<CliBridge>>,
) -> MuseRuntime {
    let body = MUSE
        .replace("__DIR__", &dir.display().to_string())
        .replace("__MODE__", mode);
    MuseRuntime::new(
        MuseConfig {
            runtime_id: "muse-code".into(),
            display_name: "Muse Code".into(),
            binary: script(dir, "muse", &body),
            work_dir: dir.join("work"),
            state_dir: dir.join("state"),
            usage_file: None,
            reasoning_effort: None,
            orchestrator_tools: false,
        },
        sink(recorder),
        bridge,
    )
    .unwrap()
}

/// A steer before `turn/start` returns joins the turn, and every row of the
/// turn (prompt, steer, reply) is journaled under its run.
#[tokio::test]
async fn muse_a_steer_before_the_turn_starts_joins_it_under_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = muse(dir.path(), "early", &recorder, None);
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("starting").exists()).await;
    assert_eq!(
        rt.steer(&thread, "early words", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Steered
    );
    std::fs::write(dir.path().join("go"), "").unwrap();
    wait_for(|| recorder.finished().len() == 1).await;
    assert!(recorder.unread().is_empty());
    let (rows, in_run) = rows(&rt, &thread, &recorder.run()).await;
    assert_eq!(rows, ["User:hi", "User:early words", "Assistant:Done"]);
    assert!(in_run, "every row in the run");
}

/// A steer at a pending approval declines it first, then steers.
#[tokio::test]
async fn muse_a_steer_at_a_pending_approval_declines_it_then_steers() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let _ledger = ledger(dir.path(), &bridge).await;
    let rt = muse(dir.path(), "gate", &recorder, Some(Arc::clone(&bridge)));
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    wait_gate(&mut events).await;
    assert_eq!(
        rt.steer(&thread, "keep the build folder", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Steered
    );
    wait_for(|| recorder.finished().len() == 1).await;
    wait_for(|| read(&dir.path().join("muse.log")).contains("approval/decide")).await;
    let log = read(&dir.path().join("muse.log"));
    let decide = log
        .lines()
        .position(|l| l.contains("approval/decide") && l.contains(r#""choiceId":"no""#))
        .expect("declined");
    let steer = log
        .lines()
        .position(|l| l.contains(r#""method":"turn/steer""#))
        .expect("steered");
    assert!(
        decide < steer,
        "declined before the steer, not at turn end: {log}"
    );
    assert_eq!(
        log.matches("keep the build folder").count(),
        1,
        "once: {log}"
    );
}

/// Review (Sol, 7): Muse cuts the reply at the steer too, and output the
/// steer's answer streams at once still lands after the steer's row.
#[tokio::test]
async fn muse_journals_the_reply_before_the_steer_ahead_of_its_row() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = muse(dir.path(), "cut", &recorder, None);
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("said").exists()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        rt.steer(&thread, "shorter please", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Steered
    );
    wait_for(|| recorder.finished().len() == 1).await;
    let (rows, in_run) = rows(&rt, &thread, &recorder.run()).await;
    assert_eq!(
        rows,
        [
            "User:hi",
            "Assistant:Looking at it",
            "User:shorter please",
            "Assistant:Done"
        ]
    );
    assert!(in_run);
}

/// Review (Sol, 2): Muse serves the agent while a startup steer waits for
/// its answer: an approval it asks opens at once.
#[tokio::test]
async fn muse_an_approval_during_a_startup_steer_opens_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(30));
    let rt = muse_with_timeout(
        dir.path(),
        "gatehold",
        &recorder,
        Some(Arc::clone(&bridge)),
        Duration::from_secs(20),
    );
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("starting").exists()).await;
    assert_eq!(
        rt.steer(&thread, "held", SteerHow::Steer).await.unwrap(),
        SteerOutcome::Steered
    );
    let started = std::time::Instant::now();
    std::fs::write(dir.path().join("go"), "").unwrap();
    wait_gate(&mut events).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the approval waited on the steer: {:?}",
        started.elapsed()
    );
    let gate = bridge.snapshot(&thread.thread_id).unwrap().gates[0].clone();
    assert!(bridge.resolve_gate(&thread.thread_id, &gate.gate_ref, GateAnswer::Approve));
    wait_for(|| recorder.finished().len() == 1).await;
}

// ── Antigravity (`agy acp`) ─────────────────────────────────────────────────

/// `agy` 1.5 that opens its session only once `go` exists (`__MODE__=ok`),
/// or fails to open it (`fail`). Its first prompt stays open until a second
/// one (the steer) arrives. `perm` opens at once and its first prompt asks
/// to run a command before it waits.
const AGY: &str = r#"#!/bin/sh
LOG=__DIR__/agy.log
DIR=__DIR__
MODE=__MODE__
id_of() { printf '%s' "$1" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p'; }
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
note() { printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"acp-1","update":%s}}\n' "$1"; }
chunk() { note "{\"sessionUpdate\":\"agent_message_chunk\",\"content\":{\"type\":\"text\",\"text\":\"$1\"}}"; }
OPTS='[{"id":"model","type":"select","currentValue":"gemini-a","options":[{"value":"gemini-a"}]}]'
P1=
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$LOG"
  id=$(id_of "$line")
  case "$line" in
    *'"method":"initialize"'*) reply "$id" '{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"Antigravity ACP Server","version":"1.5.11"}}' ;;
    *'"method":"authenticate"'*) reply "$id" '{}' ;;
    *'"method":"session/new"'*|*'"method":"session/resume"'*)
      touch "$DIR/opening"
      [ "$MODE" = perm ] || [ "$MODE" = closein ] || [ "$MODE" = deaf ] || [ "$MODE" = now ] || [ "$MODE" = silentexit ] || [ "$MODE" = gatetwo ] || while [ ! -e "$DIR/go" ]; do sleep 0.05; done
      if [ "$MODE" = fail ]; then printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32603,"message":"no session"}}\n' "$id"
      else reply "$id" "{\"sessionId\":\"acp-1\",\"configOptions\":$OPTS}"; fi ;;
    *'"method":"session/set_config_option"'*) reply "$id" '{}' ;;
    *'"method":"session/prompt"'*)
      # A steer read, then the agent exits without answering it.
      if [ -n "$P1" ] && [ "$MODE" = silentexit ]; then exit 0; fi
      # Three steers (one before the gate, two at it): every prompt is
      # logged in arrival order; the third ends the turn.
      if [ -n "$P1" ] && [ "$MODE" = gatetwo ]; then
        STEERS=$((STEERS+1)); IDS="$IDS $id"
        if [ $STEERS -lt 3 ]; then continue; fi
        reply "$P1" '{"stopReason":"end_turn"}'
        for i in $IDS; do reply "$i" '{"stopReason":"end_turn","usage":{"inputTokens":10,"outputTokens":2}}'; done
        continue
      fi
      if [ -n "$P1" ]; then
        reply "$P1" '{"stopReason":"end_turn"}'
        chunk "Steered"
        reply "$id" '{"stopReason":"end_turn","usage":{"inputTokens":10,"outputTokens":2}}'
      elif [ "$MODE" = eofstall ]; then
        # Closes its output, keeps its input open unread, then exits.
        P1=$id; chunk "Par"
        exec 1>&-
        sleep 3
        exit 0
      elif [ "$MODE" = silentexit ]; then
        P1=$id; chunk "Par"; touch "$DIR/said"
      elif [ "$MODE" = gatetwo ]; then
        # Stops reading its input a while (an earlier big steer stalls in
        # the pipe), asks to run a command meanwhile, then reads on.
        P1=$id; STEERS=0; IDS=
        touch "$DIR/said"
        sleep 0.5
        printf '%s\n' '{"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{"sessionId":"acp-1","toolCall":{"toolCallId":"c1","title":"rm -rf build","kind":"execute","status":"pending"},"options":[{"optionId":"yes","kind":"allow_once","name":"Allow"},{"optionId":"no","kind":"reject_once","name":"Deny"}]}}'
        sleep 1.5
      elif [ "$MODE" = deaf ]; then
        P1=$id; chunk "Par"
        touch "$DIR/said"
        sleep 3
      elif [ "$MODE" = closein ]; then
        P1=$id; chunk "Par"
        exec 0<&-
        touch "$DIR/said"
        sleep 3
      elif [ "$MODE" = perm ]; then
        P1=$id
        printf '%s\n' '{"jsonrpc":"2.0","id":"p1","method":"session/request_permission","params":{"sessionId":"acp-1","toolCall":{"toolCallId":"c1","title":"rm -rf build","kind":"execute","status":"pending"},"options":[{"optionId":"yes","kind":"allow_once","name":"Allow"},{"optionId":"no","kind":"reject_once","name":"Deny"}]}}'
        IFS= read -r answer; printf '%s\n' "$answer" >> "$LOG"
      else P1=$id; chunk "Par"; fi ;;
  esac
done
"#;

fn antigravity(dir: &Path, mode: &str, recorder: &Arc<Recorder>) -> AntigravityRuntime {
    antigravity_with(dir, mode, recorder, None)
}

fn antigravity_with(
    dir: &Path,
    mode: &str,
    recorder: &Arc<Recorder>,
    bridge: Option<Arc<CliBridge>>,
) -> AntigravityRuntime {
    let body = AGY
        .replace("__DIR__", &dir.display().to_string())
        .replace("__MODE__", mode);
    AntigravityRuntime::new(
        AntigravityConfig {
            runtime_id: "antigravity".into(),
            display_name: "Antigravity".into(),
            launch: AgentLaunch::Cli {
                agy: script(dir, "agy", &body),
            },
            profile_dir: dir.join("profile"),
            user_gemini_dir: None,
            auth_method: "oauth-personal".into(),
            auth_timeout: Duration::from_secs(5),
            work_dir: dir.join("work"),
            state_dir: dir.join("state"),
            background_hold: Duration::from_secs(30),
            prompt_silence: Duration::from_secs(120),
            keyring: None,
        },
        sink(recorder),
        bridge,
    )
    .unwrap()
}

/// A steer while the ACP session is still opening is accepted at once and
/// sent once the first prompt is out; a session that never opens reports
/// it unread.
#[tokio::test]
async fn antigravity_a_steer_before_the_session_opens_joins_the_turn_or_comes_back_unread() {
    for mode in ["ok", "fail"] {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Arc::new(Recorder::default());
        let rt = antigravity(dir.path(), mode, &recorder);
        let thread = thread(&rt).await;
        rt.start_turn(&thread, input()).await.unwrap();
        wait_for(|| dir.path().join("opening").exists()).await;
        assert_eq!(
            rt.steer(&thread, "early words", SteerHow::Steer)
                .await
                .unwrap(),
            SteerOutcome::Steered,
            "{mode}"
        );
        std::fs::write(dir.path().join("go"), "").unwrap();
        wait_for(|| recorder.finished().len() == 1).await;
        let log = read(&dir.path().join("agy.log"));
        if mode == "ok" {
            assert!(recorder.unread().is_empty());
            assert_eq!(recorder.finished()[0].status, TurnEndStatus::Completed);
            let prompts: Vec<&str> = log
                .lines()
                .filter(|l| l.contains(r#""method":"session/prompt""#))
                .collect();
            assert_eq!(prompts.len(), 2, "{log}");
            assert!(prompts[1].contains("early words"), "after the first: {log}");
        } else {
            assert_eq!(recorder.unread(), ["early words"]);
            assert!(!log.contains("early words"), "{log}");
        }
    }
}

/// A steer at a pending permission rejects it first, then goes out as the
/// next `session/prompt`; the words reach the agent once.
#[tokio::test]
async fn antigravity_a_steer_at_a_pending_permission_rejects_it_then_steers() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let _ledger = ledger(dir.path(), &bridge).await;
    let rt = antigravity_with(dir.path(), "perm", &recorder, Some(Arc::clone(&bridge)));
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    wait_gate(&mut events).await;
    assert_eq!(
        rt.steer(&thread, "keep the build folder", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Steered
    );
    wait_for(|| recorder.finished().len() == 1).await;
    let log = read(&dir.path().join("agy.log"));
    let lines: Vec<&str> = log.lines().collect();
    let reject = lines
        .iter()
        .position(|l| l.contains(r#""id":"p1""#) && l.contains(r#""optionId":"no""#))
        .expect("rejected");
    let steer = lines
        .iter()
        .position(|l| l.contains("session/prompt") && l.contains("keep the build folder"))
        .expect("steered");
    assert!(reject < steer, "{log}");
    assert_eq!(
        log.matches("keep the build folder").count(),
        1,
        "once: {log}"
    );
    assert_eq!(recorder.finished()[0].status, TurnEndStatus::Completed);
}

/// Review (Sol, 5): "queued" is not "sent". With the agent's stdin closed,
/// a steer's prompt never reaches it: refused (the engine queues it), not
/// acknowledged because it entered a local channel.
#[tokio::test]
async fn antigravity_a_steer_the_agent_cannot_read_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = antigravity(dir.path(), "closein", &recorder);
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("said").exists()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        rt.steer(&thread, "never read", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Rejected
    );
}

// ── Review round 3 (Sol): one ordered input side, evidence-only outcomes ──

/// Sol 3: a steer whose frame is stuck in a pipe the server stopped
/// reading is never counted as success on a late answer: its requester
/// waits for the write, and when the server goes away unread, it is
/// refused (the engine queues it).
#[tokio::test]
async fn codex_a_steer_stuck_in_a_stalled_write_is_refused_not_steered() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt =
        codex(dir.path(), "deaf", &recorder, None).with_steer_timeout(Duration::from_millis(300));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("said").exists()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let big = format!("big {}", "x".repeat(400_000));
    assert_eq!(
        rt.steer(&thread, &big, SteerHow::Steer).await.unwrap(),
        SteerOutcome::Rejected
    );
    wait_for(|| recorder.finished().len() == 1).await;
    assert!(
        recorder.unread().is_empty(),
        "its requester queues it, once"
    );
}

/// Sol 3: Muse answers with the turn it put the steer in; another turn's id
/// is a refusal, like T3 Code.
#[tokio::test]
async fn muse_a_steer_acknowledged_for_another_turn_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = muse(dir.path(), "wrongturn", &recorder, None);
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("said").exists()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        rt.steer(&thread, "elsewhere", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Rejected
    );
    rt.cancel(&thread, "stop").await.unwrap();
}

/// Sol 5: two sends while an approval waits, the decline's answer slow
/// (Muse acknowledges the decision a second later). The second send may not
/// overtake the first: both go after the decline, in the order sent. The
/// fake records arrival order.
#[tokio::test]
async fn muse_two_sends_at_a_gate_keep_their_order_behind_the_decline() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(30));
    let _ledger = ledger(dir.path(), &bridge).await;
    let rt = Arc::new(muse(
        dir.path(),
        "gateslow",
        &recorder,
        Some(Arc::clone(&bridge)),
    ));
    let thread = thread(rt.as_ref()).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    wait_gate(&mut events).await;
    let first = {
        let (rt, thread) = (Arc::clone(&rt), thread.clone());
        tokio::spawn(async move { rt.steer(&thread, "alpha", SteerHow::Steer).await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let second = {
        let (rt, thread) = (Arc::clone(&rt), thread.clone());
        tokio::spawn(async move { rt.steer(&thread, "beta", SteerHow::Steer).await.unwrap() })
    };
    assert_eq!(first.await.unwrap(), SteerOutcome::Steered);
    assert_eq!(second.await.unwrap(), SteerOutcome::Steered);
    wait_for(|| recorder.finished().len() == 1).await;
    let log = read(&dir.path().join("muse.log"));
    let at = |needle: &str| {
        log.lines()
            .position(|l| l.contains("turn/steer") && l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} not steered: {log}"))
    };
    let decide = log
        .lines()
        .position(|l| l.contains("approval/decide"))
        .expect("declined");
    assert!(decide < at("alpha") && at("alpha") < at("beta"), "{log}");
}

/// Sol 6: output Codex sent before the steer is folded before the cut, and
/// every message of it is kept, in order. (Repeated: the loop's choice of
/// what to read first must not matter.)
#[tokio::test]
async fn codex_output_sent_before_a_steer_is_cut_whole_and_in_order() {
    for _ in 0..6 {
        let dir = tempfile::tempdir().unwrap();
        let recorder = Arc::new(Recorder::default());
        let rt = codex(dir.path(), "burst", &recorder, None);
        let thread = thread(&rt).await;
        rt.start_turn(&thread, input()).await.unwrap();
        wait_for(|| dir.path().join("starting").exists()).await;
        assert_eq!(
            rt.steer(&thread, "now shorter", SteerHow::Steer)
                .await
                .unwrap(),
            SteerOutcome::Steered
        );
        std::fs::write(dir.path().join("go"), "").unwrap();
        wait_for(|| recorder.finished().len() == 1).await;
        let (rows, _) = rows(&rt, &thread, &recorder.run()).await;
        assert_eq!(
            rows,
            [
                "User:hi",
                "Assistant:First",
                "Assistant:Second",
                "User:now shorter",
                "Assistant:Short"
            ]
        );
    }
}

/// Sol 4: a steer whose prompt frame is stuck in a pipe the agent stopped
/// reading is never reported sent: the write's own bound fails it (the
/// steer is refused) and the turn is torn down at once.
#[tokio::test]
async fn antigravity_a_steer_stuck_in_a_stalled_write_is_refused_and_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt =
        antigravity(dir.path(), "deaf", &recorder).with_write_timeout(Duration::from_millis(500));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("said").exists()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let big = format!("big {}", "x".repeat(400_000));
    let started = std::time::Instant::now();
    assert_eq!(
        rt.steer(&thread, &big, SteerHow::Steer).await.unwrap(),
        SteerOutcome::Rejected
    );
    // Refused on the write's own bound, not after a 30 s wait.
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    wait_for(|| recorder.finished().len() == 1).await;
}

/// Sol 4: Stop lands while the turn writes its prompt's transcript row
/// (held here by a FIFO nobody reads yet): the first prompt is never sent
/// after the cancel.
#[tokio::test]
async fn antigravity_stop_during_the_first_prompts_journal_write_sends_no_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = Arc::new(antigravity(dir.path(), "ok", &recorder));
    let thread = thread(rt.as_ref()).await;
    let key = thread.native_session_id.clone().unwrap();
    rt.start_turn(&thread, input()).await.unwrap();
    // `session/new` is in (the recap read is done) and waits for `go`.
    wait_for(|| dir.path().join("opening").exists()).await;
    let transcripts = dir.path().join("state").join("transcripts");
    std::fs::create_dir_all(&transcripts).unwrap();
    let fifo = transcripts.join(format!("{key}.jsonl"));
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    std::fs::write(dir.path().join("go"), "").unwrap();
    // The turn is now blocked opening the FIFO for its prompt row.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stop = {
        let (rt, thread) = (Arc::clone(&rt), thread.clone());
        tokio::spawn(async move { rt.cancel(&thread, "stop").await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let drain = std::thread::spawn(move || {
        let mut row = String::new();
        std::io::Read::read_to_string(&mut std::fs::File::open(fifo).unwrap(), &mut row).unwrap();
        row
    });
    stop.await.unwrap();
    assert!(drain.join().unwrap().contains("hi"), "the row was written");
    wait_for(|| recorder.finished().len() == 1).await;
    let log = read(&dir.path().join("agy.log"));
    assert!(
        !log.contains("session/prompt"),
        "prompt sent after Stop: {log}"
    );
}

// ── Round 4 (Sol): settlement from evidence, one Stop boundary ────────────

/// Sol 1: a held steer whose write is still pending when the agent closes
/// its output is not settled as written: its report is waited for, the
/// write fails on its bound, and the steer comes back unread.
#[tokio::test]
async fn antigravity_a_held_steer_still_writing_at_eof_comes_back_unread() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = antigravity(dir.path(), "eofstall", &recorder)
        .with_write_timeout(Duration::from_millis(500));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("opening").exists()).await;
    let big = format!("big {}", "x".repeat(400_000));
    assert_eq!(
        rt.steer(&thread, &big, SteerHow::Steer).await.unwrap(),
        SteerOutcome::Steered
    );
    std::fs::write(dir.path().join("go"), "").unwrap();
    wait_for(|| recorder.finished().len() == 1).await;
    assert_eq!(recorder.unread(), [big]);
}

/// Sol 1: a steer's prompt the agent took and then exited without answering
/// is unconfirmed: never replayed, and the transcript says so.
#[tokio::test]
async fn antigravity_a_steer_written_with_no_answer_is_unconfirmed() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = antigravity(dir.path(), "silentexit", &recorder);
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("said").exists()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        steer_as(&rt, &thread, "s-maybe", "maybe read").await,
        SteerOutcome::Steered
    );
    wait_for(|| recorder.finished().len() == 1).await;
    assert!(recorder.unread().is_empty(), "replayed");
    assert_eq!(recorder.unconfirmed(), ["s-maybe"], "the engine's notice");
    let history = rt.history(&thread, 50).await.unwrap();
    assert!(
        history
            .iter()
            .any(|m| m.text == "maybe read" && m.steer_id.as_deref() == Some("s-maybe")),
        "{history:?}"
    );
}

/// Sol 5: two sends while a permission waits, the first one's decline held
/// back (an earlier big steer is stuck in the agent's input, so the
/// decline's write has not reported yet). The second send waits its turn:
/// decline, first prompt, second prompt, in that order on the wire.
#[tokio::test]
async fn antigravity_two_sends_at_a_gate_keep_their_order_behind_the_decline() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(30));
    let _ledger = ledger(dir.path(), &bridge).await;
    let rt = Arc::new(antigravity_with(
        dir.path(),
        "gatetwo",
        &recorder,
        Some(Arc::clone(&bridge)),
    ));
    let thread = thread(rt.as_ref()).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("said").exists()).await;
    let big = format!("big {}", "x".repeat(400_000));
    let stuck = {
        let (rt, thread, big) = (Arc::clone(&rt), thread.clone(), big.clone());
        tokio::spawn(async move { rt.steer(&thread, &big, SteerHow::Steer).await.unwrap() })
    };
    wait_gate(&mut events).await;
    let first = {
        let (rt, thread) = (Arc::clone(&rt), thread.clone());
        tokio::spawn(async move { rt.steer(&thread, "alpha", SteerHow::Steer).await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    let second = {
        let (rt, thread) = (Arc::clone(&rt), thread.clone());
        tokio::spawn(async move { rt.steer(&thread, "beta", SteerHow::Steer).await.unwrap() })
    };
    for steer in [stuck, first, second] {
        assert_eq!(steer.await.unwrap(), SteerOutcome::Steered);
    }
    wait_for(|| recorder.finished().len() == 1).await;
    let log = read(&dir.path().join("agy.log"));
    let at = |needle: &str| {
        log.lines()
            .position(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("{needle} missing"))
    };
    let decline = at(r#""optionId":"no""#);
    assert!(
        decline < at(r#""text":"alpha""#) && at(r#""text":"alpha""#) < at(r#""text":"beta""#),
        "decline {decline}, alpha {}, beta {}",
        at(r#""text":"alpha""#),
        at(r#""text":"beta""#)
    );
}

/// Sol 4 / Rule 2: Stop lands while the turn writes its prompt row (held by
/// a FIFO nobody reads) before `turn/start`: the prompt never goes out, and
/// Stop completes although the row is still stuck (it lands later).
#[tokio::test]
async fn codex_stop_before_turn_start_sends_no_prompt_and_does_not_hang() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = Arc::new(codex(dir.path(), "cut", &recorder, None));
    let thread = thread(rt.as_ref()).await;
    let session = thread.native_session_id.clone().unwrap();
    let transcripts = dir.path().join("state").join("transcripts");
    std::fs::create_dir_all(&transcripts).unwrap();
    let fifo = transcripts.join(format!("{session}.jsonl"));
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| read(&dir.path().join("codex.log")).contains("thread/")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let stopped = tokio::time::timeout(Duration::from_secs(15), rt.cancel(&thread, "stop")).await;
    assert!(stopped.is_ok(), "Stop hung on a stuck journal write");
    wait_for(|| recorder.finished().len() == 1).await;
    assert!(
        !read(&dir.path().join("codex.log")).contains("turn/start"),
        "prompt sent after Stop"
    );
    // The stuck row is kept and lands once the file takes it (the reader
    // keeps draining, so every later row lands too).
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let mut chunk = String::new();
            let Ok(mut file) = std::fs::File::open(&fifo) else {
                return;
            };
            if std::io::Read::read_to_string(&mut file, &mut chunk).is_err()
                || tx.send(chunk).is_err()
            {
                return;
            }
        }
    });
    let mut row = String::new();
    for _ in 0..200 {
        if let Ok(chunk) = rx.try_recv() {
            row.push_str(&chunk);
            break;
        }
        // Async wait: the journal writer runs on this test's runtime.
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(row.contains("hi"), "the row lands: {row:?}");
}
