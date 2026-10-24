//! The Antigravity (ACP) and Muse Code (MSP) drivers against fake agent
//! scripts: session create then resume, fail-closed permissions, outcome,
//! transcript and telemetry folding, steering, and Stop ending the whole
//! process group. No real model is called.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_cli_providers::antigravity::{AntigravityConfig, AntigravityRuntime};
use ironclaw_cli_providers::bridge::{CliBridge, DEFAULT_APPROVAL_TIMEOUT};
use ironclaw_cli_providers::muse::{MuseConfig, MuseRuntime};
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use ironclaw_orchestrator::model::{Millis, ThreadRecord, ThreadScope};
use ironclaw_orchestrator::runtime::*;
use ironclaw_orchestrator::telemetry::TurnTelemetry;

#[derive(Default)]
struct Recorder {
    finished: Mutex<Vec<TurnOutcome>>,
    telemetry: Mutex<Vec<TurnTelemetry>>,
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
    async fn turn_telemetry(&self, _: &ThreadId, telemetry: TurnTelemetry, _: Millis) {
        self.telemetry.lock().unwrap().push(telemetry);
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

async fn thread(rt: &dyn TurnRuntime, model: &str) -> ThreadRecord {
    let created = rt
        .create_thread(CreateThread {
            scope: scope(),
            title: None,
            provider: None,
            model: Some(model.into()),
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

fn alive(pid_file: &Path) -> bool {
    let pid = std::fs::read_to_string(pid_file)
        .unwrap()
        .trim()
        .to_string();
    std::process::Command::new("kill")
        .args(["-0", &pid])
        .status()
        .unwrap()
        .success()
}

// ── Antigravity ─────────────────────────────────────────────────────────────

const AGY: &str = r#"#!/bin/sh
LOG=__LOG__
PIDF='__PID__'
id_of() { printf '%s' "$1" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p'; }
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
note() { printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"acp-1","update":%s}}\n' "$1"; }
OPTS='[{"id":"model","type":"select","currentValue":"gemini-a","options":[{"value":"gemini-a","name":"A"},{"value":"gemini-b","name":"B"}]},{"id":"mode","category":"mode","type":"select","currentValue":"default","options":[]}]'
printf 'env GEMINI_HOME=%s BROWSER=%s KEY=%s\n' "$GEMINI_HOME" "$BROWSER" "${GEMINI_API_KEY:-none}" >> "$LOG"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$LOG"
  id=$(id_of "$line")
  case "$line" in
    *'"method":"initialize"'*) reply "$id" '{"protocolVersion":1,"agentCapabilities":{}}' ;;
    *'"method":"authenticate"'*) reply "$id" '{}' ;;
    *'"method":"session/new"'*) reply "$id" "{\"sessionId\":\"acp-1\",\"configOptions\":$OPTS}" ;;
    *'"method":"session/resume"'*) reply "$id" "{\"configOptions\":$OPTS}" ;;
    *'"method":"session/set_config_option"'*) reply "$id" '{}' ;;
    *'"method":"session/prompt"'*)
      if [ -n "$PIDF" ]; then sleep 30 & printf '%s' $! > "$PIDF"; wait; fi
      note '{"sessionUpdate":"usage_update","used":1200,"size":100000}'
      note '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hel"}}'
      printf '%s\n' '{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{"sessionId":"acp-1","toolCall":{"toolCallId":"call_1","title":"rm"},"options":[{"optionId":"yes","kind":"allow_once","name":"Allow"},{"optionId":"no","kind":"reject_once","name":"Reject"}]}}'
      IFS= read -r answer; printf '%s\n' "$answer" >> "$LOG"
      sleep 0.5
      note '{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"lo"}}'
      reply "$id" '{"stopReason":"end_turn","usage":{"inputTokens":1000,"outputTokens":50,"cachedReadTokens":600,"totalTokens":1050}}' ;;
  esac
done
"#;

