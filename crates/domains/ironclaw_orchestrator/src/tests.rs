//! Engine core tests over a real libSQL file and a scripted runtime.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use serde_json::json;

use crate::engine::{CallContext, Engine};
use crate::model::*;
use crate::restart::CONTINUE_PROMPT;
use crate::runtime::*;
use crate::store::Store;

#[derive(Default)]
struct FakeRuntime {
    started: Mutex<Vec<(ThreadId, String)>>,
    cancelled: Mutex<Vec<ThreadId>>,
    history: Mutex<HashMap<ThreadId, Vec<HistoryMessage>>>,
    created: Mutex<u32>,
    /// A second runtime (a CLI driver stand-in) advertising one provider of
    /// this name; `None` is the default runtime `fake`.
    name: Option<&'static str>,
}

impl FakeRuntime {
    fn starts_on(&self, thread: &ThreadId) -> Vec<String> {
        self.started
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == thread)
            .map(|(_, text)| text.clone())
            .collect()
    }
}

fn msg(role: HistoryRole, text: &str) -> HistoryMessage {
    HistoryMessage {
        role,
        text: text.into(),
        tool_calls: vec![],
        run_id: None,
        hidden: false,
    }
}

#[async_trait]
impl TurnRuntime for FakeRuntime {
    fn id(&self) -> RuntimeId {
        RuntimeId::new(self.name.unwrap_or("fake")).unwrap()
    }
    async fn catalog(&self, _scope: &ThreadScope) -> Result<Vec<CatalogProvider>, RuntimeError> {
        Ok(self
            .name
            .map(|name| CatalogProvider {
                provider: name.into(),
                name: name.into(),
                authenticated: true,
                current: false,
                models: vec!["m1".into(), "m2".into()],
            })
            .into_iter()
            .collect())
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            steer_in_turn: true,
            ..Default::default()
        }
    }
    async fn create_thread(&self, _r: CreateThread) -> Result<CreatedThread, RuntimeError> {
        let mut n = self.created.lock().unwrap();
        *n += 1;
        Ok(CreatedThread {
            thread_id: ThreadId::new(format!("{}child-{n}", self.name.unwrap_or(""))).unwrap(),
            native_session_id: self.name.map(|_| format!("session-{n}")),
            provider: None,
            model: None,
        })
    }
    async fn start_turn(
        &self,
        t: &ThreadRecord,
        input: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        self.started
            .lock()
            .unwrap()
            .push((t.thread_id.clone(), input.text.clone()));
        let mut history = self.history.lock().unwrap();
        history
            .entry(t.thread_id.clone())
            .or_default()
            .push(msg(HistoryRole::User, &input.text));
        Ok(TurnAdmission::Started {
            run_id: Some(format!("run-{}", now_ms())),
        })
    }
    async fn steer(
        &self,
        _t: &ThreadRecord,
        _text: &str,
        _h: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        Ok(SteerOutcome::Steered)
    }
    async fn cancel(&self, t: &ThreadRecord, _reason: &str) -> Result<(), RuntimeError> {
        self.cancelled.lock().unwrap().push(t.thread_id.clone());
        Ok(())
    }
    async fn history(
        &self,
        t: &ThreadRecord,
        _limit: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        Ok(self
            .history
            .lock()
            .unwrap()
            .get(&t.thread_id)
            .cloned()
            .unwrap_or_default())
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    db: Arc<libsql::Database>,
    engine: Arc<Engine>,
    rt: Arc<FakeRuntime>,
}

fn scope() -> ThreadScope {
    ThreadScope {
        tenant_id: TenantId::new("t1").unwrap(),
        user_id: UserId::new("u1").unwrap(),
        agent_id: None,
        project_id: None,
    }
}

fn tid(raw: &str) -> ThreadId {
    ThreadId::new(raw).unwrap()
}

fn ctx(thread: &str) -> CallContext {
    CallContext {
        thread_id: tid(thread),
        scope: scope(),
        provider: None,
        model: None,
    }
}

