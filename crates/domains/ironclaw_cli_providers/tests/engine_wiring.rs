//! The CLI runtimes wired to the orchestrator's steering contract, through
//! the real engine: a person's send while a turn runs goes through the
//! engine's admission path into `steer_message`, under the engine's steer
//! id, and the runtime's reports (read, unconfirmed) land in the engine's
//! steer ledger, which decides what is shown and what is sent again.
#![deny(clippy::let_underscore_must_use)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use ironclaw_cli_providers::bridge::CliBridge;
use ironclaw_cli_providers::claude::{ClaudeCliConfig, ClaudeCliRuntime};
use ironclaw_host_api::ids::{TenantId, UserId};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::model::{RuntimeId, ThreadRecord, ThreadScope};
use ironclaw_orchestrator::profile_runtime::UserTurn;
use ironclaw_orchestrator::runtime::*;
use ironclaw_orchestrator::steers::SteerRecord;

/// A Claude CLI that registers the hook, takes the prompt, then the steer;
/// `@AFTER@` is what it does once it has read the steer (`$b` is its uuid).
const CLAUDE: &str = r#"#!/bin/sh
uuid_of() { echo "$1" | sed 's/.*"uuid":"\([^"]*\)".*/\1/'; }
life() { echo "{\"type\":\"command_lifecycle\",\"command_uuid\":\"$1\",\"state\":\"$2\",\"session_id\":\"s\"}"; }
result() { echo "{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"duration_ms\":5,\"duration_api_ms\":5,\"num_turns\":1,\"result\":\"$2\",\"session_id\":\"s\",\"total_cost_usd\":0,\"user_message_uuids\":[\"$1\"]}"; }
read -r init
echo '{"type":"control_response","response":{"subtype":"success","request_id":"ironclaw-init","response":{}}}'
read -r prompt
a=$(uuid_of "$prompt")
life "$a" started
touch @DIR@/ready
read -r steer
b=$(uuid_of "$steer")
echo "$b" > @DIR@/steer-uuid
@AFTER@
"#;

