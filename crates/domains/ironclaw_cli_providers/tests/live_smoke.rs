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
    async fn steers_unread(
        &self,
        _: &ThreadId,
        _: &str,
        steers: Vec<UnreadSteer>,
    ) -> SteerRecovery {
        self.unread
            .lock()
            .unwrap()
            .extend(steers.into_iter().map(|s| s.text));
        SteerRecovery::default()
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
            memory: None,
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
    let mut config = antigravity::default_config(&home())
        .await
        .expect("antigravity acp runtime");
    config.state_dir = dir.path().join("state");
    config.work_dir = dir.path().join("work");
    let recorder = Arc::new(Recorder::default());
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(&recorder) as Weak<dyn TurnEventSink>;
    eprintln!("launch: {:?}", config.launch);
    let rt = antigravity::AntigravityRuntime::new(config, sink, None).unwrap();
    one_turn(&rt, &recorder).await;
}

/// A minimal HTTP/1.1 loop in front of the real loopback MCP handler
/// ([`ironclaw_cli_providers::bridge::serve_mcp`]); records every
/// JSON-RPC method it served.
async fn serve_mcp_on(
    listener: tokio::net::TcpListener,
    bridge: Arc<CliBridge>,
    engine: Arc<ironclaw_orchestrator::engine::Engine>,
    seen: Arc<Mutex<Vec<String>>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let (bridge, engine, seen) = (Arc::clone(&bridge), Arc::clone(&engine), Arc::clone(&seen));
        tokio::spawn(async move {
            let mut buf = Vec::new();
            loop {
                let head_end = loop {
                    if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break at + 4;
                    }
                    let mut chunk = [0u8; 4096];
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let header = |name: &str| {
                    head.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim()
                            .eq_ignore_ascii_case(name)
                            .then(|| v.trim().to_string())
                    })
                };
                let len: usize = header("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + len {
                    let mut chunk = [0u8; 4096];
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let body: Vec<u8> = buf[head_end..head_end + len].to_vec();
                buf.drain(..head_end + len);
                let (status, reply) = if head.starts_with("POST ") {
                    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
                        let auth = header("authorization").map(|a| {
                            if a.starts_with("Bearer ") {
                                "bearer"
                            } else {
                                "other"
                            }
                        });
                        seen.lock().unwrap().push(format!(
                            "{} auth={auth:?}",
                            v["method"].as_str().unwrap_or("?")
                        ));
                    }
                    let auth = header("authorization");
                    match ironclaw_cli_providers::bridge::serve_mcp(
                        &bridge,
                        &engine,
                        true,
                        auth.as_deref(),
                        &body,
                    )
                    .await
                    {
                        Ok(Some(v)) => (200, v.to_string()),
                        Ok(None) => (202, String::new()),
                        Err(code) => (code, String::new()),
                    }
                } else {
                    (404, String::new())
                };
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{reply}",
                    reply.len()
                );
                if socket.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
    }
}

