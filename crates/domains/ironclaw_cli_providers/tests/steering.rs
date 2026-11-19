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
use ironclaw_cli_providers::bridge::{CliBridge, LiveEvent};
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
}

impl Recorder {
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
/// - `gate`: an approval waits; the steer ends the turn.
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
    *'"decision":'*) ;;
    *'"method":"initialize"'*) reply "$id" '{"userAgent":"codex"}' ;;
    *'"method":"initialized"'*) ;;
    *'"method":"thread/start"'*|*'"method":"thread/resume"'*) reply "$id" '{"thread":{"id":"cx-1"},"model":"gpt-b"}' ;;
    *'"method":"turn/start"'*)
      touch "$DIR/starting"
      if [ "$MODE" = early ] || [ "$MODE" = never ]; then
        while [ ! -e "$DIR/go" ]; do sleep 0.05; done
      fi
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
      if [ "$MODE" = gate ]; then
        printf '%s\n' '{"jsonrpc":"2.0","id":99,"method":"item/commandExecution/requestApproval","params":{"threadId":"cx-1","turnId":"tu-1","itemId":"c1","command":"rm -rf build","kind":"command","startedAtMs":0}}'
      fi
      touch "$DIR/said" ;;
    *'"method":"turn/steer"'*)
      STEERS=$((STEERS+1))
      case "$MODE" in
        slow) touch "$DIR/steered"; while [ ! -e "$DIR/end" ]; do sleep 0.05; done; said m2 "Done"; done_turn ;;
        slowend) done_turn ;;
        early) reply "$id" '{"turnId":"tu-1"}'; if [ $STEERS -eq 2 ]; then said m2 "Done"; done_turn; fi ;;
        cut) reply "$id" '{"turnId":"tu-1"}'; sleep 0.3; said m2 "Short"; done_turn ;;
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
    // After the turn: refused, the engine queues it.
    assert_eq!(
        rt.steer(&thread, "late", SteerHow::Steer).await.unwrap(),
        SteerOutcome::Rejected
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
        rt.steer(&thread, "slow answer", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Steered
    );
    std::fs::write(dir.path().join("end"), "").unwrap();
    wait_for(|| recorder.finished().len() == 1).await;
    assert!(recorder.unread().is_empty());
    let (rows, _) = rows(&rt, &thread, &recorder.run()).await;
    assert_eq!(rows, ["User:hi", "User:slow answer", "Assistant:Done"]);
}

/// The same unanswered `turn/steer`, but its turn ends first: refused, so
/// the engine queues it, and it is not journaled into the ended run.
#[tokio::test]
async fn codex_an_unanswered_steer_whose_turn_ended_is_refused() {
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
        rt.steer(&thread, "too late", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Rejected
    );
    let (rows, _) = rows(&rt, &thread, &recorder.run()).await;
    assert!(!rows.iter().any(|r| r.contains("too late")), "{rows:?}");
}

/// A steer at a pending approval declines it first (Codex's reply carries
/// no message), then steers the words in.
#[tokio::test]
async fn codex_a_steer_at_a_pending_approval_declines_it_then_steers() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
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
/// ends the turn; `gate` asks for approval, and a steer ends the turn.
const MUSE: &str = r#"#!/bin/sh
LOG=__DIR__/muse.log
DIR=__DIR__
MODE=__MODE__
id_of() { printf '%s' "$1" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p'; }
field() { printf '%s' "$1" | sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p"; }
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
emit() { printf '{"jsonrpc":"2.0","method":"%s","params":%s}\n' "$1" "$2"; }
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
    *'"method":"approval/decide"'*) reply "$id" '{}' ;;
    *'"method":"turn/steer"'*)
      reply "$id" "{\"turnId\":\"$TID\"}"
      emit item/completed "{\"sessionId\":\"$SID\",\"item\":{\"itemId\":\"m1\",\"turnId\":\"$TID\",\"kind\":\"agentMessage\",\"status\":\"completed\",\"revision\":2,\"text\":\"Done\"}}"
      emit turn/completed "{\"sessionId\":\"$SID\",\"turnId\":\"$TID\",\"terminal\":\"completed\"}" ;;
    *'"method":"turn/start"'*)
      TID=$(field "$line" commandId)
      touch "$DIR/starting"
      if [ "$MODE" = early ]; then while [ ! -e "$DIR/go" ]; do sleep 0.05; done; fi
      reply "$id" "{\"turnId\":\"$TID\",\"disposition\":\"started\"}"
      emit turn/started "{\"sessionId\":\"$SID\",\"turnId\":\"$TID\"}"
      if [ "$MODE" = gate ]; then
        emit approval/requested "{\"sessionId\":\"$SID\",\"approvalId\":\"a1\",\"turnId\":\"$TID\",\"status\":\"pending\",\"toolName\":\"bash\",\"currentRequirementId\":\"r1\",\"subject\":{\"command\":\"rm -rf build\"},\"availableChoices\":[{\"choiceId\":\"ok\",\"decision\":\"approved\",\"scope\":\"once\"},{\"choiceId\":\"no\",\"decision\":\"denied\",\"scope\":\"once\"}]}"
      fi ;;
  esac
done
"#;

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
      [ "$MODE" = perm ] || while [ ! -e "$DIR/go" ]; do sleep 0.05; done
      if [ "$MODE" = fail ]; then printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32603,"message":"no session"}}\n' "$id"
      else reply "$id" "{\"sessionId\":\"acp-1\",\"configOptions\":$OPTS}"; fi ;;
    *'"method":"session/set_config_option"'*) reply "$id" '{}' ;;
    *'"method":"session/prompt"'*)
      if [ -n "$P1" ]; then
        reply "$P1" '{"stopReason":"end_turn"}'
        chunk "Steered"
        reply "$id" '{"stopReason":"end_turn","usage":{"inputTokens":10,"outputTokens":2}}'
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
