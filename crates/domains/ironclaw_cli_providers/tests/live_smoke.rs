//! Live canaries: one short real turn per agent host, against the logged-in
//! binaries on this machine. Ignored by default (they call real models):
//!
//! ```sh
//! cargo test -p ironclaw_cli_providers --test live_smoke -- --ignored --nocapture
//! ```
//!
//! State, work and usage files go to a temp dir; only the agents' own logins
//! are used.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_cli_providers::{antigravity, muse};
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
    async fn turn_finished(&self, _: &ThreadId, _: Option<String>, o: TurnOutcome, _: Millis) {
        self.finished.lock().unwrap().push(o);
    }
    async fn turn_telemetry(&self, _: &ThreadId, t: TurnTelemetry, _: Millis) {
        self.telemetry.lock().unwrap().push(t);
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