async fn open_engine(db: &Arc<libsql::Database>) -> Arc<Engine> {
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(db)).unwrap());
    Engine::open(Store::new(runtime), RuntimeId::new("fake").unwrap())
        .await
        .unwrap()
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let engine = open_engine(&db).await;
    let rt = Arc::new(FakeRuntime::default());
    engine.register_runtime(rt.clone());
    Harness {
        _dir: dir,
        db,
        engine,
        rt,
    }
}

async fn finish(engine: &Engine, thread: &ThreadId, outcome: TurnOutcome) {
    let t = engine.store().thread(thread).await.unwrap().unwrap();
    engine
        .turn_finished(thread, t.active_run_id.clone(), outcome, now_ms())
        .await;
}

async fn task_status(engine: &Engine, id: &str) -> TaskStatus {
    engine
        .store()
        .task(&TaskId::new(id).unwrap())
        .await
        .unwrap()
        .unwrap()
        .status
}

#[tokio::test]
async fn queue_drains_only_when_idle_and_unheld() {
    let h = harness().await;
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    h.engine
        .store()
        .set_turn_running(&a, Some("r0"), now_ms())
        .await
        .unwrap();
    h.engine.queue_add(&a, "first", Author::User).await.unwrap();
    h.engine
        .queue_add(&a, "second", Author::User)
        .await
        .unwrap();

    h.engine.tick().await;
    assert!(h.rt.starts_on(&a).is_empty(), "busy thread must not drain");

    finish(&h.engine, &a, TurnOutcome::completed(None)).await;
    h.engine.queue_hold(&a, true).await.unwrap();
    h.engine.tick().await;
    assert!(h.rt.starts_on(&a).is_empty(), "held thread must not drain");

    h.engine.queue_hold(&a, false).await.unwrap();
    h.engine.tick().await;
    h.engine.tick().await; // still running "first": no second drain
    assert_eq!(h.rt.starts_on(&a), vec!["first".to_string()]);
    finish(&h.engine, &a, TurnOutcome::completed(None)).await;
    h.engine.tick().await;
    assert_eq!(
        h.rt.starts_on(&a),
        vec!["first".to_string(), "second".to_string()]
    );
}

#[tokio::test]
async fn stop_propagates_to_every_descendant() {
    let h = harness().await;
    let child = h
        .engine
        .delegate(
            &ctx("root"),
            serde_json::from_value(json!({"task": "level 1"})).unwrap(),
        )
        .await
        .unwrap();
    let child_thread = tid(child["childThreadId"].as_str().unwrap());
    let grand = h
        .engine
        .delegate(
            &ctx(child_thread.as_str()),
            serde_json::from_value(json!({"task": "level 2"})).unwrap(),
        )
        .await
        .unwrap();
    let grand_thread = tid(grand["childThreadId"].as_str().unwrap());
    // The level-1 child finishes its own turn while its child still runs.
    finish(
        &h.engine,
        &child_thread,
        TurnOutcome::completed(Some("partial".into())),
    )
    .await;
    assert_eq!(
        task_status(&h.engine, child["taskId"].as_str().unwrap()).await,
        TaskStatus::WaitingChildren
    );

    h.engine.on_user_stop(&tid("root")).await.unwrap();
    assert!(h.rt.cancelled.lock().unwrap().contains(&grand_thread));
    assert!(h.engine.store().hold(&tid("root")).await.unwrap().is_some());
    // The interrupted grandchild reports back; its task ends cancelled.
    finish(
        &h.engine,
        &grand_thread,
        TurnOutcome::ended(TurnEndStatus::Cancelled, None),
    )
    .await;
    assert_eq!(
        task_status(&h.engine, grand["taskId"].as_str().unwrap()).await,
        TaskStatus::Cancelled
    );
    // The idle level-1 child is finalized directly.
    assert_eq!(
        task_status(&h.engine, child["taskId"].as_str().unwrap()).await,
        TaskStatus::Cancelled
    );
    // The held root receives no result card until the user speaks again.
    h.engine.tick().await;
    assert!(h.rt.starts_on(&tid("root")).is_empty());
    h.engine.on_user_prompt(&tid("root")).await.unwrap();
    h.engine.tick().await;
    assert_eq!(h.rt.starts_on(&tid("root")).len(), 1);
}

