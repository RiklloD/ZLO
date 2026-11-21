// arch-exempt: large_file, one fake-CLI suite for the Claude driver (turns, steering, skill listing) awaits a split by concern, plan #v3-cli-bridge
//! The Claude driver against a fake `claude` script: arguments (fail-closed
//! tool flags, session start then resume), stream folding into outcome,
//! usage and telemetry, Stop killing the whole process group, and the
//! approval bridge (hook registration, allow and timeout-deny answers, live
//! text, the per-turn MCP token).
#![deny(clippy::let_underscore_must_use)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_cli_providers::bridge::{CliBridge, GateAnswer, LiveEvent, SessionGrant};
use ironclaw_cli_providers::claude::{ClaudeCliConfig, ClaudeCliRuntime, SkillListing};
use ironclaw_cli_providers::skill_listing::CliSkillUsage;
use ironclaw_filesystem::{InMemoryBackend, ScopedFilesystem};
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use ironclaw_host_api::mount::{MountGrant, MountPermissions, MountView};
use ironclaw_host_api::path::{MountAlias, VirtualPath};
use ironclaw_host_api::turn::DenialFeedback;
use ironclaw_orchestrator::model::{Millis, RuntimeId, ThreadRecord, ThreadScope};
use ironclaw_orchestrator::runtime::*;
use ironclaw_orchestrator::telemetry::TurnTelemetry;
use ironclaw_skills::usage::{
    FilesystemSkillUsageStore, SkillPin, SkillUsageName, SkillUsageObservation, SkillUsageOwner,
    SkillUsageSource, SkillUsageStore,
};

#[derive(Default)]
struct Recorder {
    finished: Mutex<Vec<TurnOutcome>>,
    unread: Mutex<Vec<String>>,
    /// Report every unread steer back as not kept (a failing queue).
    refuse_steers: std::sync::atomic::AtomicBool,
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
    async fn steers_unread(
        &self,
        _: &ThreadId,
        run_id: &str,
        steers: Vec<UnreadSteer>,
    ) -> SteerRecovery {
        assert!(
            self.finished.lock().unwrap().is_empty(),
            "reported before the turn ends"
        );
        assert!(!run_id.is_empty());
        self.unread
            .lock()
            .unwrap()
            .extend(steers.iter().map(|s| s.text.clone()));
        let refuse = self.refuse_steers.load(std::sync::atomic::Ordering::SeqCst);
        SteerRecovery {
            undelivered: if refuse { steers } else { Vec::new() },
        }
    }
}

const ANSWER: &str = r#"#!/bin/sh
{ echo "$@" | tr '\n' ' '; echo; } >> "$ARGS_LOG"
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

/// `/compact` sent as a turn: the summarizing call reads the whole context,
/// then the boundary says what it left.
const COMPACT: &str = r#"#!/bin/sh
read -r _line
echo '{"type":"assistant","message":{"id":"m","type":"message","role":"assistant","model":"claude-haiku-5-5","content":[{"type":"text","text":"summary"}],"usage":{"input_tokens":10,"cache_read_input_tokens":170000,"cache_creation_input_tokens":0,"output_tokens":900}},"session_id":"s","uuid":"a1"}'
echo '{"type":"system","subtype":"compact_boundary","session_id":"s","uuid":"b","compact_metadata":{"pre_tokens":170010,"post_tokens":9000,"trigger":"manual"}}'
echo '{"type":"result","subtype":"success","is_error":false,"duration_ms":600,"duration_api_ms":500,"num_turns":1,"result":"","session_id":"s","total_cost_usd":0.01,"usage":{"input_tokens":10,"cache_read_input_tokens":170000,"cache_creation_input_tokens":0,"output_tokens":900},"modelUsage":{"claude-haiku-5-5":{"inputTokens":10,"outputTokens":900,"cacheReadInputTokens":170000,"cacheCreationInputTokens":0,"webSearchRequests":0,"costUSD":0.01,"contextWindow":200000,"maxOutputTokens":32000}}}'
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
    configured_runtime(dir, binary, sink, bridge, None)
}

fn configured_runtime(
    dir: &Path,
    binary: PathBuf,
    sink: &Arc<Recorder>,
    bridge: Option<Arc<CliBridge>>,
    autocompact: Option<u64>,
) -> ClaudeCliRuntime {
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(sink) as Weak<dyn TurnEventSink>;
    ClaudeCliRuntime::new(claude_config(dir, binary, autocompact), sink, bridge).unwrap()
}

