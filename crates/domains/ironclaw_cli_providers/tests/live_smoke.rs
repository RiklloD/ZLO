//! Live canaries: one short real turn per agent host, against the logged-in
//! binaries on this machine. Ignored by default (they call real models):
//!
//! ```sh
//! cargo test -p ironclaw_cli_providers --test live_smoke -- --ignored --nocapture
//! ```
//!
//! State, work and usage files go to a temp dir; only the agents' own logins
//! are used.
#![deny(clippy::let_underscore_must_use)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_cli_providers::bridge::{CliBridge, GateAnswer, LiveEvent};
use ironclaw_cli_providers::{antigravity, claude, muse};
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use ironclaw_orchestrator::model::{Millis, ThreadRecord, ThreadScope};
use ironclaw_orchestrator::runtime::*;
use ironclaw_orchestrator::telemetry::TurnTelemetry;

#[derive(Default)]
struct Recorder {
    finished: Mutex<Vec<TurnOutcome>>,
    telemetry: Mutex<Vec<TurnTelemetry>>,
    unread: Mutex<Vec<String>>,
}

#[async_trait]
impl TurnEventSink for Recorder {
    async fn turn_started(&self, _: &ThreadId, _: Option<String>, _: Millis) {}
    async fn turn_finished(&self, _: &ThreadId, _: Option<String>, o: TurnOutcome, _: Millis) {
        self.finished.lock().unwrap().push(o);
    }
    async fn turn_telemetry(&self, _: &ThreadId, t: TurnTelemetry, _: Millis) {
        self.telemetry.lock().unwrap().push(t);
    }
    async fn steers_unread(&self, _: &ThreadId, texts: Vec<String>) {
        self.unread.lock().unwrap().extend(texts);
    }
}

fn forget(path: &std::path::Path) {
    if let Err(error) = std::fs::remove_dir_all(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("could not remove {}: {error}", path.display());
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap())
}

fn scope() -> ThreadScope {
    ThreadScope {
        tenant_id: TenantId::new("t").unwrap(),
        user_id: UserId::new("u").unwrap(),
        agent_id: None,
        project_id: None,
    }
}

async fn one_turn(rt: &dyn TurnRuntime, recorder: &Recorder) {
    let catalog = rt.catalog(&scope()).await.unwrap();
    eprintln!("catalog: {catalog:?}");
    let created = rt
        .create_thread(CreateThread {
            scope: scope(),
            title: None,
            provider: None,
            model: None,
            idempotency_key: "smoke".into(),
        })
        .await
        .unwrap();
    let mut thread = ThreadRecord::new(created.thread_id, scope(), rt.id());
    thread.native_session_id = created.native_session_id;
    let started = std::time::Instant::now();
    rt.start_turn(
        &thread,
        TurnInput {
            text: "Reply with exactly the word pong and nothing else.".into(),
            origin: TurnOrigin::UserPrompt,
            model: None,
            goal: None,
            effort: None,
            profile: None,
            files: Vec::new(),
            work_dir: None,
        },
    )
    .await
    .unwrap();
    while recorder.finished.lock().unwrap().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "turn timed out"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    eprintln!("took {:?}", started.elapsed());
    eprintln!("outcome: {:?}", recorder.finished.lock().unwrap()[0]);
    eprintln!("telemetry: {:?}", recorder.telemetry.lock().unwrap());
    eprintln!("history: {:?}", rt.history(&thread, 10).await.unwrap());
    assert_eq!(
        recorder.finished.lock().unwrap()[0].status,
        TurnEndStatus::Completed
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "calls the real Antigravity agent"]
async fn antigravity_live_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = antigravity::default_config(&home()).expect("antigravity acp runtime");
    config.state_dir = dir.path().join("state");
    config.work_dir = dir.path().join("work");
    let recorder = Arc::new(Recorder::default());
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(&recorder) as Weak<dyn TurnEventSink>;
    let rt = antigravity::AntigravityRuntime::new(config, sink, None).unwrap();
    one_turn(&rt, &recorder).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "calls the real Muse Code agent"]
async fn muse_live_turn() {
    let dir = tempfile::tempdir().unwrap();
    let binary = muse::find_binary(&home()).expect("muse binary");
    let mut config = muse::default_config(&home(), &binary);
    config.state_dir = dir.path().join("state");
    config.work_dir = dir.path().join("work");
    config.usage_file = Some(dir.path().join("muse-code.json"));
    let recorder = Arc::new(Recorder::default());
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(&recorder) as Weak<dyn TurnEventSink>;
    let rt = muse::MuseRuntime::new(config, sink, None).unwrap();
    one_turn(&rt, &recorder).await;
    let usage = std::fs::read_to_string(dir.path().join("muse-code.json"));
    eprintln!("usage reading: {usage:?}");
}

