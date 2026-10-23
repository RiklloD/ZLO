//! The Claude driver against a fake `claude` script: arguments (fail-closed
//! tool flags, session start then resume), stream folding into outcome,
//! usage and telemetry, and Stop killing the whole process group.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_cli_providers::claude::{ClaudeCliConfig, ClaudeCliRuntime};
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use ironclaw_orchestrator::model::{Millis, RuntimeId, ThreadRecord, ThreadScope};
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

const ANSWER: &str = r#"#!/bin/sh
echo "$@" >> "$ARGS_LOG"
read -r _line
mkdir -p "$TRANSCRIPT_DIR" && echo '{"type":"user","message":{"role":"user","content":"hi"}}' > "$TRANSCRIPT_DIR/$SESSION.jsonl"
echo '{"type":"system","subtype":"init","cwd":"/w","session_id":"s","tools":[],"mcp_servers":[],"model":"claude-haiku-5-5","permissionMode":"default"}'
echo '{"type":"stream_event","event":{"type":"message_start","message":{"model":"claude-haiku-5-5","usage":{"input_tokens":10,"cache_read_input_tokens":600,"cache_creation_input_tokens":390,"output_tokens":1}}},"session_id":"s","uuid":"u1"}'
echo '{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","unifiedWindows":{"five_hour":{"utilization":0.42,"resetsAt":1900000000}}},"session_id":"s"}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}},"session_id":"s","uuid":"u2"}'
sleep 0.5
echo '{"type":"stream_event","event":{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":50}},"session_id":"s","uuid":"u3"}'
echo '{"type":"stream_event","event":{"type":"message_stop"},"session_id":"s","uuid":"u4"}'
echo '{"type":"assistant","message":{"id":"m","type":"message","role":"assistant","model":"claude-haiku-5-5","content":[{"type":"text","text":"Hello"}],"usage":{"input_tokens":10,"cache_read_input_tokens":600,"cache_creation_input_tokens":390,"output_tokens":50}},"session_id":"s","uuid":"u5"}'
echo '{"type":"result","subtype":"success","is_error":false,"duration_ms":600,"duration_api_ms":500,"num_turns":1,"result":"Hello","session_id":"s","total_cost_usd":0.01,"usage":{"input_tokens":10,"cache_read_input_tokens":600,"cache_creation_input_tokens":390,"output_tokens":50},"modelUsage":{"claude-haiku-5-5":{"inputTokens":10,"outputTokens":50,"cacheReadInputTokens":600,"cacheCreationInputTokens":390,"webSearchRequests":0,"costUSD":0.01,"contextWindow":200000,"maxOutputTokens":32000}}}'
"#;

const HANG: &str = r#"#!/bin/sh
read -r _line
sleep 30 &
echo $! > "$CHILD_PID"
wait
"#;

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn runtime(dir: &Path, binary: PathBuf, sink: &Arc<Recorder>) -> ClaudeCliRuntime {
    let work_dir = dir.join("work");
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(sink) as Weak<dyn TurnEventSink>;
    ClaudeCliRuntime::new(
        ClaudeCliConfig {
            runtime_id: "claude-cli".into(),
            display_name: "Claude CLI".into(),
            binary,
            config_dir: None,
            work_dir,
            login_file: dir.join(".claude.json"),
            projects_dir: dir.join("projects"),
            usage_file: Some(dir.join("usage").join("claude-cli.json")),
        },
        sink,
    )
    .unwrap()
}

async fn thread(rt: &ClaudeCliRuntime) -> ThreadRecord {
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
            model: Some("claude-haiku-5-5".into()),
            idempotency_key: "k".into(),
        })
        .await
        .unwrap();
    let mut record = ThreadRecord::new(
        created.thread_id,
        scope,
        RuntimeId::new("claude-cli").unwrap(),
    );
    record.native_session_id = created.native_session_id;
    record.model = created.model;
    record
}