fn claude_config(dir: &Path, binary: PathBuf, autocompact: Option<u64>) -> ClaudeCliConfig {
    ClaudeCliConfig {
        runtime_id: "claude-cli".into(),
        display_name: "Claude CLI".into(),
        binary,
        config_dir: None,
        work_dir: dir.join("work"),
        login_file: dir.join(".claude.json"),
        projects_dir: dir.join("projects"),
        usage_file: Some(dir.join("usage").join("claude-cli.json")),
        autocompact,
        artifacts: false,
        skills: SkillListing::default(),
    }
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
        files: Vec::new(),
        work_dir: None,
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
    let body = ANSWER.replacen(
        "{ echo",
        "{ printf 'entrypoint=%s ' \"$CLAUDE_CODE_ENTRYPOINT\"; echo",
        1,
    );
    let rt = runtime(dir.path(), script(dir.path(), "claude", &body), &sink);
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
    // Every turn carries the orchestrator's subagent rules; only the second
    // carries the goal.
    assert!(
        lines[0].contains("--append-system-prompt")
            && lines[0].contains("## Subagents (T3 orchestrator)"),
        "{}",
        lines[0]
    );
    assert!(!lines[0].contains("Ship stage 5"), "{}", lines[0]);
    assert!(
        lines[1].contains("--append-system-prompt") && lines[1].contains("Ship stage 5"),
        "{}",
        lines[1]
    );
    for line in &lines {
        assert!(line.starts_with("entrypoint=ironclaw "), "{line}");
        assert!(line.contains("--tools  --strict-mcp-config"), "{line}");
        assert!(line.contains("--permission-prompts none"), "{line}");
        assert!(line.contains("--model claude-haiku-5-5"), "{line}");
        // The CLI's own subagents are off: subagents are t3_delegate threads.
        // So are its artifact tools (about 12k tokens a turn).
        assert!(
            line.contains(
                "--disallowedTools Agent,Task,Workflow,SendMessage,ListAgents,TaskStop,\
                 Artifact,ArtifactComments,ArtifactData,ArtifactCheck,DesignSync"
            ),
            "{line}"
        );
        assert!(
            !line.contains("dangerously") && !line.contains("--allowedTools"),
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
/// there; a worktree-bound thread runs in its worktree; history finds a
/// transcript in either folder.
#[tokio::test]
async fn a_compact_turn_reports_the_context_it_left() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let rt = runtime(dir.path(), script(dir.path(), "claude", COMPACT), &sink);
    let thread = thread(&rt).await;
    let mut turn = input();
    turn.text = "/compact".into();
    turn.origin = TurnOrigin::UserPrompt;
    rt.start_turn(&thread, turn).await.unwrap();
    wait_for(|| !sink.telemetry.lock().unwrap().is_empty()).await;
    let t = sink.telemetry.lock().unwrap()[0].clone();
    // Not the 170k the summarizing call read: the meter drops at once.
    assert_eq!(
        (t.context_used, t.context_max),
        (Some(9_000), Some(200_000))
    );
}

#[tokio::test]
async fn profile_soul_skills_and_folder_reach_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("profile-args.log");
    let body = ANSWER
        .replace(
            "{ echo \"$@\" | tr '\\n' ' '; echo; } >> \"$ARGS_LOG\"",
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
            entries: Vec::new(),
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
    let profile_for_worktree = profile.clone();
    let mut turn = input();
    turn.profile = Some(profile);
    rt.start_turn(&legacy, turn).await.unwrap();
    wait_for(|| sink.finished.lock().unwrap().len() == 3).await;

    // A thread bound to a worktree runs there (over the profile folder),
    // resuming its session, whose transcript follows it; a Stop restart
    // stays there too.
    let worktree = dir.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let mut turn = input();
    turn.profile = Some(profile_for_worktree);
    turn.work_dir = Some(worktree.clone());
    rt.start_turn(&legacy, turn).await.unwrap();
    wait_for(|| sink.finished.lock().unwrap().len() == 4).await;
    rt.steer(&legacy, "again", SteerHow::Restart).await.unwrap();
    wait_for(|| sink.finished.lock().unwrap().len() == 5).await;
    assert!(
        dir.path()
            .join("projects")
            .join(claude_codes::transcript::encode_project_dir(&worktree))
            .join(format!("{session}.jsonl"))
            .is_file()
    );

    let args = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    let in_worktree = format!("PWD={} ", worktree.canonicalize().unwrap().display());
    for line in &lines[3..5] {
        assert!(line.starts_with(&in_worktree), "{line}");
        assert!(line.contains(&format!("--resume {session}")), "{line}");
    }
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

/// A session moved into its thread's worktree and back keeps one
/// transcript: the old copy goes, and history and the next resume read
/// every message written in either folder.
#[tokio::test]
async fn a_session_follows_its_worktree_and_back_with_every_message() {
    let dir = tempfile::tempdir().unwrap();
    // Writes neither the shared args log nor a transcript of its own (the
    // other tests' process-wide variables must not reach this one).
    let body = ANSWER
        .replace(
            "{ echo \"$@\" | tr '\\n' ' '; echo; } >> \"$ARGS_LOG\"",
            "true",
        )
        .replace(
            "mkdir -p \"$TRANSCRIPT_DIR\" && echo '{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}' > \"$TRANSCRIPT_DIR/$SESSION.jsonl\"\n",
            "",
        );
    assert!(!body.contains("ARGS_LOG") && !body.contains("TRANSCRIPT_DIR"));
    let sink = Arc::new(Recorder::default());
    let rt = runtime(dir.path(), script(dir.path(), "claude", &body), &sink);
    let t = thread(&rt).await;
    let session = t.native_session_id.clone().unwrap();
    let file = format!("{session}.jsonl");
    let project = |folder: &Path| {
        dir.path()
            .join("projects")
            .join(claude_codes::transcript::encode_project_dir(folder))
            .join(&file)
    };
    let line = |text: &str| {
        format!("{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n")
    };
    let shared = project(&dir.path().join("work"));
    std::fs::create_dir_all(shared.parent().unwrap()).unwrap();
    std::fs::write(&shared, line("before the worktree")).unwrap();

    let worktree = dir.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let mut turn = input();
    turn.work_dir = Some(worktree.clone());
    rt.start_turn(&t, turn).await.unwrap();
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    let inside = project(&worktree);
    assert!(inside.is_file());
    assert!(!shared.exists(), "the old copy would shadow later turns");
    // The CLI writes this turn into the worktree's transcript.
    let mut text = std::fs::read_to_string(&inside).unwrap();
    text.push_str(&line("in the worktree"));
    std::fs::write(&inside, text).unwrap();
    assert_eq!(rt.history(&t, 10).await.unwrap().len(), 2);

    // The worktree is gone: the next turn runs back in the shared folder.
    rt.start_turn(&t, input()).await.unwrap();
    wait_for(|| sink.finished.lock().unwrap().len() == 2).await;
    assert!(!inside.exists());
    let back = std::fs::read_to_string(&shared).unwrap();
    assert!(back.contains("before the worktree") && back.contains("in the worktree"));
    assert_eq!(rt.history(&t, 10).await.unwrap().len(), 2);
}

/// An earlier build copied a moving session: the shared folder kept an
/// older copy while the worktree copy grew. The longer copy is the
/// session; the older one is set aside (kept, not deleted) and never
/// shadows history or the next resume.
#[tokio::test]
async fn an_older_duplicate_transcript_never_shadows_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let body = ANSWER
        .replace(
            "{ echo \"$@\" | tr '\\n' ' '; echo; } >> \"$ARGS_LOG\"",
            "true",
        )
        .replace(
            "mkdir -p \"$TRANSCRIPT_DIR\" && echo '{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}' > \"$TRANSCRIPT_DIR/$SESSION.jsonl\"\n",
            "",
        );
    assert!(!body.contains("ARGS_LOG") && !body.contains("TRANSCRIPT_DIR"));
    let sink = Arc::new(Recorder::default());
    let rt = runtime(dir.path(), script(dir.path(), "claude", &body), &sink);
    let t = thread(&rt).await;
    let session = t.native_session_id.clone().unwrap();
    let file = format!("{session}.jsonl");
    let project = |folder: &Path| {
        dir.path()
            .join("projects")
            .join(claude_codes::transcript::encode_project_dir(folder))
    };
    let line = |text: &str| {
        format!("{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{text}\"}}}}\n")
    };
    let shared = project(&dir.path().join("work"));
    let worktree = dir.path().join("worktree");
    std::fs::create_dir_all(&worktree).unwrap();
    let inside = project(&worktree);
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::write(shared.join(&file), line("one")).unwrap();
    std::fs::write(
        inside.join(&file),
        format!("{}{}", line("one"), line("two")),
    )
    .unwrap();

    assert_eq!(rt.history(&t, 10).await.unwrap().len(), 2);
    assert!(!shared.join(&file).exists());
    let aside: Vec<_> = std::fs::read_dir(&shared)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        aside
            .iter()
            .any(|n| n.starts_with(&format!("{file}.superseded-"))),
        "{aside:?}"
    );

    // The next turn, back in the shared folder, resumes the whole session.
    rt.start_turn(&t, input()).await.unwrap();
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    let back = std::fs::read_to_string(shared.join(&file)).unwrap();
    assert!(back.contains("one") && back.contains("two"), "{back}");
    assert!(!inside.join(&file).exists());
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
    answer: Option<GateAnswer>,
) -> Vec<LiveEvent> {
    let mut seen = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("live event")
            .unwrap();
        if let (LiveEvent::GateOpened { gate, .. }, Some(answer)) = (&event, answer.clone()) {
            assert!(bridge.resolve_gate(thread, &gate.gate_ref, answer));
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
    let seen = follow_turn(
        &bridge,
        &mut events,
        &thread.thread_id,
        Some(GateAnswer::Approve),
    )
    .await;
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
        args.contains("--disallowedTools Agent,Task,Workflow,SendMessage,ListAgents,TaskStop"),
        "{args}"
    );
    assert!(
        !args.contains("dangerously") && !args.contains("--allowedTools"),
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

/// "No, do X instead" on Claude Code: the call is denied through the
/// PreToolUse hook and the person's words are the hook's reason, which the
/// CLI hands to the model as the denied call's result.
#[tokio::test]
async fn rejected_tool_call_carries_the_feedback_in_the_hook_reason() {
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
    rt.start_turn(&thread, input()).await.unwrap();
    let feedback = DenialFeedback::new("list only the src folder").unwrap();
    follow_turn(
        &bridge,
        &mut events,
        &thread.thread_id,
        Some(GateAnswer::Decline {
            feedback: Some(feedback),
        }),
    )
    .await;
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;

    let answer = std::fs::read_to_string(dir.path().join("answer.log")).unwrap();
    assert!(
        answer.contains(r#""permissionDecision":"deny""#),
        "{answer}"
    );
    assert!(
        answer.contains(
            r#""permissionDecisionReason":"The user rejected this call: list only the src folder""#
        ),
        "{answer}"
    );
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

/// The 2.1.328 steering protocol, plus the task-list tools. The prompt runs
/// a `TaskCreate` (allowed without a gate; the task store becomes the plan),
/// then two steers arrive: the first is queued, its first result comes in,
/// then Claude starts it and asks about a Bash call (so stdin must still be
/// open to answer); the second is discarded unread.
const STEERED: &str = r#"#!/bin/sh
{ printf 'todo=%s list=%s teams=%s ' "$CLAUDE_CODE_ENABLE_TODO_TOOLS" "$CLAUDE_CODE_TASK_LIST_ID" "$CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS"; echo "$@"; } >> @DIR@/args.log
prev=""; sid=""
for a in "$@"; do
  if [ "$prev" = "--session-id" ] || [ "$prev" = "--resume" ]; then sid="$a"; fi
  prev="$a"
done
# 2.1.328's resolver: an inherited list id wins over the session.
list="${CLAUDE_CODE_TASK_LIST_ID:-$sid}"
uuid_of() { echo "$1" | sed 's/.*"uuid":"\([^"]*\)".*/\1/'; }
life() { echo "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$1\",\"state\":\"$2\",\"uuid\":\"l$2\",\"session_id\":\"s\"}"; }
result() { echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":5,\"duration_api_ms\":5,\"num_turns\":1,\"result\":\"$2\",\"session_id\":\"s\",\"total_cost_usd\":0,\"user_message_uuids\":[\"$1\"]}"; }
read -r init
echo '{"type":"control_response","response":{"subtype":"success","request_id":"ironclaw-init","response":{}}}'
read -r prompt
a=$(uuid_of "$prompt")
life "$a" queued; life "$a" started
echo '{"type":"control_request","request_id":"h0","request":{"subtype":"hook_callback","callback_id":"ironclaw-approval","input":{"hook_event_name":"PreToolUse","tool_name":"TaskCreate","tool_input":{"subject":"Plan","description":"Plan"},"tool_use_id":"tk1"}}}'
read -r task_answer
echo "$task_answer" >> @DIR@/answer.log
mkdir -p "@DIR@/tasks/$list"
echo '{"id":"1","subject":"Plan","description":"Plan","status":"in_progress","blocks":[],"blockedBy":[]}' > "@DIR@/tasks/$list/1.json"
echo '{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tk1","content":"Task #1 created successfully: Plan"}]},"parent_tool_use_id":null,"session_id":"s","uuid":"r0","tool_use_result":{"task":{"id":"1","subject":"Plan"}}}'
touch @DIR@/ready
read -r steer1
echo "$steer1" >> @DIR@/input.log
b=$(uuid_of "$steer1")
life "$b" queued
read -r steer2
c=$(uuid_of "$steer2")
life "$c" queued
result "$a" "Answer to the brief"
life "$a" completed
life "$b" started
echo '{"type":"control_request","request_id":"h2","request":{"subtype":"hook_callback","callback_id":"ironclaw-approval","input":{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls docs"},"tool_use_id":"tu2"}}}'
read -r answer
echo "$answer" >> @DIR@/answer.log
result "$b" "Steered"
life "$b" completed
life "$c" discarded
read -r _eof
"#;

#[tokio::test]
async fn a_steer_is_read_in_turn_and_a_dropped_one_comes_back_unread() {
    // Inherited from a parent Claude session or agent team: the task list
    // must still be this session's own (the hook lets task tools through
    // because they touch nothing else).
    // SAFETY: the runtime overrides or removes both in every child, so no
    // other test's child can observe them.
    unsafe {
        std::env::set_var("CLAUDE_CODE_TASK_LIST_ID", "shared-team-list");
        std::env::set_var("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS", "1");
    }
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = script(
        dir.path(),
        "claude",
        &STEERED.replace("@DIR@", &dir.path().display().to_string()),
    );
    let rt = configured_runtime(
        dir.path(),
        binary,
        &sink,
        Some(Arc::clone(&bridge)),
        Some(300_000),
    );
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("ready").exists()).await;
    for text in ["also list the docs", "drop me"] {
        let mut outcome = SteerOutcome::Rejected;
        for _ in 0..100 {
            outcome = rt.steer(&thread, text, SteerHow::Steer).await.unwrap();
            if outcome == SteerOutcome::Steered {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(outcome, SteerOutcome::Steered, "{text}");
    }
    let seen = follow_turn(
        &bridge,
        &mut events,
        &thread.thread_id,
        Some(GateAnswer::Approve),
    )
    .await;
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;

    // TaskCreate ran without a gate; only the Bash call asked.
    let gates: Vec<String> = seen
        .iter()
        .filter_map(|e| match e {
            LiveEvent::GateOpened { gate, .. } => Some(gate.tool_name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(gates, ["Bash"]);
    let (_, _, plan) = bridge.plan_of(&thread.thread_id).expect("task plan");
    assert_eq!(plan.entries[0].content, "Plan");
    // The steer's Bash call was answered after the first result: stdin
    // stayed open while the steer ran.
    let answers = std::fs::read_to_string(dir.path().join("answer.log")).unwrap();
    let answers: Vec<&str> = answers.lines().collect();
    assert_eq!(answers.len(), 2, "{answers:?}");
    assert!(answers[0].contains(r#""request_id":"h0""#) && answers[0].contains("allow"));
    assert!(answers[1].contains(r#""request_id":"h2""#) && answers[1].contains("allow"));
    // A steer waits for the tools in flight instead of cancelling them.
    let steer = std::fs::read_to_string(dir.path().join("input.log")).unwrap();
    assert!(steer.contains(r#""priority":"next""#), "{steer}");

    let outcome = sink.finished.lock().unwrap()[0].clone();
    assert_eq!(outcome.status, TurnEndStatus::Completed);
    assert_eq!(
        outcome.final_text.as_deref(),
        Some("Answer to the brief\n\nSteered")
    );
    assert_eq!(*sink.unread.lock().unwrap(), vec!["drop me".to_string()]);
    let args = std::fs::read_to_string(dir.path().join("args.log")).unwrap();
    let session = thread.native_session_id.as_deref().unwrap();
    assert!(
        args.starts_with(&format!("todo=1 list={session} teams= ")),
        "{args}"
    );
    assert!(args.contains("--autocompact 300000"), "{args}");
    assert!(!dir.path().join("tasks").join("shared-team-list").exists());
}

/// A steer written before the result that the CLI never acknowledges,
/// while unrelated frames keep streaming: stdin still closes once the
/// ack wait's absolute deadline passes (not that long after the last
/// frame). The steer comes back unread; the engine cannot keep it, so the
/// turn ends failed and says the message was not delivered.
const UNACKED_STEER: &str = r#"#!/bin/sh
uuid_of() { echo "$1" | sed 's/.*"uuid":"\([^"]*\)".*/\1/'; }
read -r init
echo '{"type":"control_response","response":{"subtype":"success","request_id":"ironclaw-init","response":{}}}'
read -r prompt
a=$(uuid_of "$prompt")
echo "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$a\",\"state\":\"started\",\"session_id\":\"s\"}"
touch @DIR@/ready
read -r steer
( i=0; while [ $i -lt 100 ]; do echo '{"type":"stream_event","event":{"type":"ping"},"session_id":"s","uuid":"p"}'; sleep 0.1; i=$((i+1)); done ) &
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":5,\"duration_api_ms\":5,\"num_turns\":1,\"result\":\"done\",\"session_id\":\"s\",\"total_cost_usd\":0,\"user_message_uuids\":[\"$a\"]}"
echo "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$a\",\"state\":\"completed\",\"session_id\":\"s\"}"
read -r _eof
date +%s > @DIR@/closed
kill $! 2>/dev/null
"#;

#[tokio::test]
async fn an_unacknowledged_steer_closes_input_at_an_absolute_deadline_and_is_never_lost() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    sink.refuse_steers
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = script(
        dir.path(),
        "claude",
        &UNACKED_STEER.replace("@DIR@", &dir.path().display().to_string()),
    );
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)))
        .with_steer_ack_timeout(Duration::from_secs(1));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("ready").exists()).await;
    let mut outcome = SteerOutcome::Rejected;
    for _ in 0..100 {
        outcome = rt
            .steer(&thread, "also check the logs", SteerHow::Steer)
            .await
            .unwrap();
        if outcome == SteerOutcome::Steered {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(outcome, SteerOutcome::Steered);
    let sent = std::time::Instant::now();
    // The pings run for 10 s; an inactivity timeout would wait them out.
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    assert!(
        sent.elapsed() < Duration::from_secs(4),
        "input closed only after {:?}",
        sent.elapsed()
    );
    // Written, never a word from the CLI: unconfirmed (Rule 1), so it is
    // neither replayed nor reported lost.
    assert!(sink.unread.lock().unwrap().is_empty());
    let outcome = sink.finished.lock().unwrap()[0].clone();
    assert_eq!(outcome.status, TurnEndStatus::Completed);
}

/// The CLI answers, then outlives its input: a Bash call that hit its
/// timeout was moved to the background and the CLI waits for it before it
/// exits (seen live: a hung `ntn api` search held a finished turn on
/// "Working" for 15+ minutes). Its shell commands run detached, in their
/// own process group, and here it also ignores SIGTERM. The turn ends a
/// bounded grace after the result, with the answer; a frame streamed after
/// it changes nothing; the CLI and its detached work are killed. `@EOF@`
/// is whether the CLI also closes its output first (then the wait for its
/// exit is what must be bounded).
const OUTLIVES_INPUT: &str = r#"#!/bin/sh
trap '' TERM
read -r init
echo '{"type":"control_response","response":{"subtype":"success","request_id":"ironclaw-init","response":{}}}'
read -r prompt
echo '{"type":"result","subtype":"success","is_error":false,"duration_ms":5,"duration_api_ms":5,"num_turns":1,"result":"Here is the answer","session_id":"s","total_cost_usd":0}'
read -r _eof
set -m
sleep 600 &
echo $! > @DIR@/background.pid
set +m
touch @DIR@/input-closed
if [ "@EOF@" = yes ]; then
  exec >/dev/null
else
  echo '{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":5,"duration_api_ms":5,"num_turns":1,"result":"woken up","session_id":"s","total_cost_usd":0}'
fi
while :; do sleep 1; done
"#;

#[tokio::test]
async fn a_cli_that_outlives_its_input_after_the_result_still_ends_the_turn() {
    for eof in ["no", "yes"] {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Recorder::default());
        let bridge = CliBridge::new(Duration::from_secs(10));
        let binary = script(
            dir.path(),
            "claude",
            &OUTLIVES_INPUT
                .replace("@DIR@", &dir.path().display().to_string())
                .replace("@EOF@", eof),
        );
        let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)))
            .with_exit_grace(Duration::from_millis(500));
        let thread = thread(&rt).await;
        let started = std::time::Instant::now();
        rt.start_turn(&thread, input()).await.unwrap();
        wait_for(|| dir.path().join("input-closed").exists()).await;
        wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "eof={eof}: turn ended only after {:?}",
            started.elapsed()
        );
        let outcome = sink.finished.lock().unwrap()[0].clone();
        assert_eq!(outcome.status, TurnEndStatus::Completed, "eof={eof}");
        assert_eq!(outcome.final_text.as_deref(), Some("Here is the answer"));
        // The detached background work went with the turn.
        let pid = std::fs::read_to_string(dir.path().join("background.pid")).unwrap();
        let pid = pid.trim();
        let pgid = std::process::Command::new("ps")
            .args(["-o", "pgid=", "-p", pid])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&pgid.stdout).trim().is_empty(),
            "eof={eof}: detached background process {pid} survived the turn"
        );
    }
}

/// A steer the CLI acknowledged (queued) and then never works on: after the
/// result it streams a while (still working, so input stays open), then
/// goes silent with nothing running. Before the idle bound this held stdin
/// open forever and the turn sat on "Working". Now input closes after the
/// silence, the steer comes back unread for the engine to queue, and the
/// turn ends with its answer.
const ACKED_STEER_GOES_IDLE: &str = r#"#!/bin/sh
uuid_of() { echo "$1" | sed 's/.*"uuid":"\([^"]*\)".*/\1/'; }
life() { echo "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$1\",\"state\":\"$2\",\"session_id\":\"s\"}"; }
read -r init
echo '{"type":"control_response","response":{"subtype":"success","request_id":"ironclaw-init","response":{}}}'
read -r prompt
a=$(uuid_of "$prompt")
life "$a" started
touch @DIR@/ready
read -r steer
b=$(uuid_of "$steer")
life "$b" queued
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":5,\"duration_api_ms\":5,\"num_turns\":1,\"result\":\"done\",\"session_id\":\"s\",\"total_cost_usd\":0,\"user_message_uuids\":[\"$a\"]}"
life "$a" completed
i=0; while [ $i -lt 15 ]; do echo '{"type":"stream_event","event":{"type":"ping"},"session_id":"s","uuid":"p"}'; sleep 0.1; i=$((i+1)); done
date +%s > @DIR@/quiet
read -r _eof
"#;

#[tokio::test]
async fn an_acknowledged_steer_the_cli_never_works_on_ends_the_turn_after_silence() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = script(
        dir.path(),
        "claude",
        &ACKED_STEER_GOES_IDLE.replace("@DIR@", &dir.path().display().to_string()),
    );
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)))
        .with_steer_idle_timeout(Duration::from_millis(800))
        .with_exit_grace(Duration::from_millis(500));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("ready").exists()).await;
    assert_eq!(
        rt.steer(&thread, "also check the logs", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Steered
    );
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    // The 1.5 s of output after the result kept input open: the silence
    // bound only counts from the last frame.
    assert!(
        dir.path().join("quiet").exists(),
        "closed while the CLI still streamed"
    );
    // Queued, never started, no word after: unconfirmed, not replayed.
    assert!(sink.unread.lock().unwrap().is_empty());
    let outcome = sink.finished.lock().unwrap()[0].clone();
    assert_eq!(outcome.status, TurnEndStatus::Completed);
    assert_eq!(outcome.final_text.as_deref(), Some("done"));
}

/// The hook registration is acknowledged only once `@DIR@/go` exists, so a
/// steer sent meanwhile arrives before the prompt is written. `@MODE@=ok`
/// acks and runs both; `refuse` refuses the registration.
const STEER_BEFORE_READY: &str = r#"#!/bin/sh
uuid_of() { echo "$1" | sed 's/.*"uuid":"\([^"]*\)".*/\1/'; }
life() { echo "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$1\",\"state\":\"$2\",\"session_id\":\"s\"}"; }
read -r init
touch @DIR@/init
while [ ! -e @DIR@/go ]; do sleep 0.05; done
if [ "@MODE@" = refuse ]; then
  echo '{"type":"control_response","response":{"subtype":"error","request_id":"ironclaw-init","error":"no hooks"}}'
  read -r _eof
  exit 0
fi
echo '{"type":"control_response","response":{"subtype":"success","request_id":"ironclaw-init","response":{}}}'
read -r prompt
echo "$prompt" >> @DIR@/input.log
a=$(uuid_of "$prompt")
read -r steer
echo "$steer" >> @DIR@/input.log
b=$(uuid_of "$steer")
life "$a" started
life "$b" queued
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":5,\"duration_api_ms\":5,\"num_turns\":1,\"result\":\"first\",\"session_id\":\"s\",\"total_cost_usd\":0,\"user_message_uuids\":[\"$a\"]}"
life "$a" completed
life "$b" started
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":5,\"duration_api_ms\":5,\"num_turns\":1,\"result\":\"steered\",\"session_id\":\"s\",\"total_cost_usd\":0,\"user_message_uuids\":[\"$b\"]}"
life "$b" completed
read -r _eof
"#;

#[tokio::test]
async fn a_steer_before_the_prompt_is_written_joins_the_turn_or_comes_back_unread() {
    for mode in ["ok", "refuse"] {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(Recorder::default());
        let bridge = CliBridge::new(Duration::from_secs(10));
        let binary = script(
            dir.path(),
            "claude",
            &STEER_BEFORE_READY
                .replace("@DIR@", &dir.path().display().to_string())
                .replace("@MODE@", mode),
        );
        let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)));
        let thread = thread(&rt).await;
        rt.start_turn(&thread, input()).await.unwrap();
        wait_for(|| dir.path().join("init").exists()).await;
        // Not a separate later turn: the running one takes it.
        assert_eq!(
            rt.steer(&thread, "early words", SteerHow::Steer)
                .await
                .unwrap(),
            SteerOutcome::Steered,
            "{mode}"
        );
        std::fs::write(dir.path().join("go"), "").unwrap();
        wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
        let outcome = sink.finished.lock().unwrap()[0].clone();
        if mode == "ok" {
            let log = std::fs::read_to_string(dir.path().join("input.log")).unwrap();
            let lines: Vec<&str> = log.lines().collect();
            assert_eq!(lines.len(), 2, "{log}");
            assert!(!lines[0].contains("early words"), "prompt first: {log}");
            assert!(lines[1].contains("early words"), "{log}");
            assert!(lines[1].contains(r#""priority":"next""#), "{log}");
            assert!(sink.unread.lock().unwrap().is_empty());
            assert_eq!(outcome.status, TurnEndStatus::Completed);
            assert_eq!(outcome.final_text.as_deref(), Some("first\n\nsteered"));
        } else {
            assert_eq!(
                *sink.unread.lock().unwrap(),
                vec!["early words".to_string()],
                "never written, so the engine queues it"
            );
            assert_eq!(outcome.status, TurnEndStatus::Failed);
        }
    }
}