/// The default Claude account, bridged (every gate is declined: nothing here
/// needs one): a task-list tool runs without a gate and becomes the plan, and
/// a steer sent while the reply streams is read in the same turn.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "calls the real Claude CLI"]
async fn claude_live_turn_with_tasks_and_a_steer() {
    let dir = tempfile::tempdir().unwrap();
    let binary = claude::find_binary(&home()).expect("claude binary");
    let version = std::process::Command::new(&binary)
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    eprintln!(
        "claude {version:?}, checked against {}",
        claude::EXPECTED_CLI_VERSION
    );
    let mut config = claude::default_accounts(&home(), &binary).remove(0);
    config.work_dir = dir.path().join("work");
    config.usage_file = Some(dir.path().join("claude-cli.json"));
    let recorder = Arc::new(Recorder::default());
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(&recorder) as Weak<dyn TurnEventSink>;
    let bridge = CliBridge::new(Duration::from_secs(30));
    let rt =
        claude::ClaudeCliRuntime::new(config.clone(), sink, Some(Arc::clone(&bridge))).unwrap();
    let created = rt
        .create_thread(CreateThread {
            scope: scope(),
            title: None,
            provider: None,
            model: Some("claude-haiku-5-5".into()),
            idempotency_key: "smoke".into(),
        })
        .await
        .unwrap();
    let mut thread = ThreadRecord::new(created.thread_id.clone(), scope(), rt.id());
    thread.native_session_id = created.native_session_id.clone();
    thread.model = created.model;
    let mut events = bridge.subscribe();
    let started = std::time::Instant::now();
    rt.start_turn(
        &thread,
        TurnInput {
            text: "First call TaskCreate once with subject \"Canary\". Then write the numbers one to sixty as words, one per line.".into(),
            origin: TurnOrigin::UserPrompt,
            model: None,
            goal: None,
            effort: None,
            profile: None,
            files: Vec::new(),
            work_dir: None,
        },
    )
    .await
    .unwrap();
    let mut gates = Vec::new();
    let mut steered = None;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(300), events.recv())
            .await
            .expect("live event")
            .unwrap();
        match &event {
            LiveEvent::GateOpened { gate, .. } => {
                gates.push(gate.tool_name.clone());
                bridge.resolve_gate(
                    &thread.thread_id,
                    &gate.gate_ref,
                    GateAnswer::Decline { feedback: None },
                );
            }
            LiveEvent::Text { body, .. } if steered.is_none() && body.contains("five") => {
                steered = Some(
                    rt.steer(
                        &thread,
                        "Stop counting now and reply with exactly the word steered.",
                        SteerHow::Steer,
                    )
                    .await
                    .unwrap(),
                );
            }
            LiveEvent::Ended { .. } => break,
            _ => {}
        }
    }
    while recorder.finished.lock().unwrap().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "turn timed out"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let outcome = recorder.finished.lock().unwrap()[0].clone();
    eprintln!("took {:?}", started.elapsed());
    eprintln!("outcome: {outcome:?}");
    eprintln!("gates: {gates:?}, steer: {steered:?}");
    eprintln!("telemetry: {:?}", recorder.telemetry.lock().unwrap());
    let plan = bridge.plan_of(&thread.thread_id);
    eprintln!("plan: {plan:?}");
    // Leave nothing in the account: this session's tasks and the project
    // folder its temp work dir got.
    if let Some(session) = &thread.native_session_id {
        forget(&home().join(".claude/tasks").join(session));
    }
    // `.tmpXXXX`: the CLI encodes the dot as a dash, like every separator.
    let work = dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .trim_start_matches('.')
        .to_string();
    for entry in std::fs::read_dir(&config.projects_dir).unwrap().flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .ends_with(&format!("{work}-work"))
        {
            forget(&entry.path());
        }
    }
    assert_eq!(outcome.status, TurnEndStatus::Completed);
    assert_eq!(steered, Some(SteerOutcome::Steered));
    assert!(
        recorder.unread.lock().unwrap().is_empty(),
        "the steer was read"
    );
    let answer = outcome.final_text.unwrap_or_default().to_lowercase();
    assert!(answer.contains("steered"), "{answer}");
    assert!(gates.is_empty(), "task tools need no gate: {gates:?}");
    let (_, _, plan) = plan.expect("the task list became the plan");
    assert!(plan.entries.iter().any(|e| e.content.contains("Canary")));
}