fn script(dir: &Path, after: &str) -> PathBuf {
    let path = dir.join("claude");
    let body = CLAUDE
        .replace("@AFTER@", after)
        .replace("@DIR@", &dir.display().to_string());
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
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

struct Wired {
    engine: Arc<Engine>,
    thread: ThreadRecord,
    _dir: tempfile::TempDir,
    dir: PathBuf,
}

/// The engine over a fresh store, with a bridged Claude CLI runtime whose
/// events go to it, and a thread on that runtime.
async fn wired(after: &str) -> Wired {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let db = Arc::new(
        libsql::Builder::new_local(path.join("orch.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "claude-cli").await.unwrap();
    let sink: Weak<dyn TurnEventSink> = Arc::downgrade(&engine) as Weak<dyn TurnEventSink>;
    let bridge = CliBridge::new(Duration::from_secs(10));
    let rt = ClaudeCliRuntime::new(
        ClaudeCliConfig {
            runtime_id: "claude-cli".into(),
            display_name: "Claude CLI".into(),
            binary: script(&path, after),
            config_dir: None,
            work_dir: path.join("work"),
            login_file: path.join(".claude.json"),
            projects_dir: path.join("projects"),
            usage_file: None,
            autocompact: None,
            artifacts: false,
            skills: Default::default(),
        },
        sink,
        Some(bridge),
    )
    .unwrap()
    .with_exit_grace(Duration::from_millis(500));
    let rt: Arc<dyn TurnRuntime> = Arc::new(rt);
    engine.register_runtime(Arc::clone(&rt));
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
            model: None,
            idempotency_key: "k".into(),
        })
        .await
        .unwrap();
    let mut record = ThreadRecord::new(
        created.thread_id.clone(),
        scope,
        RuntimeId::new("claude-cli").unwrap(),
    );
    record.native_session_id = created.native_session_id;
    engine.store().ensure_thread(&record).await.unwrap();
    let thread = engine
        .store()
        .thread(&record.thread_id)
        .await
        .unwrap()
        .unwrap();
    Wired {
        engine,
        thread,
        _dir: dir,
        dir: path,
    }
}

impl Wired {
    async fn current(&self) -> ThreadRecord {
        self.engine
            .store()
            .thread(&self.thread.thread_id)
            .await
            .unwrap()
            .unwrap()
    }

    /// Start a turn, steer `text` into it once its prompt is written, and
    /// wait until the turn has ended. Returns the steer's ledger record.
    async fn steer_through_the_engine(&self, text: &str) -> SteerRecord {
        let started = self.engine.user_turn(&self.thread, "hi").await.unwrap();
        assert!(matches!(started, UserTurn::Started { .. }), "{started:?}");
        wait_for(|| self.dir.join("ready").exists()).await;
        let running = self.current().await;
        assert!(running.is_running());
        let steered = self.engine.user_turn(&running, text).await.unwrap();
        let UserTurn::Steered { steer_id, .. } = steered else {
            panic!("not steered: {steered:?}");
        };
        let ended = async {
            loop {
                if !self.current().await.is_running() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), ended)
            .await
            .expect("the turn never ended");
        // The runtime wrote the steer under the engine's id.
        let uuid = std::fs::read_to_string(self.dir.join("steer-uuid")).unwrap();
        let steers = self
            .engine
            .thread_steers(&self.thread.thread_id)
            .await
            .unwrap();
        let record = steers
            .into_iter()
            .find(|s| s.text == text)
            .expect("the steer is in the ledger");
        assert_eq!(
            uuid.trim(),
            record.id.as_str(),
            "the CLI got the engine's id"
        );
        if let Some(id) = steer_id {
            assert_eq!(id, record.id);
        }
        record
    }
}

/// The CLI starts the steer and answers it: the runtime reports it read,
/// keyed by the engine's id; the engine's ledger says read, nothing queued.
#[tokio::test]
async fn a_steer_the_cli_starts_is_read_in_the_engines_ledger() {
    let w = wired(
        r#"life "$b" queued
result "$a" "first"
life "$a" completed
life "$b" started
result "$b" "steered"
life "$b" completed
read -r _eof"#,
    )
    .await;
    let steer = w.steer_through_the_engine("also check the logs").await;
    assert_eq!(
        steer.state,
        ironclaw_orchestrator::model::SteerState::Read,
        "{steer:?}"
    );
    assert!(
        w.engine
            .store()
            .queue(&w.thread.thread_id, false)
            .await
            .unwrap()
            .is_empty(),
        "a read steer is never sent again"
    );
}

/// The CLI takes the steer and exits without a word about it: the runtime
/// reports it unconfirmed; the engine keeps a "Steer delivery not
/// confirmed" notice and never queues it again (Rule 1).
#[tokio::test]
async fn a_steer_with_no_word_either_way_is_unconfirmed_in_the_engines_ledger() {
    let w = wired("exit 0").await;
    let steer = w.steer_through_the_engine("maybe read").await;
    assert_eq!(
        steer.state,
        ironclaw_orchestrator::model::SteerState::Unconfirmed,
        "{steer:?}"
    );
    assert!(steer.unconfirmed_notice().contains("maybe read"));
    assert!(
        w.engine
            .store()
            .queue(&w.thread.thread_id, false)
            .await
            .unwrap()
            .is_empty(),
        "an unconfirmed steer is never sent again"
    );
}

/// The CLI drops the steer unread: reported dropped, the engine sends it
/// again as the next message (once).
#[tokio::test]
async fn a_steer_the_cli_drops_is_queued_once_by_the_engine() {
    let w = wired(
        r#"life "$b" queued
result "$a" "first"
life "$a" completed
life "$b" discarded
read -r _eof"#,
    )
    .await;
    let steer = w.steer_through_the_engine("drop me").await;
    let queued: Vec<String> = w
        .engine
        .store()
        .queue(&w.thread.thread_id, false)
        .await
        .unwrap()
        .into_iter()
        .map(|q| q.text)
        .collect();
    assert_eq!(queued, ["drop me"], "{steer:?}");
}