/// A steer while a tool call waits for approval: the call is declined with
/// the person's words as the hook's reason, and no steer frame follows (the
/// model would read the words twice).
const STEER_AT_GATE: &str = r#"#!/bin/sh
uuid_of() { echo "$1" | sed 's/.*"uuid":"\([^"]*\)".*/\1/'; }
life() { echo "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$1\",\"state\":\"$2\",\"session_id\":\"s\"}"; }
read -r init
echo '{"type":"control_response","response":{"subtype":"success","request_id":"ironclaw-init","response":{}}}'
read -r prompt
a=$(uuid_of "$prompt")
life "$a" started
echo '{"type":"control_request","request_id":"h1","request":{"subtype":"hook_callback","callback_id":"ironclaw-approval","input":{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf build"},"tool_use_id":"tu1"}}}'
read -r answer
echo "$answer" >> @DIR@/answer.log
echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":5,\"duration_api_ms\":5,\"num_turns\":1,\"result\":\"ok\",\"session_id\":\"s\",\"total_cost_usd\":0,\"user_message_uuids\":[\"$a\"]}"
life "$a" completed
while IFS= read -r line; do echo "$line" >> @DIR@/input.log; done
"#;

#[tokio::test]
async fn a_steer_at_a_pending_approval_declines_it_with_the_words_and_writes_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = script(
        dir.path(),
        "claude",
        &STEER_AT_GATE.replace("@DIR@", &dir.path().display().to_string()),
    );
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)));
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("gate")
            .unwrap();
        if matches!(event, LiveEvent::GateOpened { .. }) {
            break;
        }
    }
    assert_eq!(
        rt.steer(&thread, "keep the build folder", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Steered
    );
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    let answer = std::fs::read_to_string(dir.path().join("answer.log")).unwrap();
    assert!(
        answer.contains(r#""permissionDecision":"deny""#)
            && answer.contains("keep the build folder"),
        "{answer}"
    );
    let after = std::fs::read_to_string(dir.path().join("input.log")).unwrap_or_default();
    assert!(
        !after.contains("keep the build folder"),
        "the words went twice: {after}"
    );
    assert!(sink.unread.lock().unwrap().is_empty());
    assert_eq!(
        sink.finished.lock().unwrap()[0].status,
        TurnEndStatus::Completed
    );
}

// ── Review (Sol) regressions ────────────────────────────────────────────────

/// Shell helpers every fixture below starts with.
const HEAD: &str = r#"#!/bin/sh
uuid_of() { echo "$1" | sed 's/.*"uuid":"\([^"]*\)".*/\1/'; }
life() { echo "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$1\",\"state\":\"$2\",\"session_id\":\"s\"}"; }
result() { echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":5,\"duration_api_ms\":5,\"num_turns\":1,\"result\":\"$2\",\"session_id\":\"s\",\"total_cost_usd\":0,\"user_message_uuids\":[\"$1\"]}"; }
hook() { echo "{\"type\":\"control_request\",\"request_id\":\"$1\",\"request\":{\"subtype\":\"hook_callback\",\"callback_id\":\"ironclaw-approval\",\"input\":{\"hook_event_name\":\"PreToolUse\",\"tool_name\":\"Bash\",\"tool_input\":{\"command\":\"$2\"},\"tool_use_id\":\"$1\"}}}"; }
read -r init
echo '{"type":"control_response","response":{"subtype":"success","request_id":"ironclaw-init","response":{}}}'
read -r prompt
a=$(uuid_of "$prompt")
life "$a" started
touch @DIR@/ready
"#;

fn fixture(dir: &Path, body: &str) -> PathBuf {
    script(
        dir,
        "claude",
        &format!("{HEAD}{body}").replace("@DIR@", &dir.display().to_string()),
    )
}

async fn steer_when_ready(rt: &ClaudeCliRuntime, thread: &ThreadRecord, dir: &Path, text: &str) {
    wait_for(|| dir.join("ready").exists()).await;
    assert_eq!(
        rt.steer(thread, text, SteerHow::Steer).await.unwrap(),
        SteerOutcome::Steered
    );
}

async fn wait_event(
    events: &mut tokio::sync::broadcast::Receiver<LiveEvent>,
    matches: impl Fn(&LiveEvent) -> bool,
) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
            .await
            .expect("live event")
            .unwrap();
        if matches(&event) {
            return;
        }
    }
}