fn input() -> TurnInput {
    TurnInput {
        text: "hi".into(),
        origin: TurnOrigin::DelegatedBrief,
        model: None,
    }
}

async fn wait_for(cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("condition not reached");
}

#[tokio::test]
async fn turns_start_then_resume_fail_closed_and_report_telemetry() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let rt = runtime(dir.path(), script(dir.path(), "claude", ANSWER), &sink);
    let thread = thread(&rt).await;
    let session = thread.native_session_id.clone().unwrap();
    let args_log = dir.path().join("args.log");
    let transcript_dir =
        dir.path()
            .join("projects")
            .join(claude_codes::transcript::encode_project_dir(
                &dir.path().join("work"),
            ));
    // SAFETY: only this test reads these variables, and they are set before its first spawn.
    unsafe {
        std::env::set_var("ARGS_LOG", &args_log);
        std::env::set_var("TRANSCRIPT_DIR", &transcript_dir);
        std::env::set_var("SESSION", &session);
    }

    for n in 1..=2 {
        let admission = rt.start_turn(&thread, input()).await.unwrap();
        assert!(matches!(
            admission,
            TurnAdmission::Started { run_id: Some(_) }
        ));
        wait_for(|| sink.finished.lock().unwrap().len() == n).await;
    }

    let args = std::fs::read_to_string(&args_log).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    assert!(
        lines[0].contains(&format!("--session-id {session}")),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].contains(&format!("--resume {session}")),
        "{}",
        lines[1]
    );
    for line in &lines {
        assert!(line.contains("--tools  --strict-mcp-config"), "{line}");
        assert!(line.contains("--permission-prompts none"), "{line}");
        assert!(line.contains("--model claude-haiku-5-5"), "{line}");
        assert!(
            !line.contains("dangerously") && !line.contains("allowed"),
            "{line}"
        );
    }

    let outcome = sink.finished.lock().unwrap()[1].clone();
    assert_eq!(outcome.status, TurnEndStatus::Completed);
    assert_eq!(outcome.final_text.as_deref(), Some("Hello"));
    let usage = outcome.usage.unwrap();
    assert_eq!(
        (usage.output_tokens, usage.context_window),
        (50, Some(200_000))
    );

    let t = sink.telemetry.lock().unwrap()[1].clone();
    assert_eq!(t.provider.as_deref(), Some("claude-cli"));
    assert_eq!(t.context_used, Some(1000));
    assert_eq!(t.context_max, Some(200_000));
    assert_eq!(t.context_pct(), Some(0.5));
    assert_eq!(t.cache_hit_rate, Some(0.6));
    // 50 tokens over ~0.5 s of streaming.
    let tps = t.tps.unwrap();
    assert!((40.0..=110.0).contains(&tps), "tps {tps}");

    // The CLI's own limit reading is kept for the Usage page.
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.path().join("usage").join("claude-cli.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["windows"]["five_hour"]["utilization"], 0.42);

    let history = rt.history(&thread, 10).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].role, HistoryRole::User);
}

#[tokio::test]
async fn stop_kills_the_process_group_and_reports_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let pid_file = dir.path().join("child.pid");
    let body = HANG.replace("$CHILD_PID", &pid_file.display().to_string());
    let rt = runtime(dir.path(), script(dir.path(), "claude", &body), &sink);
    let thread = thread(&rt).await;

    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| pid_file.is_file() && !std::fs::read_to_string(&pid_file).unwrap().is_empty())
        .await;
    assert_eq!(
        rt.start_turn(&thread, input()).await.unwrap(),
        TurnAdmission::Busy
    );

    rt.cancel(&thread, "stop").await.unwrap();
    let outcome = sink.finished.lock().unwrap()[0].clone();
    assert_eq!(outcome.status, TurnEndStatus::Cancelled);
    let grandchild = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .to_string();
    let alive = std::process::Command::new("kill")
        .args(["-0", &grandchild])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "tool subprocess {grandchild} survived Stop");
}