#[tokio::test]
async fn delegation_limits_and_result_delivery() {
    let h = harness().await;
    h.engine
        .store()
        .set_settings(&json!({"max_running_per_parent": 1}))
        .await
        .unwrap();
    let first = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(json!({"task": "x", "client_request_id": "r1"})).unwrap(),
        )
        .await
        .unwrap();
    let again = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(json!({"task": "x", "client_request_id": "r1"})).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        first["taskId"], again["taskId"],
        "client_request_id is idempotent"
    );
    let err = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(json!({"task": "y"})).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "too_many_tasks");

    let child = tid(first["childThreadId"].as_str().unwrap());
    h.engine
        .store()
        .set_turn_running(&tid("p"), Some("pr"), now_ms())
        .await
        .unwrap();
    finish(
        &h.engine,
        &child,
        TurnOutcome::completed(Some("done!".into())),
    )
    .await;
    h.engine.tick().await;
    assert!(
        h.rt.starts_on(&tid("p")).is_empty(),
        "busy parent waits for its card"
    );
    finish(&h.engine, &tid("p"), TurnOutcome::completed(None)).await;
    h.engine.tick().await;
    let card = h.rt.starts_on(&tid("p"));
    assert_eq!(card.len(), 1);
    assert!(card[0].contains("done!"));
}

#[tokio::test]
async fn restart_cut_turns_continue_once_idle() {
    let h = harness().await;
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    h.engine
        .store()
        .set_turn_running(&a, Some("r1"), now_ms())
        .await
        .unwrap();
    let cut = TurnOutcome {
        cut_by_restart: true,
        ..TurnOutcome::ended(TurnEndStatus::Interrupted, None)
    };
    finish(&h.engine, &a, cut).await;
    h.engine.tick().await;
    assert_eq!(h.rt.starts_on(&a), vec![CONTINUE_PROMPT.to_string()]);
    h.engine.tick().await;
    assert_eq!(h.rt.starts_on(&a).len(), 1, "fires once");

    // Live work a dead engine instance left behind is a cut turn too.
    let b = tid("b");
    h.engine.ensure_thread(&b, &scope(), None).await.unwrap();
    h.engine
        .store()
        .put_live_work(&b, "old-instance", &json!({"run_id": "x"}))
        .await
        .unwrap();
    let fresh = open_engine(&h.db).await;
    fresh.register_runtime(h.rt.clone());
    fresh.tick().await;
    assert_eq!(h.rt.starts_on(&b), vec![CONTINUE_PROMPT.to_string()]);

    // A user message after the cut supersedes the continuation.
    let c = tid("c");
    h.engine.ensure_thread(&c, &scope(), None).await.unwrap();
    h.engine
        .store()
        .set_turn_running(&c, Some("r2"), now_ms() - 5_000)
        .await
        .unwrap();
    let cut = TurnOutcome {
        cut_by_restart: true,
        ..TurnOutcome::ended(TurnEndStatus::Interrupted, None)
    };
    h.engine
        .turn_finished(&c, Some("r2".into()), cut, now_ms() - 1_000)
        .await;
    h.engine.on_user_prompt(&c).await.unwrap();
    h.engine.tick().await;
    assert!(h.rt.starts_on(&c).is_empty());

    // A clean quit is not a crash: reopening continues nothing and the
    // queue waits until the user presses Continue (releases the hold).
    let q = tid("q");
    h.engine.ensure_thread(&q, &scope(), None).await.unwrap();
    h.engine
        .store()
        .set_turn_running(&q, Some("r3"), now_ms())
        .await
        .unwrap();
    h.engine
        .store()
        .put_live_work(&q, &h.engine.instance_id, &json!({"run_id": "r3"}))
        .await
        .unwrap();
    assert_eq!(h.engine.prepare_for_shutdown().await.unwrap(), 1);
    let reopened = open_engine(&h.db).await;
    reopened.register_runtime(h.rt.clone());
    reopened.queue_add(&q, "later", Author::User).await.unwrap();
    let cut = TurnOutcome {
        cut_by_restart: true,
        ..TurnOutcome::ended(TurnEndStatus::Interrupted, None)
    };
    finish(&reopened, &q, cut).await;
    reopened.tick().await;
    reopened.tick().await;
    assert!(
        h.rt.starts_on(&q).is_empty(),
        "quit work never auto-continues"
    );
    reopened.queue_hold(&q, false).await.unwrap();
    reopened.tick().await;
    assert_eq!(
        h.rt.starts_on(&q),
        vec![CONTINUE_PROMPT.to_string()],
        "Continue resumes the cut turn before the queue drains"
    );
}