/// Sol 1: a hook waits on a person after the result (busy), then is answered
/// and the CLI goes silent without another frame. The answer must wake the
/// read loop so the silence bound starts; before, the loop sat in an
/// untimed read forever.
#[tokio::test]
async fn silence_after_a_hook_is_answered_still_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(30));
    let binary = fixture(
        dir.path(),
        r#"read -r steer
b=$(uuid_of "$steer")
life "$b" queued
hook h1 "make"
result "$a" "done"
life "$a" completed
read -r answer
echo "$answer" >> @DIR@/answer.log
read -r _eof
"#,
    );
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)))
        .with_steer_idle_timeout(Duration::from_millis(600))
        .with_exit_grace(Duration::from_millis(500));
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    steer_when_ready(&rt, &thread, dir.path(), "also lint").await;
    wait_event(&mut events, |e| matches!(e, LiveEvent::GateOpened { .. })).await;
    // Busy on the person: no silence bound while the hook waits.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        sink.finished.lock().unwrap().is_empty(),
        "closed while a hook waited"
    );
    let gate = bridge.snapshot(&thread.thread_id).unwrap().gates[0].clone();
    assert!(bridge.resolve_gate(&thread.thread_id, &gate.gate_ref, GateAnswer::Approve));
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    // Queued, never started: unconfirmed (never replayed).
    assert!(sink.unread.lock().unwrap().is_empty());
}