fn antigravity(
    dir: &Path,
    pid_file: Option<&Path>,
    recorder: &Arc<Recorder>,
) -> AntigravityRuntime {
    let body = AGY
        .replace("__LOG__", &dir.join("agy.log").display().to_string())
        .replace(
            "__PID__",
            &pid_file
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
    AntigravityRuntime::new(
        AntigravityConfig {
            runtime_id: "antigravity".into(),
            display_name: "Antigravity".into(),
            executable: script(dir, "agy_acp_server", &body),
            harness: dir.join("localharness_external"),
            profile_dir: dir.join("profile"),
            auth_method: "oauth-personal".into(),
            auth_timeout: Duration::from_secs(5),
            work_dir: dir.join("work"),
            state_dir: dir.join("state"),
        },
        sink(recorder),
        None,
    )
    .unwrap()
}

#[tokio::test]
async fn antigravity_resumes_its_session_denies_permissions_and_reports_telemetry() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = antigravity(dir.path(), None, &recorder);

    let catalog = rt.catalog(&scope()).await.unwrap();
    assert!(!catalog[0].authenticated, "no token file yet");
    std::fs::create_dir_all(dir.path().join("profile/antigravity-acp")).unwrap();
    std::fs::write(
        dir.path().join("profile/antigravity-acp/acp_token.json"),
        "{}",
    )
    .unwrap();

    // SAFETY: set before this test's first spawn; only the fake agent reads it.
    unsafe { std::env::set_var("GEMINI_API_KEY", "ambient-secret") };
    let thread = thread(&rt, "gemini-b").await;
    for n in 1..=2 {
        rt.start_turn(&thread, input()).await.unwrap();
        wait_for(|| recorder.finished.lock().unwrap().len() == n).await;
    }

    let log = std::fs::read_to_string(dir.path().join("agy.log")).unwrap();
    assert_eq!(log.matches(r#""method":"session/new""#).count(), 1, "{log}");
    assert!(
        log.contains(r#""method":"session/resume","params":{"cwd""#)
            && log.contains(r#""sessionId":"acp-1""#)
    );
    assert!(log.contains(r#""configId":"model""#) && log.contains(r#""value":"gemini-b""#));
    // Fail closed: no file system or terminal offered, every permission rejected.
    assert!(log.contains(r#""fs":{"readTextFile":false,"writeTextFile":false},"terminal":false"#));
    assert_eq!(log.matches(r#""optionId":"no""#).count(), 2, "{log}");
    assert!(!log.contains(r#""optionId":"yes""#));
    // Only allowlisted variables are inherited: the ambient key never arrives.
    assert!(log.contains("BROWSER=/usr/bin/true KEY=none"), "{log}");

    let outcome = recorder.finished.lock().unwrap()[1].clone();
    assert_eq!(outcome.status, TurnEndStatus::Completed, "{outcome:?}");
    assert_eq!(outcome.final_text.as_deref(), Some("Hello"));
    let usage = outcome.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.cache_read_tokens), (400, 600));

    let t = recorder.telemetry.lock().unwrap()[1].clone();
    assert_eq!(t.provider.as_deref(), Some("antigravity"));
    assert_eq!((t.context_used, t.context_max), (Some(1200), Some(100_000)));
    assert_eq!(t.cache_hit_rate, Some(0.6));
    let tps = t.tps.unwrap();
    assert!((40.0..=110.0).contains(&tps), "tps {tps}");

    let history = rt.history(&thread, 10).await.unwrap();
    let roles: Vec<HistoryRole> = history.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            HistoryRole::User,
            HistoryRole::Assistant,
            HistoryRole::User,
            HistoryRole::Assistant
        ]
    );
    let catalog = rt.catalog(&scope()).await.unwrap();
    assert!(catalog[0].authenticated);
    assert_eq!(catalog[0].models, ["gemini-a", "gemini-b"]);
}

#[tokio::test]
async fn antigravity_stop_kills_the_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let pid_file = dir.path().join("child.pid");
    let rt = antigravity(dir.path(), Some(&pid_file), &recorder);
    let thread = thread(&rt, "default").await;

    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| std::fs::read_to_string(&pid_file).is_ok_and(|s| !s.is_empty())).await;
    assert_eq!(
        rt.start_turn(&thread, input()).await.unwrap(),
        TurnAdmission::Busy
    );

    rt.cancel(&thread, "stop").await.unwrap();
    assert_eq!(
        recorder.finished.lock().unwrap()[0].status,
        TurnEndStatus::Cancelled
    );
    let log = std::fs::read_to_string(dir.path().join("agy.log")).unwrap();
    assert!(
        !log.contains("session/set_config_option"),
        "default model keeps the agent's choice"
    );
    assert!(!alive(&pid_file), "tool subprocess survived Stop");
}

// ── Muse Code ───────────────────────────────────────────────────────────────

const MUSE: &str = r#"#!/bin/sh
LOG=__LOG__
PIDF='__PID__'
id_of() { printf '%s' "$1" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p'; }
field() { printf '%s' "$1" | sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p"; }
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
emit() { printf '{"jsonrpc":"2.0","method":"%s","params":%s}\n' "$1" "$2"; }
printf 'argv %s\n' "$*" >> "$LOG"
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$LOG"
  id=$(id_of "$line")
  case "$line" in
    *'"method":"initialize"'*) reply "$id" '{"schema":{"version":1}}' ;;
    *'"method":"initialized"'*) ;;
    *'"method":"usage/read"'*) reply "$id" '{"usage":{"window":{"usedPercent":42}}}' ;;
    *'"method":"model/list"'*) reply "$id" '{"providerId":"meta","models":[{"modelId":"muse-wonder-1.0","providerId":"meta","contextLimit":1007997,"isDefault":false},{"modelId":"muse-spark-1.3","providerId":"meta","contextLimit":1007997,"isDefault":true}]}' ;;
    *'"method":"session/start"'*|*'"method":"session/resume"'*)
      SID=$(field "$line" sessionId)
      reply "$id" "{\"session\":{\"sessionId\":\"$SID\",\"modelId\":\"muse-spark-1.3\"}}" ;;
    *'"method":"session/setApprovalMode"'*|*'"method":"session/setModel"'*) reply "$id" '{}' ;;
    *'"method":"approval/decide"'*)
      reply "$id" '{}'
      # Like the real host, the turn goes on once the approval is decided.
      emit session/tokenUsage "{\"sessionId\":\"$SID\",\"turnId\":\"$TID\",\"modelId\":\"muse-spark-1.3\",\"promptTokens\":1000,\"totalTokens\":1050,\"durationMs\":500,\"usage\":{\"inputTokens\":1000,\"outputTokens\":50,\"cachedTokens\":600,\"cacheReadTokens\":600,\"reasoningTokens\":0}}"
      emit session/contextUsage "{\"sessionId\":\"$SID\",\"usedTokens\":1050,\"windowTokens\":1007997,\"pressure\":\"normal\"}"
      emit item/completed "{\"sessionId\":\"$SID\",\"item\":{\"itemId\":\"m1\",\"turnId\":\"$TID\",\"kind\":\"agentMessage\",\"status\":\"completed\",\"revision\":2,\"text\":\"Hello\"}}"
      emit usage/changed '{"window":{"usedPercent":55}}'
      emit turn/completed "{\"sessionId\":\"$SID\",\"turnId\":\"$TID\",\"terminal\":\"completed\"}" ;;
    *'"method":"turn/steer"'*) reply "$id" "{\"turnId\":\"$TID\"}" ;;
    *'"method":"turn/interrupt"'*)
      reply "$id" '{}'
      emit turn/completed "{\"sessionId\":\"$SID\",\"turnId\":\"$TID\",\"terminal\":\"cancelled\"}" ;;
    *'"method":"turn/start"'*)
      TID=$(field "$line" commandId)
      reply "$id" "{\"turnId\":\"$TID\",\"disposition\":\"started\"}"
      if [ -n "$PIDF" ]; then sleep 30 & printf '%s' $! > "$PIDF"; continue; fi
      emit turn/started "{\"sessionId\":\"$SID\",\"turnId\":\"$TID\"}"
      emit item/started "{\"sessionId\":\"$SID\",\"item\":{\"itemId\":\"m1\",\"turnId\":\"$TID\",\"kind\":\"agentMessage\",\"status\":\"inProgress\",\"revision\":1}}"
      emit item/delta "{\"sessionId\":\"$SID\",\"itemId\":\"m1\",\"field\":\"text\",\"delta\":\"Hel\"}"
      emit approval/requested "{\"sessionId\":\"$SID\",\"approvalId\":\"a1\",\"turnId\":\"$TID\",\"status\":\"pending\",\"toolName\":\"bash\",\"currentRequirementId\":\"r1\",\"subject\":{\"command\":\"rm -rf /\"},\"availableChoices\":[{\"choiceId\":\"ok\",\"decision\":\"approved\",\"scope\":\"once\"},{\"choiceId\":\"stop\",\"decision\":\"abort\"}]}" ;;
  esac