#[tokio::test]
async fn usage_limit_parks_task_and_resumes_after_reset() {
    let h = harness().await;
    h.engine
        .store()
        .set_settings(&json!({"usage_limit_grace_ms": 0}))
        .await
        .unwrap();
    let task = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(json!({"task": "long job"})).unwrap(),
        )
        .await
        .unwrap();
    let child = tid(task["childThreadId"].as_str().unwrap());
    let reset = now_ms() + 300;
    let limited = TurnOutcome {
        usage_limit_resets_at: Some(reset),
        usage: Some(TurnUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        }),
        ..TurnOutcome::ended(TurnEndStatus::Failed, Some("usage limit".into()))
    };
    finish(&h.engine, &child, limited).await;
    let id = task["taskId"].as_str().unwrap();
    assert_eq!(task_status(&h.engine, id).await, TaskStatus::Waiting);
    h.engine.tick().await;
    assert_eq!(
        h.rt.starts_on(&child).len(),
        1,
        "no resume before the reset"
    );
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    h.engine.tick().await;
    assert_eq!(
        h.rt.starts_on(&child).last().map(String::as_str),
        Some(CONTINUE_PROMPT)
    );
    assert_eq!(task_status(&h.engine, id).await, TaskStatus::Running);
    let t = h
        .engine
        .store()
        .task(&TaskId::new(id).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(t.usage.unwrap().input_tokens, 10);
}

#[tokio::test]
async fn fork_hands_off_context_and_merge_back_sends_the_delta() {
    let h = harness().await;
    let src = tid("src");
    h.engine.ensure_thread(&src, &scope(), None).await.unwrap();
    h.rt.history.lock().unwrap().insert(
        src.clone(),
        vec![
            msg(HistoryRole::User, "plan it"),
            msg(HistoryRole::Assistant, "the plan"),
        ],
    );
    let source = h.engine.store().thread(&src).await.unwrap().unwrap();
    let fork = h
        .engine
        .fork(&ctx("src"), &source, None, None, None, Author::User)
        .await
        .unwrap();
    assert_eq!(fork["strategy"], "portable_context");
    let fork_id = tid(fork["forkThreadId"].as_str().unwrap());
    h.engine.tick().await;
    let handoff = h.rt.starts_on(&fork_id);
    assert_eq!(handoff.len(), 1);
    assert!(handoff[0].contains("the plan"));
    finish(&h.engine, &fork_id, TurnOutcome::completed(None)).await;
    h.rt.history
        .lock()
        .unwrap()
        .get_mut(&fork_id)
        .unwrap()
        .extend([
            msg(HistoryRole::Assistant, "ack"),
            msg(HistoryRole::User, "try B"),
            msg(HistoryRole::Assistant, "B works"),
        ]);
    let fork_row = h.engine.store().thread(&fork_id).await.unwrap().unwrap();
    let merge = h
        .engine
        .merge_back(&ctx("src"), &fork_row, None, None, None, Author::User)
        .await
        .unwrap();
    assert_eq!(merge["targetThreadId"], "src");
    h.engine.tick().await;
    let merged = h.rt.starts_on(&src);
    assert_eq!(merged.len(), 1);
    assert!(merged[0].contains("B works") && !merged[0].contains("the plan"));
}