/// Sol 1: a tool call whose result never comes no longer counts as work
/// forever: past its bound, silence ends the turn.
#[tokio::test]
async fn a_tool_call_that_never_finishes_stops_counting_as_work() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = fixture(
        dir.path(),
        r#"read -r steer
b=$(uuid_of "$steer")
life "$b" queued
result "$a" "done"
life "$a" completed
life "$b" started
echo '{"type":"assistant","parent_tool_use_id":null,"message":{"id":"m","type":"message","role":"assistant","content":[{"type":"tool_use","id":"t9","name":"Bash","input":{"command":"sleep 999"}}]},"session_id":"s"}'
read -r _eof
"#,
    );
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)))
        .with_steer_idle_timeout(Duration::from_millis(500))
        .with_tool_busy_bound(Duration::from_millis(500))
        .with_exit_grace(Duration::from_millis(500));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    steer_when_ready(&rt, &thread, dir.path(), "and then this").await;
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    assert!(sink.unread.lock().unwrap().is_empty(), "the CLI started it");
}

/// Sol 1: flushing a held steer the CLI is slow to read happens off the
/// output loop: the CLI's output after the hook ack is shown at once.
#[tokio::test]
async fn a_held_steer_flush_never_stalls_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let body = r#"#!/bin/sh
uuid_of() { echo "$1" | sed 's/.*"uuid":"\([^"]*\)".*/\1/' | cut -c1-36; }
read -r init
touch @DIR@/init
while [ ! -e @DIR@/go ]; do sleep 0.05; done
echo '{"type":"control_response","response":{"subtype":"success","request_id":"ironclaw-init","response":{}}}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}},"parent_tool_use_id":null,"session_id":"s","uuid":"e1"}'
while [ ! -e @DIR@/release ]; do sleep 0.05; done
read -r prompt
read -r steer
echo '{"type":"result","subtype":"success","is_error":false,"duration_ms":5,"duration_api_ms":5,"num_turns":1,"result":"ok","session_id":"s","total_cost_usd":0}'
read -r _eof
"#;
    let binary = script(
        dir.path(),
        "claude",
        &body.replace("@DIR@", &dir.path().display().to_string()),
    );
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)));
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("init").exists()).await;
    // Far bigger than a pipe buffer: its write blocks until the CLI reads.
    let big = "x".repeat(400_000);
    assert_eq!(
        rt.steer(&thread, &big, SteerHow::Steer).await.unwrap(),
        SteerOutcome::Steered
    );
    std::fs::write(dir.path().join("go"), "").unwrap();
    tokio::time::timeout(
        Duration::from_secs(3),
        wait_event(&mut events, |e| matches!(e, LiveEvent::Text { .. })),
    )
    .await
    .expect("output handled while the held steer was being written");
    std::fs::write(dir.path().join("release"), "").unwrap();
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
}

