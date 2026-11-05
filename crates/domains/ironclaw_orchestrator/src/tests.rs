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
    history_reads: Mutex<u32>,
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
        at: None,
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
                efforts: vec!["low".into(), "high".into()],
                model_efforts: [("m2".to_string(), vec!["low".to_string()])].into(),
                brand: None,
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
    fn compaction(&self) -> RuntimeCompaction {
        RuntimeCompaction {
            manual_prompt: self.name.map(|_| "/compact"),
            automatic: false,
        }
    }
    fn injected_context(
        &self,
        _profile: Option<&TurnProfile>,
        _goal: Option<&str>,
    ) -> crate::context_report::ContextSample {
        let mut sample = crate::context_report::ContextSample::default();
        if self.name.is_some() {
            sample.add(
                ironclaw_product_contracts::orchestration::ContextCategory::Instructions,
                300,
            );
            sample.add_tool("mcp-ironclaw.t3_delegate", 2_000);
        }
        sample
    }
    async fn history(
        &self,
        t: &ThreadRecord,
        _limit: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        *self.history_reads.lock().unwrap() += 1;
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
    // A subagent that did not finish its task stays in the inbox.
    let owner = crate::profiles::ProfileOwner::from(&scope());
    let records = h.engine.store().inbox_records(&owner).await.unwrap();
    assert!(
        records
            .get(grand_thread.as_str())
            .is_none_or(|r| r.settled_at.is_none()),
        "a cancelled subagent is not auto-settled"
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
    // The finished subagent's thread settles by itself; the parent stays.
    let owner = crate::profiles::ProfileOwner::from(&scope());
    let records = h.engine.store().inbox_records(&owner).await.unwrap();
    assert!(
        records
            .get(child.as_str())
            .is_some_and(|r| r.settled_at.is_some()),
        "a completed subagent is auto-settled: {records:?}"
    );
    assert!(!records.contains_key("p"), "the parent is left alone");
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

/// The two kinds of delegation: a subagent on this task gets the parent's
/// conversation ahead of its task; a side thread gets only its brief.
#[tokio::test]
async fn a_subagent_inherits_the_conversation_and_a_side_thread_gets_only_its_brief() {
    let h = harness().await;
    let p = tid("p");
    h.engine.ensure_thread(&p, &scope(), None).await.unwrap();
    h.rt.history.lock().unwrap().insert(
        p.clone(),
        vec![
            msg(HistoryRole::User, "We are fixing the flaky inbox poller."),
            msg(HistoryRole::Assistant, "The poller backs off to 2 s."),
        ],
    );
    let brief_of = |task: &serde_json::Value| {
        let child = tid(task["childThreadId"].as_str().unwrap());
        h.rt.starts_on(&child).join("\n")
    };

    let sub = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(
                json!({"task": "Review the back-off.", "context": "inherit", "target": {"model": "m2"}}),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let brief = brief_of(&sub);
    assert!(brief.contains("conversation so far is below"), "{brief}");
    assert!(
        brief.contains("flaky inbox poller") && brief.contains("backs off to 2 s"),
        "{brief}"
    );
    assert!(brief.ends_with("Review the back-off."), "{brief}");
    assert_eq!(sub["model"], "m2");

    let side = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(json!({"task": "Summarize the README."})).unwrap(),
        )
        .await
        .unwrap();
    let brief = brief_of(&side);
    assert!(brief.contains("conversation is not available"), "{brief}");
    assert!(!brief.contains("flaky inbox poller"), "{brief}");

    let err = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(json!({"task": "x", "context": "everything"})).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_request");
}

#[tokio::test]
async fn a_reply_after_cancel_or_a_follow_up_still_reaches_the_parent() {
    let h = harness().await;
    let task = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(json!({"task": "review it"})).unwrap(),
        )
        .await
        .unwrap();
    let id = task["taskId"].as_str().unwrap().to_string();
    let child = tid(task["childThreadId"].as_str().unwrap());
    // The cancel finalizes without a result (the child looked idle) ...
    h.engine
        .store()
        .set_turn_ended(&child, None, TurnState::Idle, None, now_ms())
        .await
        .unwrap();
    h.engine
        .cancel_task(&ctx("p"), &TaskId::new(&id).unwrap(), "stop")
        .await
        .unwrap();
    assert_eq!(task_status(&h.engine, &id).await, TaskStatus::Cancelled);
    h.engine.tick().await;
    // ... and the run it could not stop finishes anyway.
    finish(
        &h.engine,
        &child,
        TurnOutcome::completed(Some("late review".into())),
    )
    .await;
    finish(&h.engine, &tid("p"), TurnOutcome::completed(None)).await;
    h.engine.tick().await;
    let cards = h.rt.starts_on(&tid("p"));
    assert!(
        cards.last().is_some_and(|c| c.contains("late review")),
        "the late reply is delivered: {cards:?}"
    );

    // A follow-up the person asks for in the child chat comes back too.
    finish(&h.engine, &tid("p"), TurnOutcome::completed(None)).await;
    finish(
        &h.engine,
        &child,
        TurnOutcome::completed(Some("second pass".into())),
    )
    .await;
    h.engine.tick().await;
    let cards = h.rt.starts_on(&tid("p"));
    assert!(cards.last().is_some_and(|c| c.contains("second pass")));
}

#[test]
fn brief_paths_finds_absolute_and_home_paths() {
    let brief = "Read `/Users/me/Downloads/My Page.html` and ~/notes/a.md, \
                 then report. See (/tmp/x.txt). Not this: relative/a.md";
    let paths: Vec<String> = crate::delegation::brief_paths(brief)
        .into_iter()
        .map(|p| p.display().to_string())
        .collect();
    assert_eq!(
        paths,
        [
            "/Users/me/Downloads/My Page.html",
            "~/notes/a.md",
            "/tmp/x.txt"
        ]
    );
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
    // A runtime that reclaims the cut run on boot is stopped, not followed.
    reopened.turn_started(&q, Some("r3".into()), now_ms()).await;
    assert_eq!(*h.rt.cancelled.lock().unwrap(), vec![q.clone()]);
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
    // Nothing runs in the fork until the person sends a message there.
    h.engine.tick().await;
    assert!(h.rt.starts_on(&fork_id).is_empty());
    let context = h
        .engine
        .take_next_message_context(&fork_id)
        .await
        .unwrap()
        .unwrap();
    assert!(context.contains("the plan"));
    assert!(
        h.engine
            .take_next_message_context(&fork_id)
            .await
            .unwrap()
            .is_none()
    );
    h.rt.history.lock().unwrap().insert(
        fork_id.clone(),
        vec![
            msg(HistoryRole::User, &format!("{context}\n\n---\n\ntry B")),
            msg(HistoryRole::Assistant, "B works"),
        ],
    );
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
    assert!(merged[0].contains("try B") && merged[0].contains("B works"));
    assert!(!merged[0].contains("the plan"));
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

    // A subagent that inherits this conversation keeps this thread's profile.
    let err = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(
                json!({"task": "x", "context": "inherit", "target": {"profile": "coder"}}),
            )
            .unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "profile_unavailable");

    // A new thread on the profile moves to that runtime; user turns go there.
    let t = tid("t");
    h.engine.ensure_thread(&t, &scope(), None).await.unwrap();
    store.set_thread_profile(&t, &coder).await.unwrap();
    let profile = store.thread_profile(&t).await.unwrap().unwrap();
    assert!(h.engine.bind_profile_runtime(&t, &profile).await.unwrap());
    let bound = h.engine.external_thread(&t).await.unwrap().unwrap();
    assert_eq!(bound.native_session_id.as_deref(), Some("session-2"));

    // A profile whose provider lives on the thread's runtime still pins its
    // model (the Default profile on a provider of IronClaw's own loop).
    let same = tid("same");
    h.engine.ensure_thread(&same, &scope(), None).await.unwrap();
    store.set_thread_profile(&same, &coder).await.unwrap();
    assert!(
        h.engine
            .bind_profile_runtime(&same, &profile)
            .await
            .unwrap()
    );
    let mut on_m1 = profile.clone();
    on_m1.settings.default_model = Some("m1".into());
    assert!(h.engine.bind_profile_runtime(&same, &on_m1).await.unwrap());
    let pinned = store.thread(&same).await.unwrap().unwrap();
    assert_eq!(pinned.runtime_id.as_str(), "cli");
    assert_eq!(pinned.provider.as_deref(), Some("cli"));
    assert_eq!(pinned.model.as_deref(), Some("m1"));

    let turn = h.engine.user_turn(&bound, "hello").await.unwrap();
    assert!(matches!(turn, UserTurn::Started { .. }));
    assert_eq!(cli.starts_on(&t), vec!["hello".to_string()]);
    assert!(
        h.rt.starts_on(&t).is_empty(),
        "IronClaw's loop never sees it"
    );
    // Busy: the next message steers the running turn instead of restarting it.
    let busy = h.engine.user_turn(&bound, "again").await.unwrap();
    assert!(matches!(busy, UserTurn::Steered { .. }));
    assert_eq!(cli.starts_on(&t), vec!["hello".to_string()]);
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

/// `bind_to_current_thread=false` gives every run its own thread on the
/// schedule thread's engine; webhook schedules run per request to their
/// URL, check the signature and the safety scan, and quote the request.
#[tokio::test]
async fn schedules_run_in_fresh_threads_and_on_webhooks() {
    use crate::schedule_webhooks::{WebhookRefusal, WebhookRequest};
    use hmac::{Hmac, KeyInit, Mac};

    let h = harness().await;
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    h.engine.set_schedule_planner(Arc::new(FakePlanner));
    h.engine.set_webhook_base("http://127.0.0.1:9".into());

    // Fresh thread per run.
    let fresh = h
        .engine
        .call_tool(
            "t3_schedule",
            &ctx("a"),
            &json!({"action": "create", "prompt": "daily digest", "title": "Digest",
                    "bind_to_current_thread": false,
                    "schedule": {"type": "interval", "everyMs": 3600000}}),
        )
        .await;
    assert_eq!(fresh["schedule"]["bindToCurrentThread"], false, "{fresh}");
    let id = fresh["schedule"]["id"].as_str().unwrap().to_string();
    let ran = h
        .engine
        .call_tool(
            "t3_schedule",
            &ctx("a"),
            &json!({"action": "run_now", "id": id}),
        )
        .await;
    let run_thread = tid(ran["runThreadId"].as_str().unwrap());
    assert_ne!(run_thread, a);
    let record = h.engine.store().thread(&run_thread).await.unwrap().unwrap();
    assert_eq!(record.title.as_deref(), Some("⏰ Digest"));
    assert_eq!(record.scope, scope());
    assert!(h.engine.store().queue(&a, false).await.unwrap().is_empty());
    h.engine.tick().await;
    assert!(h.rt.starts_on(&run_thread)[0].contains("daily digest"));

    // Webhook with a signature.
    let hook = h
        .engine
        .call_tool(
            "t3_schedule",
            &ctx("a"),
            &json!({"action": "create", "title": "Push",
                    "prompt": "New push to {{body.repo}} by {{headers.x-user}}",
                    "schedule": {"type": "webhook"},
                    "signature": {"header": "X-Sig", "prefix": "sha256=", "secret": "k"}}),
        )
        .await;
    assert_eq!(hook["ok"], true, "{hook}");
    assert!(hook["schedule"]["nextRunAt"].is_null());
    assert_eq!(hook["schedule"]["signature"]["hasSecret"], true);
    assert!(
        !hook.to_string().contains("\"k\""),
        "secret never shown: {hook}"
    );
    let url = hook["schedule"]["webhookUrl"].as_str().unwrap();
    let key = url
        .strip_prefix("http://127.0.0.1:9/api/orchestrator/webhooks/")
        .unwrap();

    let signed = |body: &str| {
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"k").unwrap();
        mac.update(body.as_bytes());
        WebhookRequest {
            method: "POST".into(),
            path: "/".into(),
            headers: vec![
                ("x-user".into(), "lorik".into()),
                (
                    "x-sig".into(),
                    format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
                ),
            ],
            query: vec![],
            body: body.as_bytes().to_vec(),
        }
    };
    let good = signed(r#"{"repo":"ironclaw"}"#);
    let (hook_id, _) = key.split_once('.').unwrap();
    assert_eq!(
        h.engine
            .fire_webhook(&format!("{hook_id}.wrong"), &good)
            .await,
        Err(WebhookRefusal::NotFound)
    );
    let mut forged = good.clone();
    forged.body = br#"{"repo":"other"}"#.to_vec();
    assert_eq!(
        h.engine.fire_webhook(key, &forged).await,
        Err(WebhookRefusal::Unauthorized)
    );
    assert_eq!(
        h.engine
            .fire_webhook(key, &signed(r#"{"repo":"evil"}"#))
            .await,
        Err(WebhookRefusal::Rejected),
        "the rendered prompt goes through the safety scan"
    );
    let fired = h.engine.fire_webhook(key, &good).await.unwrap();
    assert_eq!(fired["threadId"], "a");
    let queue = h.engine.store().queue(&a, false).await.unwrap();
    assert!(
        queue[0].text.contains("New push to ironclaw by lorik"),
        "{}",
        queue[0].text
    );

    // Paused: refused, nothing queued.
    h.engine
        .call_tool(
            "t3_schedule",
            &ctx("a"),
            &json!({"action": "update", "id": hook_id, "enabled": false}),
        )
        .await;
    assert_eq!(
        h.engine.fire_webhook(key, &good).await,
        Err(WebhookRefusal::Paused)
    );
    assert_eq!(h.engine.store().queue(&a, false).await.unwrap().len(), 1);
}

/// A database from before the webhook/fresh-thread columns opens and works.
#[tokio::test]
async fn old_schedule_tables_gain_the_new_columns() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    db.connect()
        .unwrap()
        .execute_batch(
            "CREATE TABLE orch_schedules (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, \
             user_id TEXT NOT NULL, thread_id TEXT NOT NULL, title TEXT NOT NULL, \
             prompt TEXT NOT NULL, spec TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, \
             next_run_at INTEGER, last_run_at INTEGER, run_count INTEGER NOT NULL DEFAULT 0, \
             last_error TEXT, client_request_id TEXT, created_at INTEGER NOT NULL, \
             updated_at INTEGER NOT NULL); \
             INSERT INTO orch_schedules (id, tenant_id, user_id, thread_id, title, prompt, spec, \
             created_at, updated_at) VALUES ('s1', 't1', 'u1', 'a', 'Old', 'p', '{}', 1, 1);",
        )
        .await
        .unwrap();
    let engine = open_engine(&db).await;
    let _again = open_engine(&db).await; // idempotent
    let old = engine
        .schedule(&ScheduleId::new("s1").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(!old.new_thread && old.webhook_token.is_none());
}

#[tokio::test]
async fn route_switch_rebinds_or_switches_in_place_with_handoff() {
    let h = harness().await;
    let cli = Arc::new(FakeRuntime {
        name: Some("cli"),
        ..Default::default()
    });
    h.engine.register_runtime(cli.clone());
    let store = h.engine.store();
    let target = |provider: &str, model: &str| crate::delegation::Target {
        provider: Some(provider.into()),
        model: Some(model.into()),
        ..Default::default()
    };

    // An unused thread moves runtime; the same runtime switches in place.
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    let thread = store.thread(&a).await.unwrap().unwrap();
    let r = h
        .engine
        .set_route(&thread, &target("cli", "m1"))
        .await
        .unwrap();
    assert_eq!(r["moved"], "rebound");
    let thread = store.thread(&a).await.unwrap().unwrap();
    assert_eq!(thread.runtime_id.as_str(), "cli");
    let r = h
        .engine
        .set_route(&thread, &target("cli", "m2"))
        .await
        .unwrap();
    assert_eq!(r["moved"], "in_place");
    let thread = store.thread(&a).await.unwrap().unwrap();
    assert_eq!(thread.model.as_deref(), Some("m2"));
    assert_eq!(thread.native_session_id.as_deref(), Some("session-1"));

    // History on IronClaw's loop: the thread stays itself and moves to the
    // CLI, whose first turn gets the whole conversation.
    let b = tid("b");
    h.engine.ensure_thread(&b, &scope(), None).await.unwrap();
    h.engine.on_user_prompt(&b).await.unwrap();
    h.rt.history.lock().unwrap().insert(
        b.clone(),
        vec![
            msg(HistoryRole::User, "hello"),
            msg(HistoryRole::Assistant, "hi there"),
        ],
    );
    let thread = store.thread(&b).await.unwrap().unwrap();
    let r = h
        .engine
        .set_route(&thread, &target("cli", "m1"))
        .await
        .unwrap();
    assert_eq!(r["moved"], "switched", "{r}");
    assert!(r["forkThreadId"].is_null());
    let thread = store.thread(&b).await.unwrap().unwrap();
    assert_eq!(thread.runtime_id.as_str(), "cli");
    assert_eq!(thread.model.as_deref(), Some("m1"));
    let context = h
        .engine
        .take_next_message_context(&b)
        .await
        .unwrap()
        .unwrap();
    assert!(context.starts_with("Context handoff (provider switch)"));
    assert!(context.contains("hello") && context.contains("hi there"));

    // The CLI turn sees the handoff ahead of the typed text; the stitched
    // transcript shows only what the user typed.
    cli.history.lock().unwrap().insert(
        b.clone(),
        vec![
            msg(
                HistoryRole::User,
                &format!("{context}\n\n---\n\nnext question"),
            ),
            msg(HistoryRole::Assistant, "cli answer"),
        ],
    );
    let texts: Vec<String> = h
        .engine
        .history(&thread)
        .await
        .unwrap()
        .into_iter()
        .map(|m| m.text)
        .collect();
    assert_eq!(texts, ["hello", "hi there", "next question", "cli answer"]);

    // Back to the loop: it resumes its own transcript and gets the delta only.
    let back = crate::delegation::Target {
        model: Some("default".into()),
        ..Default::default()
    };
    let r = h.engine.set_route(&thread, &back).await.unwrap();
    assert_eq!(r["moved"], "switched", "{r}");
    assert_eq!(r["resumed"], true);
    let delta = h
        .engine
        .take_next_message_context(&b)
        .await
        .unwrap()
        .unwrap();
    assert!(delta.contains("next question") && delta.contains("cli answer"));
    assert!(
        !delta.contains("hello"),
        "the loop already holds its own turns"
    );
    let segments = h.engine.segments(&b).await.unwrap();
    assert_eq!(segments.len(), 3);
    assert_eq!(segments[2].runtime_id.as_str(), "fake");
    assert_eq!((segments[2].start, segments[1].end), (2, Some(2)));
    assert_eq!(segments[1].marker(), "Switched to m1 · cli");

    // A detour without a turn leaves no segment behind.
    let thread = store.thread(&b).await.unwrap().unwrap();
    h.engine
        .set_route(&thread, &target("cli", "m1"))
        .await
        .unwrap();
    let thread = store.thread(&b).await.unwrap().unwrap();
    h.engine.set_route(&thread, &back).await.unwrap();
    assert_eq!(h.engine.segments(&b).await.unwrap().len(), 3);
}

#[tokio::test]
async fn model_picker_prefs_persist_deduplicated_and_ride_the_route_view() {
    let h = harness().await;
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    let thread = h.engine.store().thread(&a).await.unwrap().unwrap();
    let fav = json!({"provider": "glm", "model": "glm-5.3"});
    let r = h
        .engine
        .set_model_picker(json!({
            "hiddenProviders": ["inception", "inception"],
            "hiddenModels": [{"provider": "glm", "model": "glm-4.5"}],
            "favorites": [fav, fav],
        }))
        .await
        .unwrap();
    assert_eq!(r["picker"]["hiddenProviders"], json!(["inception"]));
    assert_eq!(r["picker"]["favorites"], json!([fav]));

    let view = h.engine.route_view(&thread).await.unwrap();
    assert_eq!(view["picker"], r["picker"]);
    // Screens without a thread get the same lists.
    let bare = h.engine.providers_view(&scope()).await.unwrap();
    assert_eq!(
        (&bare["providers"], &bare["picker"]),
        (&view["providers"], &view["picker"])
    );
    assert!(bare.get("threadId").is_none());
    assert!(
        h.engine
            .set_model_picker(json!({"hiddenProviders": [""]}))
            .await
            .is_err(),
        "empty ids are rejected"
    );
}

#[tokio::test]
async fn profile_defaults_must_name_an_offered_provider_and_model() {
    use ironclaw_product_contracts::profiles::ProfileSettings;
    let h = harness().await;
    h.engine.register_runtime(Arc::new(FakeRuntime {
        name: Some("cli"),
        ..Default::default()
    }));
    let defaults = |p: &str, m: &str| ProfileSettings {
        default_provider: Some(p.into()),
        default_model: Some(m.into()),
        ..ProfileSettings::default()
    };
    let check = |s: ProfileSettings| {
        let engine = h.engine.clone();
        async move { engine.check_profile_defaults(&s, &scope()).await }
    };
    assert!(check(defaults("cli", "m2")).await.is_ok());
    assert!(check(ProfileSettings::default()).await.is_ok());
    assert!(check(defaults("openai-codex", "m1")).await.is_err());
    assert!(check(defaults("cli", "m9")).await.is_err());
}

#[tokio::test]
async fn an_effort_pick_rides_the_route_and_a_switch_keeps_only_its_own() {
    let h = harness().await;
    let cli = Arc::new(FakeRuntime {
        name: Some("cli"),
        ..Default::default()
    });
    h.engine.register_runtime(cli.clone());
    let store = h.engine.store();
    let a = tid("effort");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    let thread = store.thread(&a).await.unwrap().unwrap();
    let effort = |e: &str| crate::delegation::Target {
        reasoning_effort: Some(e.into()),
        ..Default::default()
    };

    let r = h.engine.set_route(&thread, &effort("high")).await.unwrap();
    assert_eq!(
        (r["moved"].as_str(), r["effort"].as_str()),
        (Some("in_place"), Some("high"))
    );
    let view = h.engine.route_view(&thread).await.unwrap();
    assert_eq!(view["effort"], "high");
    let cli_row = view["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider"] == "cli")
        .unwrap();
    assert_eq!(cli_row["efforts"], serde_json::json!(["low", "high"]));
    assert_eq!(
        cli_row["modelEfforts"],
        serde_json::json!({ "m2": ["low"] })
    );

    // A switch without an effort clears it; `default` clears it too.
    let switch = crate::delegation::Target {
        provider: Some("cli".into()),
        model: Some("m1".into()),
        ..Default::default()
    };
    h.engine.set_route(&thread, &switch).await.unwrap();
    assert_eq!(store.thread_effort(&a).await.unwrap(), None);
    let thread = store.thread(&a).await.unwrap().unwrap();
    h.engine.set_route(&thread, &effort("low")).await.unwrap();
    assert_eq!(
        store.thread_effort(&a).await.unwrap().as_deref(),
        Some("low")
    );
    h.engine
        .set_route(&thread, &effort("default"))
        .await
        .unwrap();
    assert_eq!(store.thread_effort(&a).await.unwrap(), None);
}

#[tokio::test]
async fn cli_context_reports_sum_to_the_provider_count_and_reuse_the_turns_estimate() {
    use crate::telemetry::TurnTelemetry;
    use ironclaw_product_contracts::orchestration::{
        ContextCategory, ContextLevel, ContextSource, ContextToolOwner,
    };
    let h = harness().await;
    let cli = Arc::new(FakeRuntime {
        name: Some("cli"),
        ..Default::default()
    });
    h.engine.register_runtime(cli.clone());
    let a = tid("a");
    let thread = h
        .engine
        .ensure_thread(&a, &scope(), Some(&RuntimeId::new("cli").unwrap()))
        .await
        .unwrap();
    cli.history.lock().unwrap().insert(
        a.clone(),
        vec![
            msg(HistoryRole::User, &"question ".repeat(200)),
            msg(HistoryRole::Assistant, &"answer ".repeat(100)),
        ],
    );

    // Before any turn reports, the whole report is an estimate.
    let r = h.engine.context_report(&thread).await.unwrap();
    assert_eq!(r.source, ContextSource::Estimated);
    assert_eq!(r.categories.iter().map(|c| c.tokens).sum::<u64>(), r.used);
    assert_eq!(r.compact_prompt.as_deref(), Some("/compact"));
    assert!(!r.auto_compacts);

    let report = |used: u64| TurnTelemetry {
        context_used: Some(used),
        context_max: Some(10_000),
        ..TurnTelemetry::default()
    };
    h.engine.turn_telemetry(&a, report(8_500), 1_000).await;
    let r = h.engine.context_report(&thread).await.unwrap();
    assert_eq!((r.used, r.max), (8_500, Some(10_000)));
    assert_eq!(r.source, ContextSource::Provider);
    assert_eq!(r.level, ContextLevel::Warning);
    assert_eq!(r.categories.iter().map(|c| c.tokens).sum::<u64>(), 8_500);
    assert!(
        r.categories
            .iter()
            .any(|c| c.category == ContextCategory::Unattributed)
    );
    assert_eq!(
        r.tool_owners[0].owner,
        ContextToolOwner::Mcp("ironclaw".into())
    );
    let reads = *cli.history_reads.lock().unwrap();

    // The same turn: the stored estimate is reused, not recomputed.
    h.engine.context_report(&thread).await.unwrap();
    assert_eq!(*cli.history_reads.lock().unwrap(), reads);

    // A new turn reports: estimated again, and over the window.
    h.engine.turn_telemetry(&a, report(12_000), 2_000).await;
    let r = h.engine.context_report(&thread).await.unwrap();
    assert_eq!(*cli.history_reads.lock().unwrap(), reads + 1);
    assert_eq!(r.level, ContextLevel::Exceeded);
    assert_eq!(r.categories.iter().map(|c| c.tokens).sum::<u64>(), 12_000);
}

#[tokio::test]
async fn native_context_reports_use_the_recorded_prompt_sample() {
    use crate::context_report::ContextSample;
    use crate::telemetry::TurnTelemetry;
    use ironclaw_product_contracts::orchestration::{ContextCategory, ContextSource};
    let h = harness().await;
    let a = tid("a");
    let thread = h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    let mut sample = ContextSample {
        window: Some(200_000),
        ..ContextSample::default()
    };
    sample.add(ContextCategory::SystemPrompt, 4_000);
    sample.add(ContextCategory::UserMessages, 1_000);
    sample.add_tool("builtin.shell", 8_000);
    h.engine
        .record_context_sample(&a, &sample, 10)
        .await
        .unwrap();
    h.engine
        .turn_telemetry(
            &a,
            TurnTelemetry {
                context_used: Some(6_000),
                ..TurnTelemetry::default()
            },
            20,
        )
        .await;
    let r = h.engine.context_report(&thread).await.unwrap();
    assert_eq!(r.source, ContextSource::Provider);
    assert_eq!(r.max, Some(200_000));
    assert_eq!(r.used, 6_000);
    assert_eq!(r.categories.iter().map(|c| c.tokens).sum::<u64>(), 6_000);
    assert!(r.auto_compacts);
    assert_eq!(r.compact_prompt, None);
    // The transcript is not re-read for native threads.
    assert_eq!(*h.rt.history_reads.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_summary_fork_carries_a_digest_even_where_sessions_branch_natively() {
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
        .fork_from_summary(&ctx("src"), &source, None, Author::User)
        .await
        .unwrap();
    assert_eq!(fork["strategy"], "portable_context");
    assert!(fork["title"].as_str().unwrap().ends_with("(from summary)"));
    let fork_id = tid(fork["forkThreadId"].as_str().unwrap());
    let context = h
        .engine
        .take_next_message_context(&fork_id)
        .await
        .unwrap()
        .unwrap();
    assert!(context.contains("the plan"));
}

#[tokio::test]
async fn summary_fork_chains_stay_inside_one_handoff_budget() {
    let h = harness().await;
    let budget = 6_000;
    h.engine
        .store()
        .set_settings(&json!({ "handoff_bytes": budget }))
        .await
        .unwrap();
    let long = |tag: &str| format!("{tag} {}", "detail ".repeat(300));
    let mut source_id = tid("gen0");
    h.engine
        .ensure_thread(&source_id, &scope(), None)
        .await
        .unwrap();
    for generation in 0..4 {
        h.rt.history.lock().unwrap().insert(
            source_id.clone(),
            (0..6)
                .flat_map(|i| {
                    [
                        msg(HistoryRole::User, &long(&format!("g{generation} ask {i}"))),
                        msg(
                            HistoryRole::Assistant,
                            &long(&format!("g{generation} answer {i}")),
                        ),
                    ]
                })
                .collect(),
        );
        let source = h.engine.store().thread(&source_id).await.unwrap().unwrap();
        let fork = h
            .engine
            .fork_from_summary(&ctx(source_id.as_str()), &source, None, Author::User)
            .await
            .unwrap();
        let fork_id = tid(fork["forkThreadId"].as_str().unwrap());
        let injected = h
            .engine
            .take_next_message_context(&fork_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            injected.len() <= budget,
            "generation {generation}: {} bytes over a {budget}-byte budget",
            injected.len()
        );
        // The newest conversation is always carried.
        assert!(injected.contains(&format!("g{generation} answer 5")));
        source_id = fork_id;
    }
}

#[tokio::test]
async fn the_final_reply_is_not_split_into_the_input_it_followed() {
    use crate::context_report::estimate_tokens;
    use crate::telemetry::TurnTelemetry;
    use ironclaw_product_contracts::orchestration::{ContextCategory, ContextSource};
    let h = harness().await;
    let cli = Arc::new(FakeRuntime {
        name: Some("cli"),
        ..Default::default()
    });
    h.engine.register_runtime(cli.clone());
    let a = tid("a");
    let thread = h
        .engine
        .ensure_thread(&a, &scope(), Some(&RuntimeId::new("cli").unwrap()))
        .await
        .unwrap();
    let question = "a short question";
    let reply = "a very long final answer ".repeat(2_000);
    cli.history.lock().unwrap().insert(
        a.clone(),
        vec![
            msg(HistoryRole::User, question),
            msg(HistoryRole::Assistant, &reply),
        ],
    );
    let tokens = |r: &ironclaw_product_contracts::orchestration::ThreadContextReport,
                  c: ContextCategory| {
        r.categories
            .iter()
            .find(|x| x.category == c)
            .map_or(0, |x| x.tokens)
    };

    // No provider count yet: the estimate is the thread's occupancy, reply
    // included.
    let r = h.engine.context_report(&thread).await.unwrap();
    assert_eq!(r.source, ContextSource::Estimated);
    assert_eq!(
        tokens(&r, ContextCategory::AssistantMessages),
        estimate_tokens(&reply)
    );

    // The provider counted the last call's input: the reply was its output,
    // so it neither shows as input nor squeezes the categories that were.
    h.engine
        .turn_telemetry(
            &a,
            TurnTelemetry {
                context_used: Some(2_000),
                context_max: Some(200_000),
                ..TurnTelemetry::default()
            },
            1_000,
        )
        .await;
    let r = h.engine.context_report(&thread).await.unwrap();
    assert_eq!(r.source, ContextSource::Provider);
    assert_eq!(r.used, 2_000);
    assert_eq!(tokens(&r, ContextCategory::AssistantMessages), 0);
    assert_eq!(
        tokens(&r, ContextCategory::UserMessages),
        estimate_tokens(question)
    );
    assert_eq!(r.categories.iter().map(|c| c.tokens).sum::<u64>(), 2_000);
}

#[tokio::test]
async fn a_compaction_without_a_new_user_turn_refreshes_the_report() {
    use crate::telemetry::TurnTelemetry;
    use ironclaw_product_contracts::orchestration::{ContextLevel, ContextSource};
    let h = harness().await;
    let cli = Arc::new(FakeRuntime {
        name: Some("cli"),
        ..Default::default()
    });
    h.engine.register_runtime(cli.clone());
    let a = tid("a");
    let thread = h
        .engine
        .ensure_thread(&a, &scope(), Some(&RuntimeId::new("cli").unwrap()))
        .await
        .unwrap();
    let old = "old exchange ".repeat(4_000);
    cli.history.lock().unwrap().insert(
        a.clone(),
        vec![
            msg(HistoryRole::User, &old),
            msg(HistoryRole::Assistant, "done"),
        ],
    );
    let telemetry = |used: Option<u64>| TurnTelemetry {
        context_used: used,
        context_max: Some(10_000),
        ..TurnTelemetry::default()
    };
    h.engine
        .turn_telemetry(&a, telemetry(Some(9_000)), 1_000)
        .await;
    let r = h.engine.context_report(&thread).await.unwrap();
    assert_eq!(r.level, ContextLevel::Warning);

    // The idle compactor runs: the transcript gains its marker and summary,
    // and the driver reports what the compaction left. No user turn.
    cli.history.lock().unwrap().get_mut(&a).unwrap().extend([
        msg(HistoryRole::System, "Compacted"),
        HistoryMessage {
            hidden: true,
            ..msg(HistoryRole::Assistant, "summary of the old exchange")
        },
    ]);
    h.engine
        .turn_telemetry(&a, telemetry(Some(900)), 2_000)
        .await;
    let r = h.engine.context_report(&thread).await.unwrap();
    assert_eq!((r.used, r.level), (900, ContextLevel::Normal));
    assert_eq!(r.categories.iter().map(|c| c.tokens).sum::<u64>(), 900);

    // When the CLI gives no post-compaction count the report is an estimate
    // of what is left, labelled so, never the stale exact figure.
    h.engine.turn_telemetry(&a, telemetry(None), 3_000).await;
    let r = h.engine.context_report(&thread).await.unwrap();
    assert_eq!(r.source, ContextSource::Estimated);
    // The fake runtime injects ~800 tokens; the compacted-away exchange
    // alone was ~13k.
    assert!(r.used < 1_000, "estimate after compaction: {}", r.used);
    assert_eq!(r.level, ContextLevel::Normal);
}