#[tokio::test]
async fn legacy_threads_import_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("l.db"))
            .build()
            .await
            .unwrap(),
    );
    let conn = db.connect().unwrap();
    conn.execute_batch(
        "CREATE TABLE root_filesystem_entries (path TEXT PRIMARY KEY, contents BLOB, is_dir INTEGER DEFAULT 0, kind TEXT);
         INSERT INTO root_filesystem_entries VALUES ('/x/thread.json', CAST('{\"scope\":{\"tenant_id\":\"t1\",\"agent_id\":\"ag\",\"owner_user_id\":\"u1\"},\"thread_id\":\"old-1\",\"title\":\"Hello\",\"created_at\":\"2026-10-22T21:44:54.063018Z\",\"updated_at\":\"2026-10-22T21:44:54.063018Z\"}' AS BLOB), 0, 'session_thread');",
    )
    .await
    .unwrap();
    let rt = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
    let store = Store::new(rt);
    let id = RuntimeId::new("ironclaw").unwrap();
    assert_eq!(store.migrate(&id).await.unwrap(), 1);
    // A legacy row written after the import is not picked up: the second run is a no-op.
    conn.execute_batch(
        "INSERT INTO root_filesystem_entries VALUES ('/y/thread.json', CAST('{\"scope\":{\"tenant_id\":\"t1\",\"owner_user_id\":\"u1\"},\"thread_id\":\"old-2\",\"created_at\":\"2026-10-22T21:44:54Z\",\"updated_at\":\"2026-10-22T21:44:54Z\"}' AS BLOB), 0, 'session_thread');",
    )
    .await
    .unwrap();
    assert_eq!(store.migrate(&id).await.unwrap(), 0);
    assert!(store.thread(&tid("old-2")).await.unwrap().is_none());
    assert_eq!(
        store
            .meta("legacy_threads_imported_v1")
            .await
            .unwrap()
            .as_deref(),
        Some("1")
    );
    let t = store.thread(&tid("old-1")).await.unwrap().unwrap();
    assert_eq!(t.title.as_deref(), Some("Hello"));
    assert_eq!(t.scope.user_id.as_str(), "u1");
    assert!(t.created_at > 1_700_000_000_000);
    assert!(t.profile_id.is_none());
    assert_eq!(t.runtime_id.as_str(), "ironclaw");
    // The import created the owner's default profile; the thread resolves to it.
    let profile = store.thread_profile(&tid("old-1")).await.unwrap().unwrap();
    assert!(profile.is_default());
    // The transcript source is untouched.
    let mut rows = conn
        .query("SELECT COUNT(*) FROM root_filesystem_entries", ())
        .await
        .unwrap();
    let count: i64 = rows.next().await.unwrap().unwrap().get(0).unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