/// Sol 3: the silence bound closed input, then the CLI starts the queued
/// steer: that read still counts (it is not sent again as a later turn).
#[tokio::test]
async fn a_steer_started_after_input_closed_is_not_sent_again() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = fixture(
        dir.path(),
        r#"read -r steer
b=$(uuid_of "$steer")
life "$b" queued
result "$a" "done"
life "$a" completed
read -r _eof
life "$b" started
"#,
    );
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)))
        .with_steer_idle_timeout(Duration::from_millis(500))
        .with_exit_grace(Duration::from_millis(800));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    steer_when_ready(&rt, &thread, dir.path(), "read late").await;
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    assert!(
        sink.unread.lock().unwrap().is_empty(),
        "read during the exit grace: {:?}",
        sink.unread.lock().unwrap()
    );
}

/// Sol 3: Claude completes a command it never started; only `started` or
/// an echo proves a steer was read.
#[tokio::test]
async fn a_steer_completed_without_being_started_comes_back_unread() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = fixture(
        dir.path(),
        r#"read -r steer
b=$(uuid_of "$steer")
life "$b" queued
result "$a" "done"
life "$a" completed
life "$b" completed
read -r _eof
"#,
    );
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    steer_when_ready(&rt, &thread, dir.path(), "never started").await;
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    assert_eq!(
        *sink.unread.lock().unwrap(),
        vec!["never started".to_string()]
    );
}

/// Sol 4: with two calls waiting, the person's words go once, whole (past
/// the gate preview limit), as one denial's reason; the other is declined
/// plainly; no steer frame follows.
#[tokio::test]
async fn a_steer_at_two_gates_carries_its_whole_text_through_one_denial() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = fixture(
        dir.path(),
        r#"hook h1 "rm -rf build"
hook h2 "rm -rf dist"
read -r one
echo "$one" >> @DIR@/answer.log
read -r two
echo "$two" >> @DIR@/answer.log
result "$a" "ok"
life "$a" completed
while IFS= read -r line; do echo "$line" >> @DIR@/input.log; done
"#,
    );
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)));
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    for _ in 0..2 {
        wait_event(&mut events, |e| matches!(e, LiveEvent::GateOpened { .. })).await;
    }
    let text = format!("keep both folders {}", "and explain why ".repeat(200));
    assert!(text.chars().count() > 3_000);
    assert_eq!(
        rt.steer(&thread, &text, SteerHow::Steer).await.unwrap(),
        SteerOutcome::Steered
    );
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    let answers = std::fs::read_to_string(dir.path().join("answer.log")).unwrap();
    let answers: Vec<&str> = answers.lines().collect();
    assert_eq!(answers.len(), 2);
    assert!(
        answers
            .iter()
            .all(|a| a.contains(r#""permissionDecision":"deny""#))
    );
    let carrying: Vec<&&str> = answers
        .iter()
        .filter(|a| a.contains("keep both folders"))
        .collect();
    assert_eq!(carrying.len(), 1, "once: {answers:?}");
    assert!(carrying[0].contains(text.trim_end()), "whole text");
    let after = std::fs::read_to_string(dir.path().join("input.log")).unwrap_or_default();
    assert!(!after.contains("keep both folders"), "{after}");
}

/// Sol 6: a steer while Claude's own question waits answers the question
/// with the person's words as typed (no approval-denial sentence), and no
/// steer frame follows.
#[tokio::test]
async fn a_steer_at_a_pending_question_is_its_answer() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = fixture(
        dir.path(),
        r#"echo '{"type":"control_request","request_id":"q1","request":{"subtype":"hook_callback","callback_id":"ironclaw-approval","input":{"hook_event_name":"PreToolUse","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which database?","header":"DB","options":[{"label":"postgres"},{"label":"sqlite"}]}]},"tool_use_id":"q1"}}}'
read -r answer
echo "$answer" >> @DIR@/answer.log
result "$a" "ok"
life "$a" completed
while IFS= read -r line; do echo "$line" >> @DIR@/input.log; done
"#,
    );
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)));
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    wait_event(&mut events, |e| {
        matches!(e, LiveEvent::QuestionOpened { .. })
    })
    .await;
    assert_eq!(
        rt.steer(&thread, "use sqlite", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Steered
    );
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    let answer = std::fs::read_to_string(dir.path().join("answer.log")).unwrap();
    assert!(answer.contains("wrote instead: use sqlite"), "{answer}");
    assert!(!answer.contains("declined this call"), "{answer}");
    let after = std::fs::read_to_string(dir.path().join("input.log")).unwrap_or_default();
    assert!(!after.contains("use sqlite"), "{after}");
}

