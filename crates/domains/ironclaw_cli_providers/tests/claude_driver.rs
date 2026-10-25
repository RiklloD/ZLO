//! The Claude driver against a fake `claude` script: arguments (fail-closed
//! tool flags, session start then resume), stream folding into outcome,
//! usage and telemetry, Stop killing the whole process group, and the
//! approval bridge (hook registration, allow and timeout-deny answers, live
//! text, the per-turn MCP token).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_cli_providers::bridge::{CliBridge, LiveEvent, SessionGrant};
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
    bridged_runtime(dir, binary, sink, None)
}

fn bridged_runtime(
    dir: &Path,
    binary: PathBuf,
    sink: &Arc<Recorder>,
    bridge: Option<Arc<CliBridge>>,
) -> ClaudeCliRuntime {
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
        bridge,
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
        goal: None,
        effort: None,
        profile: None,
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
        // The second turn carries the thread's goal.
        let mut turn = input();
        turn.goal = (n == 2).then(|| "Ship stage 5".to_string());
        let admission = rt.start_turn(&thread, turn).await.unwrap();
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
    assert!(!lines[0].contains("--append-system-prompt"), "{}", lines[0]);
    assert!(
        lines[1].contains("--append-system-prompt") && lines[1].contains("Ship stage 5"),
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

/// The profile reaches the CLI: the session runs in the profile folder with
/// SOUL.md and the goal in its system prompt and the skills as a plugin; a
/// Stop restart keeps it; a session that began in the shared folder stays
/// there; history finds a transcript in either folder.
#[tokio::test]
async fn profile_soul_skills_and_folder_reach_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("profile-args.log");
    let body = ANSWER
        .replace(
            "echo \"$@\" >> \"$ARGS_LOG\"",
            "{ printf 'PWD=%s ' \"$PWD\"; echo \"$@\" | tr '\\n' ' '; echo; } >> @LOG@",
        )
        .replace(
            "mkdir -p \"$TRANSCRIPT_DIR\" && echo '{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}' > \"$TRANSCRIPT_DIR/$SESSION.jsonl\"\n",
            "",
        )
        .replace("@LOG@", &log.display().to_string());
    let sink = Arc::new(Recorder::default());
    let rt = runtime(dir.path(), script(dir.path(), "claude", &body), &sink);
    let folder = dir.path().join("profile-folder");
    std::fs::create_dir_all(&folder).unwrap();
    let plugin = dir.path().join("cli-skills").join("builder");
    let profile = TurnProfile {
        soul: "You are the builder.".into(),
        folder: Some(folder.clone()),
        skills: Some(ProfileSkills {
            plugin_dir: plugin.clone(),
            index: String::new(),
        }),
    };

    let fresh = thread(&rt).await;
    let mut turn = input();
    turn.goal = Some("Ship it".into());
    turn.profile = Some(profile.clone());
    rt.start_turn(&fresh, turn).await.unwrap();
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    rt.steer(&fresh, "again", SteerHow::Restart).await.unwrap();
    wait_for(|| sink.finished.lock().unwrap().len() == 2).await;

    // A session already started in the shared work dir keeps running there.
    let legacy = thread(&rt).await;
    let session = legacy.native_session_id.clone().unwrap();
    let shared = dir
        .path()
        .join("projects")
        .join(claude_codes::transcript::encode_project_dir(
            &dir.path().join("work"),
        ));
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join(format!("{session}.jsonl")), "").unwrap();
    let mut turn = input();
    turn.profile = Some(profile);
    rt.start_turn(&legacy, turn).await.unwrap();
    wait_for(|| sink.finished.lock().unwrap().len() == 3).await;

    let args = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    let in_folder = format!("PWD={} ", folder.canonicalize().unwrap().display());
    let plugin_arg = format!("--plugin-dir {}", plugin.display());
    for line in &lines[..2] {
        assert!(line.starts_with(&in_folder), "{line}");
        assert!(line.contains("You are the builder."), "{line}");
        assert!(line.contains(&plugin_arg), "{line}");
    }
    assert!(lines[0].contains("Ship it"), "{}", lines[0]);
    assert!(
        lines[2].starts_with(&format!(
            "PWD={} ",
            dir.path().join("work").canonicalize().unwrap().display()
        )),
        "{}",
        lines[2]
    );
    assert!(
        lines[2].contains(&format!("--resume {session}")),
        "{}",
        lines[2]
    );

    // History reads a transcript filed under the profile folder's project.
    let fresh_session = fresh.native_session_id.clone().unwrap();
    let theirs = dir
        .path()
        .join("projects")
        .join(claude_codes::transcript::encode_project_dir(&folder));
    std::fs::create_dir_all(&theirs).unwrap();
    std::fs::write(
        theirs.join(format!("{fresh_session}.jsonl")),
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();
    assert_eq!(rt.history(&fresh, 10).await.unwrap().len(), 1);
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

/// Speaks the control side: acks the hook registration, takes the prompt,
/// asks the hook about one Bash call, logs the answer, streams, finishes.
const BRIDGED: &str = r#"#!/bin/sh
echo "$@" >> @DIR@/args.log
prev=""
for a in "$@"; do
  if [ "$prev" = "--mcp-config" ]; then cat "$a" >> @DIR@/mcp.log; fi
  prev="$a"
done
read -r init
echo "$init" >> @DIR@/input.log
echo '{"type":"control_response","response":{"subtype":"success","request_id":"ironclaw-init","response":{}}}'
read -r prompt
echo "$prompt" >> @DIR@/input.log
echo '{"type":"control_request","request_id":"h1","request":{"subtype":"hook_callback","callback_id":"ironclaw-approval","input":{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls -la"},"tool_use_id":"tu1"}}}'
read -r answer
echo "$answer" >> @DIR@/answer.log
echo '{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}},"parent_tool_use_id":null,"session_id":"s","uuid":"e1"}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}},"parent_tool_use_id":null,"session_id":"s","uuid":"e2"}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}},"parent_tool_use_id":null,"session_id":"s","uuid":"e3"}'
echo '{"type":"result","subtype":"success","is_error":false,"duration_ms":5,"duration_api_ms":5,"num_turns":1,"result":"Hello","session_id":"s","total_cost_usd":0}'
read -r _eof
"#;

fn bridged_script(dir: &Path) -> PathBuf {
    script(
        dir,
        "claude",
        &BRIDGED.replace("@DIR@", &dir.display().to_string()),
    )
}

/// Waits for the turn's gate on the live stream, answering it with
/// `answer` (`None` leaves it to the timeout); returns every event seen.
async fn follow_turn(
    bridge: &CliBridge,
    events: &mut tokio::sync::broadcast::Receiver<LiveEvent>,
    thread: &ThreadId,
    answer: Option<bool>,
) -> Vec<LiveEvent> {
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("live event")
            .unwrap();
        if let (LiveEvent::GateOpened { gate, .. }, Some(approved)) = (&event, answer) {
            assert!(bridge.resolve_gate(thread, &gate.gate_ref, approved));
        }
        let ended = matches!(event, LiveEvent::Ended { .. });
        seen.push(event);
        if ended {
            return seen;
        }
    }
}

#[tokio::test]
async fn bridged_turn_asks_before_each_tool_and_streams_text() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    bridge.set_mcp_url("http://127.0.0.1:9/api/cli-bridge/mcp".into());
    let rt = bridged_runtime(
        dir.path(),
        bridged_script(dir.path()),
        &sink,
        Some(Arc::clone(&bridge)),
    );
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();

    let TurnAdmission::Started { run_id: Some(run) } =
        rt.start_turn(&thread, input()).await.unwrap()
    else {
        panic!("turn not started");
    };
    let seen = follow_turn(&bridge, &mut events, &thread.thread_id, Some(true)).await;
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;

    // The person's allow reached the CLI as the hook's decision.
    let answer = std::fs::read_to_string(dir.path().join("answer.log")).unwrap();
    assert!(
        answer.contains(r#""permissionDecision":"allow""#),
        "{answer}"
    );
    assert!(answer.contains(r#""request_id":"h1""#), "{answer}");
    let gate = seen
        .iter()
        .find_map(|e| match e {
            LiveEvent::GateOpened { gate, .. } => Some(gate.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (gate.tool_name.as_str(), gate.body.as_deref()),
        ("Bash", Some("ls -la"))
    );
    let texts: Vec<&str> = seen
        .iter()
        .filter_map(|e| match e {
            LiveEvent::Text { body, .. } => Some(body.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["Hel", "Hello"]);
    assert!(matches!(
        seen.last(),
        Some(LiveEvent::Ended {
            status: "completed",
            ..
        })
    ));
    let outcome = sink.finished.lock().unwrap()[0].clone();
    assert_eq!(outcome.status, TurnEndStatus::Completed);

    // Hook registered first; the prompt carries the run id as its uuid.
    let input_log = std::fs::read_to_string(dir.path().join("input.log")).unwrap();
    let lines: Vec<&str> = input_log.lines().collect();
    assert!(lines[0].contains(r#""subtype":"initialize""#) && lines[0].contains("PreToolUse"));
    assert!(
        lines[1].contains(&format!(r#""uuid":"{run}""#)),
        "{}",
        lines[1]
    );

    // Tools on, only our MCP server, still no allow rules or bypass.
    let args = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
    assert!(!args.contains("--tools"), "{args}");
    assert!(args.contains("--mcp-config") && args.contains("--strict-mcp-config"));
    assert!(args.contains("--permission-prompts none"), "{args}");
    assert!(
        !args.contains("dangerously") && !args.contains("allowed"),
        "{args}"
    );

    // The session token was this turn's only and died with it.
    let mcp: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("mcp.log")).unwrap())
            .unwrap();
    let server = &mcp["mcpServers"]["ironclaw"];
    assert_eq!(server["url"], "http://127.0.0.1:9/api/cli-bridge/mcp");
    let token = server["headers"]["Authorization"]
        .as_str()
        .unwrap()
        .strip_prefix("Bearer ")
        .unwrap();
    assert_eq!(bridge.resolve_token(token), None);
    let fresh = bridge.mint_token(
        SessionGrant {
            thread_id: thread.thread_id.clone(),
            run_id: run.clone(),
        },
        Duration::from_secs(60),
    );
    assert!(bridge.resolve_token(&fresh).is_some());
}

#[tokio::test]
async fn unanswered_tool_call_is_denied_on_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_millis(200));
    let rt = bridged_runtime(
        dir.path(),
        bridged_script(dir.path()),
        &sink,
        Some(Arc::clone(&bridge)),
    );
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();

    rt.start_turn(&thread, input()).await.unwrap();
    let seen = follow_turn(&bridge, &mut events, &thread.thread_id, None).await;
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;

    let answer = std::fs::read_to_string(dir.path().join("answer.log")).unwrap();
    assert!(
        answer.contains(r#""permissionDecision":"deny""#),
        "{answer}"
    );
    assert!(
        answer.contains(r#""subtype":"success""#),
        "never an error frame: {answer}"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, LiveEvent::GateClosed { .. }))
    );
    // No gateway URL yet: no MCP config, the hook still guards every call.
    let args = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
    assert!(
        !args.contains("--mcp-config") && !args.contains("--tools"),
        "{args}"
    );
}