async fn profiles_own_threads_for_life_and_steer_delegation() {
    use crate::profiles::{ProfileOwner, parse_profile_slug};
    use ironclaw_product_contracts::profiles::ProfileSettings;

    let h = harness().await;
    let store = h.engine.store();
    let owner = ProfileOwner::from(&scope());
    let work = parse_profile_slug("work").unwrap();
    let settings = ProfileSettings {
        display_name: "Work".into(),
        soul: "be terse".into(),
        ..ProfileSettings::default()
    };
    store
        .create_profile(&owner, &work, &settings)
        .await
        .unwrap();
    let dup = store.create_profile(&owner, &work, &settings).await;
    assert_eq!(dup.unwrap_err().code(), "profile_exists");

    // A thread keeps its first profile; one without a profile runs as default.
    let (a, b) = (tid("a"), tid("b"));
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    h.engine.ensure_thread(&b, &scope(), None).await.unwrap();
    assert!(store.set_thread_profile(&a, &work).await.unwrap());
    let default = parse_profile_slug("default").unwrap();
    assert!(!store.set_thread_profile(&a, &default).await.unwrap());
    let soul = store
        .thread_profile(&a)
        .await
        .unwrap()
        .unwrap()
        .settings
        .soul;
    assert_eq!(soul, "be terse");

    // Last use orders the picker.
    h.engine.on_user_prompt(&a).await.unwrap();
    let list = store.profiles(&owner).await.unwrap();
    assert_eq!(list[0].profile_id, work);
    assert_eq!(list.len(), 2);
    let b_profile = store.thread_profile(&b).await.unwrap().unwrap();
    assert!(
        b_profile.is_default(),
        "a thread without a profile runs as default"
    );

    // Children inherit the parent's profile unless the target names another.
    let p = tid("p");
    h.engine.ensure_thread(&p, &scope(), None).await.unwrap();
    store.set_thread_profile(&p, &work).await.unwrap();
    let inherit = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(json!({"task": "x"})).unwrap(),
        )
        .await
        .unwrap();
    let child = tid(inherit["childThreadId"].as_str().unwrap());
    let child_profile = store.thread(&child).await.unwrap().unwrap().profile_id;
    assert_eq!(child_profile, Some(work.clone()));
    let unknown = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(json!({"task": "y", "target": {"profile": "nope"}})).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), "profile_unavailable");

    // Archiving hides the profile; its threads still resolve it.
    store.archive_profile(&owner, &work).await.unwrap();
    assert_eq!(store.profiles(&owner).await.unwrap().len(), 1);
    assert!(store.thread_profile(&a).await.unwrap().is_some());
    let protected = store.archive_profile(&owner, &default).await.unwrap_err();
    assert_eq!(protected.code(), "profile_protected");
}

#[tokio::test]
async fn profiles_run_on_their_providers_runtime() {
    use crate::profile_runtime::UserTurn;
    use crate::profiles::{ProfileOwner, parse_profile_slug};
    use ironclaw_product_contracts::profiles::ProfileSettings;

    let h = harness().await;
    let cli = Arc::new(FakeRuntime {
        name: Some("cli"),
        ..FakeRuntime::default()
    });
    h.engine.register_runtime(cli.clone());
    let store = h.engine.store();
    let owner = ProfileOwner::from(&scope());
    let coder = parse_profile_slug("coder").unwrap();
    let settings = ProfileSettings {
        display_name: "Coder".into(),
        default_provider: Some("cli".into()),
        default_model: Some("m2".into()),
        ..ProfileSettings::default()
    };
    store
        .create_profile(&owner, &coder, &settings)
        .await
        .unwrap();

    // Delegating to the profile without naming a provider uses its engine.
    let p = tid("p");
    h.engine.ensure_thread(&p, &scope(), None).await.unwrap();
    let task = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(json!({"task": "x", "target": {"profile": "coder"}})).unwrap(),
        )
        .await
        .unwrap();
    let child = store
        .thread(&tid(task["childThreadId"].as_str().unwrap()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.runtime_id.as_str(), "cli");
    assert_eq!(child.model.as_deref(), Some("m2"));
    assert_eq!(cli.starts_on(&child.thread_id).len(), 1);
    assert!(h.rt.starts_on(&child.thread_id).is_empty());

    // A new thread on the profile moves to that runtime; user turns go there.
    let t = tid("t");
    h.engine.ensure_thread(&t, &scope(), None).await.unwrap();
    store.set_thread_profile(&t, &coder).await.unwrap();
    let profile = store.thread_profile(&t).await.unwrap().unwrap();
    assert!(h.engine.bind_profile_runtime(&t, &profile).await.unwrap());
    let bound = h.engine.external_thread(&t).await.unwrap().unwrap();
    assert_eq!(bound.native_session_id.as_deref(), Some("session-2"));
    let turn = h.engine.user_turn(&bound, "hello").await.unwrap();
    assert!(matches!(turn, UserTurn::Started { .. }));
    assert_eq!(cli.starts_on(&t), vec!["hello".to_string()]);
    assert!(
        h.rt.starts_on(&t).is_empty(),
        "IronClaw's loop never sees it"
    );
    // Busy: the next message waits in the queue.
    let busy = h.engine.user_turn(&bound, "again").await.unwrap();
    assert!(matches!(busy, UserTurn::Queued { .. }));
}