// ── Review round 3 (Sol): one input owner per turn ─────────────────────────

/// The CLI acks the hook registration, then never reads its input again
/// while keeping its output open. `@AFTER@` runs after the ack.
const DEAF_AFTER_ACK: &str = r#"#!/bin/sh
read -r init
touch @DIR@/init
while [ ! -e @DIR@/go ]; do sleep 0.05; done
echo '{"type":"control_response","response":{"subtype":"success","request_id":"ironclaw-init","response":{}}}'
@AFTER@
"#;

fn deaf_fixture(dir: &Path, after: &str) -> PathBuf {
    script(
        dir,
        "claude",
        &DEAF_AFTER_ACK
            .replace("@AFTER@", after)
            .replace("@DIR@", &dir.display().to_string()),
    )
}

/// A steer bigger than any pipe buffer, held for the prompt: its write
/// stalls until the CLI reads.
async fn hold_a_big_steer(rt: &ClaudeCliRuntime, thread: &ThreadRecord, dir: &Path) -> String {
    wait_for(|| dir.join("init").exists()).await;
    let big = format!("big {}", "x".repeat(400_000));
    assert_eq!(
        rt.steer(thread, &big, SteerHow::Steer).await.unwrap(),
        SteerOutcome::Steered
    );
    std::fs::write(dir.join("go"), "").unwrap();
    big
}

/// Sol 1: the held-steer write times out while the CLI keeps its output
/// open with no result. Before, the writer only logged and the reader
/// waited forever; now the failed write closes input, wakes the reader, and
/// the turn ends after the exit grace, with the steer back unread.
#[tokio::test]
async fn a_write_that_times_out_ends_the_turn_even_without_a_result() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = deaf_fixture(dir.path(), "while :; do sleep 1; done");
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)))
        .with_write_timeout(Duration::from_millis(500))
        .with_exit_grace(Duration::from_millis(500));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    let big = hold_a_big_steer(&rt, &thread, dir.path()).await;
    let started = std::time::Instant::now();
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        sink.finished.lock().unwrap()[0].status,
        TurnEndStatus::Failed
    );
    assert_eq!(*sink.unread.lock().unwrap(), vec![big]);
}

/// Sol 1: while the held-steer write is stalled, the reader still handles
/// `command_lifecycle` and `result` frames and the output after them: it
/// never waits on stdin.
#[tokio::test]
async fn output_frames_are_handled_while_a_write_is_stalled() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = deaf_fixture(
        dir.path(),
        r#"sleep 0.3
echo '{"type":"command_lifecycle","command_uuid":"other","state":"queued","session_id":"s"}'
echo '{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}},"parent_tool_use_id":null,"session_id":"s","uuid":"e1"}'
while [ ! -e @DIR@/release ]; do sleep 0.05; done
read -r prompt
read -r steer
echo '{"type":"result","subtype":"success","is_error":false,"duration_ms":5,"duration_api_ms":5,"num_turns":1,"result":"ok","session_id":"s","total_cost_usd":0}'
read -r _eof"#,
    );
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)))
        .with_steer_idle_timeout(Duration::from_millis(500))
        .with_steer_ack_timeout(Duration::from_millis(500))
        .with_exit_grace(Duration::from_millis(500));
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    hold_a_big_steer(&rt, &thread, dir.path()).await;
    tokio::time::timeout(
        Duration::from_secs(3),
        wait_event(&mut events, |e| matches!(e, LiveEvent::Text { .. })),
    )
    .await
    .expect("output after a lifecycle frame was held up by the stalled write");
    std::fs::write(dir.path().join("release"), "").unwrap();
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
}

/// Sol 2: the reply that carries the steer's words is stuck behind a
/// stalled write past the receipt bound. It was already handed to the
/// writer, so it is waited for (not withdrawn) and the words reach the CLI
/// once, in the denial, never again as a steer frame.
#[tokio::test]
async fn a_gate_reply_stuck_behind_a_write_is_not_doubled_by_a_steer() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(30));
    bridge.set_gate_receipt_timeout(Duration::from_millis(300));
    let binary = deaf_fixture(
        dir.path(),
        r#"echo '{"type":"control_request","request_id":"h1","request":{"subtype":"hook_callback","callback_id":"ironclaw-approval","input":{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf build"},"tool_use_id":"h1"}}}'
while [ ! -e @DIR@/release ]; do sleep 0.05; done
read -r prompt
read -r steer
read -r answer
printf '%s\n' "$answer" >> @DIR@/input.log
echo '{"type":"result","subtype":"success","is_error":false,"duration_ms":5,"duration_api_ms":5,"num_turns":1,"result":"ok","session_id":"s","total_cost_usd":0}'
while IFS= read -r line; do printf '%s\n' "$line" | cut -c1-2000 >> @DIR@/input.log; done"#,
    );
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)))
        .with_exit_grace(Duration::from_millis(500));
    let thread = thread(&rt).await;
    let mut events = bridge.subscribe();
    rt.start_turn(&thread, input()).await.unwrap();
    hold_a_big_steer(&rt, &thread, dir.path()).await;
    wait_event(&mut events, |e| matches!(e, LiveEvent::GateOpened { .. })).await;
    let steer = {
        let rt = &rt;
        let thread = thread.clone();
        async move {
            rt.steer(&thread, "use sqlite", SteerHow::Steer)
                .await
                .unwrap()
        }
    };
    let release = async {
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        std::fs::write(dir.path().join("release"), "").unwrap();
    };
    let (outcome, ()) = tokio::join!(steer, release);
    assert_eq!(outcome, SteerOutcome::Steered);
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    let log = std::fs::read_to_string(dir.path().join("input.log")).unwrap();
    assert_eq!(log.matches("use sqlite").count(), 1, "{log}");
    assert!(
        log.lines().next().unwrap().contains("permissionDecision"),
        "{log}"
    );
}

// ── Round 4 (Sol): settlement from evidence, one Stop boundary ────────────

/// Sol 2 / Rule 1: the CLI took a steer and exited before saying whether it
/// started it. Missing evidence is not "unread": it is never replayed.
#[tokio::test]
async fn a_steer_the_cli_took_before_exiting_silently_is_not_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = fixture(dir.path(), "read -r steer\nexit 0\n");
    let rt = bridged_runtime(dir.path(), binary, &sink, Some(Arc::clone(&bridge)));
    let thread = thread(&rt).await;
    rt.start_turn(&thread, input()).await.unwrap();
    steer_when_ready(&rt, &thread, dir.path(), "maybe read").await;
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    assert!(
        sink.unread.lock().unwrap().is_empty(),
        "replayed without evidence"
    );
}