/// The real `agy acp` sees ZLO's orchestrator tools over the loopback MCP
/// route with the turn's token, and its call to one waits as an IronClaw
/// approval (approved here) before it reaches the route.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "calls the real Antigravity agent"]
async fn antigravity_live_sees_the_zlo_mcp_tools() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = antigravity::default_config(&home())
        .await
        .expect("antigravity agent");
    config.state_dir = dir.path().join("state");
    config.work_dir = dir.path().join("work");
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("orch.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = ironclaw_orchestrator::engine::Engine::open_libsql(runtime, "antigravity")
        .await
        .unwrap();
    let bridge = CliBridge::new(ironclaw_cli_providers::bridge::DEFAULT_APPROVAL_TIMEOUT);
    bridge.record_to(engine.store().clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    bridge.set_mcp_url(format!("http://{addr}/api/cli-bridge/mcp"));
    let seen = Arc::new(Mutex::new(Vec::new()));
    tokio::spawn(serve_mcp_on(
        listener,
        Arc::clone(&bridge),
        Arc::clone(&engine),
        Arc::clone(&seen),
    ));

    let recorder = Arc::new(Recorder::default());
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(&recorder) as Weak<dyn TurnEventSink>;
    let rt = antigravity::AntigravityRuntime::new(config, sink, Some(Arc::clone(&bridge))).unwrap();
    let created = rt
        .create_thread(CreateThread {
            scope: scope(),
            title: None,
            provider: None,
            model: None,
            idempotency_key: "smoke-mcp".into(),
        })
        .await
        .unwrap();
    let mut thread = ThreadRecord::new(created.thread_id, scope(), rt.id());
    thread.native_session_id = created.native_session_id;
    rt.start_turn(
        &thread,
        TurnInput {
            text: "List the exact names of every tool you have from the MCP server named \
                   ironclaw, one per line. Then call its t3_capabilities tool once and say \
                   how many providers it lists."
                .into(),
            memory: None,
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
    let started = std::time::Instant::now();
    let mut gates = Vec::new();
    while recorder.finished.lock().unwrap().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(400),
            "turn timed out"
        );
        if let Some(gate) = bridge
            .snapshot(&thread.thread_id)
            .and_then(|s| s.gates.first().cloned())
        {
            eprintln!("gate: {} {:?}", gate.tool_name, gate.body);
            gates.push(gate.tool_name.clone());
            assert!(bridge.resolve_gate(&thread.thread_id, &gate.gate_ref, GateAnswer::Approve));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let outcome = recorder.finished.lock().unwrap()[0].clone();
    eprintln!("mcp requests: {:?}", seen.lock().unwrap());
    eprintln!("gates: {gates:?}");
    eprintln!("reply: {:?}", outcome.final_text);
    assert_eq!(outcome.status, TurnEndStatus::Completed, "{outcome:?}");
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter().any(|m| m == "tools/list auth=Some(\"bearer\")"),
        "{seen:?}"
    );
    assert!(seen.iter().any(|m| m.starts_with("tools/call")), "{seen:?}");
    assert!(
        gates.iter().any(|g| g.contains("t3_capabilities")),
        "{gates:?}"
    );
    // The call itself answers "thread not found": this engine never
    // created the thread. Reaching the route after the gate is the point.
    let reply = outcome.final_text.unwrap_or_default();
    assert!(reply.contains("t3_delegate"), "{reply}");
}

/// The native `agy acp` takes a message mid-turn as a steer: the turn
/// carries on under it and finishes once, with the steered answer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "calls the real Antigravity agent"]
async fn antigravity_live_steer() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = antigravity::default_config(&home())
        .await
        .expect("antigravity agent");
    assert!(
        matches!(config.launch, antigravity::launch::AgentLaunch::Cli { .. }),
        "the native agy CLI is preferred: {:?}",
        config.launch
    );
    config.state_dir = dir.path().join("state");
    config.work_dir = dir.path().join("work");
    let recorder = Arc::new(Recorder::default());
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(&recorder) as Weak<dyn TurnEventSink>;
    let rt = antigravity::AntigravityRuntime::new(config, sink, None).unwrap();
    let created = rt
        .create_thread(CreateThread {
            scope: scope(),
            title: None,
            provider: None,
            model: None,
            idempotency_key: "smoke-steer".into(),
        })
        .await
        .unwrap();
    let mut thread = ThreadRecord::new(created.thread_id, scope(), rt.id());
    thread.native_session_id = created.native_session_id;
    rt.start_turn(
        &thread,
        TurnInput {
            text: "Write a 60-line poem about the sea, one line at a time. Use no tools.".into(),
            memory: None,
            origin: TurnOrigin::UserPrompt,
            model: None,
            goal: Some("keep replies short".into()),
            effort: None,
            profile: None,
            files: Vec::new(),
            work_dir: None,
        },
    )
    .await
    .unwrap();
    let started = std::time::Instant::now();
    let mut steered = SteerOutcome::Rejected;
    while started.elapsed() < Duration::from_secs(60) {
        tokio::time::sleep(Duration::from_secs(3)).await;
        steered = rt
            .steer(
                &thread,
                "Stop the poem. Reply with only the word BANANA.",
                SteerHow::Steer,
            )
            .await
            .unwrap();
        if steered == SteerOutcome::Steered || !recorder.finished.lock().unwrap().is_empty() {
            break;
        }
    }
    assert_eq!(steered, SteerOutcome::Steered);
    while recorder.finished.lock().unwrap().is_empty() {
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "turn timed out"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let finished = recorder.finished.lock().unwrap().clone();
    eprintln!("outcome: {:?}", finished);
    eprintln!("telemetry: {:?}", recorder.telemetry.lock().unwrap());
    eprintln!("history: {:?}", rt.history(&thread, 10).await.unwrap());
    assert_eq!(finished.len(), 1, "one turn, steered, not restarted");
    assert_eq!(finished[0].status, TurnEndStatus::Completed);
    assert!(
        finished[0]
            .final_text
            .as_deref()
            .is_some_and(|t| t.contains("BANANA")),
        "{:?}",
        finished[0]
    );
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
    let mut config = claude::default_accounts(
        &home(),
        &binary,
        &ironclaw_cli_providers::skill_listing::CliSkillUsage::default(),
    )
    .remove(0);
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
            memory: None,
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