/// Fires as soon as it is created, then hourly; refuses prompts containing "evil".
struct FakePlanner;

impl crate::schedules::SchedulePlanner for FakePlanner {
    fn normalize(
        &self,
        raw: &serde_json::Value,
    ) -> Result<serde_json::Value, crate::error::OrchestratorError> {
        Ok(raw.clone())
    }
    fn next_after(
        &self,
        _spec: &serde_json::Value,
        last_slot: Option<Millis>,
        now: Millis,
    ) -> Result<Option<Millis>, crate::error::OrchestratorError> {
        Ok(Some(if last_slot.is_some() {
            now + 3_600_000
        } else {
            now
        }))
    }
    fn describe(&self, _spec: &serde_json::Value) -> String {
        "test".into()
    }
    fn check_prompt(&self, prompt: &str) -> Result<(), crate::error::OrchestratorError> {
        if prompt.contains("evil") {
            return Err(crate::error::OrchestratorError::invalid("unsafe prompt"));
        }
        Ok(())
    }
}

#[tokio::test]
async fn schedule_fires_into_a_busy_thread_as_a_queued_item() {
    let h = harness().await;
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    let off = h
        .engine
        .call_tool(
            "t3_schedule",
            &ctx("a"),
            &json!({"action": "create", "prompt": "x", "schedule": {}}),
        )
        .await;
    assert_eq!(off["error"], "schedules_unavailable");
    h.engine.set_schedule_planner(Arc::new(FakePlanner));

    let bad = json!({"action": "create", "prompt": "evil plan", "schedule": {"type": "interval"}});
    assert_eq!(
        h.engine.call_tool("t3_schedule", &ctx("a"), &bad).await["ok"],
        false
    );

    h.engine
        .store()
        .set_turn_running(&a, Some("r0"), now_ms())
        .await
        .unwrap();
    let create = json!({"action": "create", "prompt": "check mail", "title": "Mail",
                        "schedule": {"type": "interval", "everyMs": 3600000}});
    let created = h.engine.call_tool("t3_schedule", &ctx("a"), &create).await;
    assert_eq!(created["ok"], true, "{created}");
    assert!(created["schedule"]["nextRunAt"].is_i64());

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    h.engine.tick().await;
    h.engine.tick().await; // next slot is an hour away: no second fire
    assert!(h.rt.starts_on(&a).is_empty(), "busy thread must not start");
    let queue = h.engine.store().queue(&a, false).await.unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].created_by, Author::Schedule);
    assert!(queue[0].text.contains("check mail"));

    finish(&h.engine, &a, TurnOutcome::completed(None)).await;
    h.engine.tick().await;
    let started = h.rt.starts_on(&a);
    assert_eq!(started.len(), 1);
    assert!(started[0].contains("[Scheduled: Mail]"));

    let list = h
        .engine
        .call_tool("t3_schedule", &ctx("a"), &json!({"action": "list"}))
        .await;
    assert_eq!(list["schedules"][0]["runCount"], 1);
    // Another user's thread cannot touch it.
    let id = list["schedules"][0]["id"].as_str().unwrap().to_string();
    let mut other = ctx("a");
    other.scope.user_id = UserId::new("u2").unwrap();
    let denied = h
        .engine
        .call_tool(
            "t3_schedule",
            &other,
            &json!({"action": "delete", "id": id}),
        )
        .await;
    assert_eq!(denied["error"], "schedule_not_found");
}