/// Sol 4 / Rule 2: Stop is one boundary. While the CLI ignores SIGTERM for
/// the grace, a steer sent after Stop is not written to it; it is consumed
/// and handed to the engine (which shows the "not confirmed" notice).
#[tokio::test]
async fn a_steer_after_stop_is_never_written_during_the_signal_grace() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let bridge = CliBridge::new(Duration::from_secs(10));
    let binary = fixture(
        dir.path(),
        "trap '' TERM\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" | cut -c1-300 >> @DIR@/input.log; done\n",
    );
    let rt = Arc::new(bridged_runtime(
        dir.path(),
        binary,
        &sink,
        Some(Arc::clone(&bridge)),
    ));
    let thread = thread(rt.as_ref()).await;
    rt.start_turn(&thread, input()).await.unwrap();
    wait_for(|| dir.path().join("ready").exists()).await;
    let stop = {
        let (rt, thread) = (Arc::clone(&rt), thread.clone());
        tokio::spawn(async move { rt.cancel(&thread, "stop").await.unwrap() })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        rt.steer(&thread, "after stop", SteerHow::Steer)
            .await
            .unwrap(),
        SteerOutcome::Steered
    );
    stop.await.unwrap();
    wait_for(|| sink.finished.lock().unwrap().len() == 1).await;
    let log = std::fs::read_to_string(dir.path().join("input.log")).unwrap_or_default();
    assert!(!log.contains("after stop"), "written after Stop: {log}");
    assert_eq!(*sink.unread.lock().unwrap(), vec!["after stop".to_string()]);
}

/// Init frame with a skill catalog, then one `Skill` call.
const SKILLS: &str = r#"#!/bin/sh
{ echo "$@" | tr '\n' ' '; echo; } >> "@ARGS@"
read -r _line
echo '{"type":"system","subtype":"init","cwd":"/w","session_id":"s","tools":[],"mcp_servers":[],"model":"claude-haiku-5-5","permissionMode":"default","skills":["hot","cold","fresh","pinned","ironclaw:mine","artifact-design"]}'
echo '{"type":"assistant","message":{"id":"m","type":"message","role":"assistant","model":"claude-haiku-5-5","content":[{"type":"tool_use","id":"t1","name":"Skill","input":{"skill":"fresh"}}],"usage":{"input_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":1}},"session_id":"s","uuid":"u1"}'
echo '{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"duration_api_ms":1,"num_turns":1,"result":"ok","session_id":"s","total_cost_usd":0,"usage":{"input_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"output_tokens":1}}'
"#;

/// The skill listing a turn launches with comes from the shared skill usage
/// store (this runtime's catalog, every source's uses, the person's pins)
/// merged with `.claude.json`'s `skillUsage`, which is never written; the
/// turn's init frame and `Skill` calls update the store. A pre-store state
/// file's ledger is imported once and set aside.
#[tokio::test]
async fn skill_listing_follows_the_shared_store() {
    let dir = tempfile::tempdir().unwrap();
    let sink = Arc::new(Recorder::default());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let store = Arc::new(FilesystemSkillUsageStore::new(Arc::new(
        ScopedFilesystem::with_fixed_view(
            Arc::new(InMemoryBackend::new()),
            MountView::new(vec![MountGrant::new(
                MountAlias::new("/skill-usage").unwrap(),
                VirtualPath::new("/engine/tenants/t/users/u/skill-usage").unwrap(),
                MountPermissions::read_write(),
            )])
            .unwrap(),
        ),
    )));
    let owner = SkillUsageOwner {
        tenant_id: TenantId::new("t").unwrap(),
        user_id: UserId::new("u").unwrap(),
    };
    let source = SkillUsageSource::cli_runtime("claude-cli").unwrap();
    let name = |s: &str| SkillUsageName::new(s).unwrap();
    // The catalog of an earlier turn, and a pin set on the Skills page.
    store
        .record(
            &owner,
            &source,
            SkillUsageObservation {
                catalog: Some(vec![name("hot"), name("cold"), name("pinned")]),
                used: Vec::new(),
            },
            now,
        )
        .await
        .unwrap();
    store
        .set_pin(&owner, &name("pinned"), Some(SkillPin::AlwaysFull))
        .await
        .unwrap();
    // The pre-store state file: "hot" was used three times today.
    let legacy = dir.path().join("skills").join("claude-cli.json");
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(
        &legacy,
        serde_json::json!({
            "catalog": ["hot", "cold", "pinned"],
            "ledger": {"hot": {"count": 3, "last_used_ms": now}},
        })
        .to_string(),
    )
    .unwrap();
    // The account's own ranking: "cold" was used, but a year ago.
    let account = serde_json::json!({
        "skillUsage": {"cold": {"usageCount": 1, "lastUsedAt": now - 365 * 86_400_000}},
    })
    .to_string();
    std::fs::write(dir.path().join(".claude.json"), &account).unwrap();
    let profile_dir = dir.path().join("plugin");
    std::fs::create_dir_all(profile_dir.join(".claude-plugin")).unwrap();
    std::fs::write(
        profile_dir.join(".claude-plugin/plugin.json"),
        r#"{"name":"ironclaw"}"#,
    )
    .unwrap();
    let args_log = dir.path().join("args.log");
    let body = SKILLS.replace("@ARGS@", args_log.to_str().unwrap());
    let mut config = claude_config(dir.path(), script(dir.path(), "claude", &body), None);
    config.skills = SkillListing {
        usage: CliSkillUsage::new(store.clone(), owner.clone()),
        legacy_state_file: Some(legacy.clone()),
    };
    let weak: Weak<dyn TurnEventSink> = Arc::downgrade(&sink) as Weak<dyn TurnEventSink>;
    let rt = ClaudeCliRuntime::new(config, weak, None).unwrap();
    let thread = thread(&rt).await;
    let turn = || {
        let mut turn = input();
        turn.profile = Some(TurnProfile {
            skills: Some(ProfileSkills {
                plugin_dir: profile_dir.clone(),
                index: String::new(),
                entries: vec![ProfileSkillEntry {
                    name: "mine".into(),
                    description: "The profile's own".into(),
                    skill_md: profile_dir.join("skills/mine/SKILL.md"),
                }],
            }),
            ..TurnProfile::default()
        });
        turn
    };
    for n in 1..=2 {
        rt.start_turn(&thread, turn()).await.unwrap();
        wait_for(|| sink.finished.lock().unwrap().len() == n).await;
    }

    let args = std::fs::read_to_string(&args_log).unwrap();
    let settings: Vec<serde_json::Value> = args
        .lines()
        .map(|line| {
            let rest = line.split("--settings ").nth(1).expect(line);
            let json = rest.split(' ').next().unwrap();
            serde_json::from_str(json).expect(json)
        })
        .collect();
    let first = &settings[0]["skillOverrides"];
    assert_eq!(first["hot"], "on", "the imported ledger counts");
    assert_eq!(first["cold"], "name-only");
    assert_eq!(first["pinned"], "on");
    assert_eq!(first["artifact-design"], "off");
    assert!(first.get("fresh").is_none(), "not in the catalog yet");
    // The init frame replaced the catalog; the `Skill` call made "fresh" hot.
    let second = &settings[1]["skillOverrides"];
    assert_eq!(second["fresh"], "on");
    assert_eq!(second["ironclaw:mine"], "on", "the profile plugin stays on");
    assert_eq!(second["cold"], "name-only");
    assert_eq!(second["artifact-design"], "off");
    let document = store.load(&owner).await.unwrap().unwrap();
    let record = &document.sources[&source];
    assert_eq!(record.ledger[&name("fresh")].count, 2);
    assert_eq!(record.ledger[&name("hot")].count, 3);
    assert!(
        record.catalog.contains(&name("mine")),
        "plugin skills are kept under their bare name"
    );
    assert_eq!(record.catalog.len(), 6);
    assert!(
        !legacy.exists(),
        "the legacy file is set aside once imported"
    );
    assert!(legacy.with_extension("json.imported").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".claude.json")).unwrap(),
        account,
        "the account's file is never written"
    );
}