done
"#;

fn muse(
    dir: &Path,
    pid_file: Option<&Path>,
    recorder: &Arc<Recorder>,
    bridge: Option<Arc<CliBridge>>,
) -> MuseRuntime {
    let body = MUSE
        .replace("__LOG__", &dir.join("muse.log").display().to_string())
        .replace(
            "__PID__",
            &pid_file
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
    MuseRuntime::new(
        MuseConfig {
            runtime_id: "muse-code".into(),
            display_name: "Muse Code".into(),
            binary: script(dir, "muse", &body),
            work_dir: dir.join("work"),
            state_dir: dir.join("state"),
            usage_file: Some(dir.join("usage").join("muse-code.json")),
            reasoning_effort: None,
        },
        sink(recorder),
        bridge,
    )
    .unwrap()
}

#[tokio::test]
async fn muse_starts_then_resumes_denies_approvals_and_reports_telemetry() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let rt = muse(dir.path(), None, &recorder, None);

    let catalog = rt.catalog(&scope()).await.unwrap();
    assert!(catalog[0].authenticated);
    assert_eq!(catalog[0].models, ["muse-spark-1.3", "muse-wonder-1.0"]);

    let thread = thread(&rt, "muse-wonder-1.0").await;
    let session = thread.native_session_id.clone().unwrap();
    for n in 1..=2 {
        rt.start_turn(&thread, input()).await.unwrap();
        wait_for(|| recorder.finished.lock().unwrap().len() == n).await;
    }

    let log = std::fs::read_to_string(dir.path().join("muse.log")).unwrap();
    for argv in log.lines().filter(|l| l.starts_with("argv")) {
        assert!(
            argv.contains("--disable-shell")
                && argv.contains("--disable-write")
                && argv.contains("--disable-request-user-input"),
            "{argv}"
        );
        assert!(
            !argv.contains("trust-workspace") && !argv.contains("disable-sandbox"),
            "{argv}"
        );
    }
    assert_eq!(
        log.matches(r#""method":"session/start""#).count(),
        1,
        "{log}"
    );
    assert_eq!(
        log.matches(r#""method":"session/resume""#).count(),
        1,
        "{log}"
    );
    assert!(log.contains(&format!(r#""sessionId":"{session}""#)));
    assert!(log.contains(r#""approvalMode":"denyUnmatched""#));
    assert!(log.contains(r#""mode":"denyUnmatched""#));
    assert!(log.contains(r#""modelId":"muse-wonder-1.0""#));
    // The approval is answered, with the abort choice, never "ok".
    assert_eq!(log.matches(r#""choiceId":"stop""#).count(), 2, "{log}");
    assert!(!log.contains(r#""choiceId":"ok""#));

    let outcome = recorder.finished.lock().unwrap()[1].clone();
    assert_eq!(outcome.status, TurnEndStatus::Completed, "{outcome:?}");
    assert_eq!(outcome.final_text.as_deref(), Some("Hello"));
    let usage = outcome.usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_read_tokens,
            usage.output_tokens
        ),
        (400, 600, 50)
    );

    let t = recorder.telemetry.lock().unwrap()[1].clone();
    assert_eq!(t.provider.as_deref(), Some("muse-code"));
    assert_eq!(t.model.as_deref(), Some("muse-spark-1.3"));
    assert_eq!(
        (t.context_used, t.context_max),
        (Some(1050), Some(1_007_997))
    );
    assert_eq!(t.cache_hit_rate, Some(0.6));
    assert_eq!(t.tps, Some(100.0));

    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("usage").join("muse-code.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["usage"]["window"]["usedPercent"], 55);

    let history = rt.history(&thread, 10).await.unwrap();
    assert_eq!(history.len(), 4);
    assert_eq!(history[3].text, "Hello");
}

/// With the bridge, shell and writes are on and Muse's approval waits as
/// a gate in the thread; the owner's approval picks the once-scoped allow.
#[tokio::test]
async fn muse_with_a_bridge_asks_the_owner_and_runs_the_approved_call() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
    let rt = muse(dir.path(), None, &recorder, Some(Arc::clone(&bridge)));
    let thread = thread(&rt, "default").await;
    rt.start_turn(&thread, input()).await.unwrap();

    let gate = loop {
        if let Some(g) = bridge
            .snapshot(&thread.thread_id)
            .and_then(|s| s.gates.first().cloned())
        {
            break g;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(gate.tool_name, "bash");
    assert_eq!(gate.body.as_deref(), Some("rm -rf /"));
    assert!(bridge.resolve_gate(&thread.thread_id, &gate.gate_ref, true));
    wait_for(|| recorder.finished.lock().unwrap().len() == 1).await;

    let log = std::fs::read_to_string(dir.path().join("muse.log")).unwrap();
    let argv = log.lines().find(|l| l.starts_with("argv")).unwrap();
    assert!(
        !argv.contains("--disable-shell") && !argv.contains("--disable-write"),
        "{argv}"
    );
    assert!(argv.contains("--disable-request-user-input"), "{argv}");
    assert!(log.contains(r#""choiceId":"ok""#), "{log}");
    assert!(!log.contains(r#""choiceId":"stop""#), "{log}");
    assert_eq!(
        recorder.finished.lock().unwrap()[0].status,
        TurnEndStatus::Completed
    );
    assert!(bridge.snapshot(&thread.thread_id).is_none(), "turn ended");
}

#[tokio::test]
async fn muse_steers_in_turn_and_stop_interrupts_and_clears_the_group() {
    let dir = tempfile::tempdir().unwrap();
    let recorder = Arc::new(Recorder::default());
    let pid_file = dir.path().join("child.pid");
    let rt = muse(dir.path(), Some(&pid_file), &recorder, None);
    let thread = thread(&rt, "default").await;

    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| std::fs::read_to_string(&pid_file).is_ok_and(|s| !s.is_empty())).await;
    let steered = rt
        .steer(&thread, "also this", SteerHow::Steer)
        .await
        .unwrap();
    assert_eq!(steered, SteerOutcome::Steered);

    rt.cancel(&thread, "stop").await.unwrap();
    assert_eq!(
        recorder.finished.lock().unwrap()[0].status,
        TurnEndStatus::Cancelled
    );
    let log = std::fs::read_to_string(dir.path().join("muse.log")).unwrap();
    assert!(
        log.contains(r#""expectedTurnId":"#) && log.contains("also this"),
        "{log}"
    );
    assert!(log.contains(r#""method":"turn/interrupt""#));
    assert!(
        !log.contains("session/setModel"),
        "default model keeps Muse's choice"
    );
    assert!(!alive(&pid_file), "tool subprocess survived Stop");
    let history = rt.history(&thread, 10).await.unwrap();
    assert_eq!(
        history.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
        ["hi", "also this"]
    );
}
