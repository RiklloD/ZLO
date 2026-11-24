//! A delegated task remembers the parent run that launched it, so the
//! parent's transcript can draw the subagent where it started (and again
//! after a restart), and a running child reports its newest words.

use super::*;

#[tokio::test]
async fn a_task_keeps_its_launching_run_and_reports_the_childs_latest_words() {
    let h = harness().await;
    let p = tid("p");
    let mut call = ctx("p");
    call.run_id = Some("run-launch".into());
    let first = h
        .engine
        .delegate(
            &call,
            serde_json::from_value(json!({"task": "look around"})).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first["parentRunId"], "run-launch", "{first}");

    // A caller that does not know its run (an older bridge): the parent
    // thread's running turn stands in.
    h.engine
        .store()
        .set_turn_running(&p, Some("run-active"), now_ms())
        .await
        .unwrap();
    let second = h
        .engine
        .delegate(
            &ctx("p"),
            serde_json::from_value(json!({"task": "look elsewhere"})).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second["parentRunId"], "run-active", "{second}");

    // The running child's newest words, flattened to one line.
    let child = tid(first["childThreadId"].as_str().unwrap());
    h.rt.history.lock().unwrap().insert(
        child.clone(),
        vec![
            HistoryMessage {
                role: HistoryRole::Assistant,
                text: "Older words".into(),
                tool_calls: vec![],
                run_id: None,
                hidden: false,
                at: Some(1),
                steer_id: None,
            },
            HistoryMessage {
                role: HistoryRole::Assistant,
                text: "- Reading `flow.rs`\n  and [the spec](docs/spec.md) now".into(),
                tool_calls: vec!["Read(flow.rs)".into()],
                run_id: None,
                hidden: false,
                at: Some(2),
                steer_id: None,
            },
        ],
    );
    let listed = h.engine.task_list(&p).await.unwrap();
    let view = listed["tasks"]
        .as_array()
        .expect("a task list")
        .iter()
        .find(|t| t["taskId"] == first["taskId"])
        .expect("the first task is listed");
    assert_eq!(
        view["progress"]["latestOutput"], "Reading flow.rs and the spec now",
        "{view}"
    );

    // Reopened (an app restart): the anchor is durable.
    let reopened = open_engine(&h.db).await;
    let id = TaskId::new(first["taskId"].as_str().unwrap()).unwrap();
    let stored = reopened.store().task(&id).await.unwrap().unwrap();
    assert_eq!(stored.parent_run_id.as_deref(), Some("run-launch"));
}

#[test]
fn markdown_flattens_to_one_capped_line() {
    use crate::delegation::one_line;
    assert_eq!(
        one_line("- Read `a.rs`\n* see [docs](x/y.md) and [b] ok", 100),
        "Read a.rs see docs and [b] ok"
    );
    assert_eq!(one_line("ééééé", 3), "ééé…");
    assert_eq!(one_line("   \n ", 10), "");
}

/// A database from before the anchor column gains it on open, and its old
/// tasks read back with no anchor.
#[tokio::test]
async fn an_old_tasks_table_gains_the_anchor_column() {
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
            "CREATE TABLE orch_tasks (id TEXT PRIMARY KEY, parent_thread TEXT NOT NULL, child_thread TEXT, \
             root_thread TEXT NOT NULL, depth INTEGER NOT NULL, profile_id TEXT, parent_profile_id TEXT, \
             title TEXT NOT NULL, role TEXT NOT NULL, runtime_id TEXT NOT NULL, provider TEXT, model TEXT, \
             prompt TEXT NOT NULL, status TEXT NOT NULL, result TEXT, error TEXT, progress TEXT, usage TEXT, \
             created_at INTEGER NOT NULL, started_at INTEGER, finished_at INTEGER, client_request_id TEXT, \
             delivery TEXT NOT NULL DEFAULT 'none', delivered_at INTEGER, \
             cancel_requested INTEGER NOT NULL DEFAULT 0, limit_reset_at INTEGER);
             INSERT INTO orch_tasks (id, parent_thread, root_thread, depth, title, role, runtime_id, prompt, \
             status, created_at) VALUES ('task-old', 'p', 'p', 1, 'Old', 'general', 'fake', 'x', 'completed', 5);",
        )
        .await
        .unwrap();
    let engine = open_engine(&db).await;
    let old = engine
        .store()
        .task(&TaskId::new("task-old").unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.parent_run_id, None);
}
