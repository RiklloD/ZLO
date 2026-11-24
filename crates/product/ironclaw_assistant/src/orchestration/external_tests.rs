//! A CLI thread through the orchestrating surface: its stream shows the
//! pending tool call, `gate.resolve` from the owner answers it, and nobody
//! else reaches either.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_cli_providers::bridge::{CliBridge, ToolDecision};
use ironclaw_host_api::ids::{ActivityId, AgentId, CapabilityId, TenantId, ThreadId, UserId};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::model::{RuntimeId, ThreadRecord};
use ironclaw_orchestrator::runtime::{
    CreateThread, CreatedThread, HistoryMessage, HistoryRole, RuntimeCapabilities, RuntimeError,
    SteerHow, SteerOutcome, TurnAdmission, TurnInput, TurnRuntime,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use serde_json::json;

use super::surface::{OrchestratedProductSurface, scope_of};
use crate::reborn_services::RESOLVE_GATE_COMMAND_ID;

/// Stands for IronClaw's own surface: anything reaching it was not a CLI thread.
struct Inner;

#[async_trait]
impl ProductSurface for Inner {
    async fn invoke(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
    async fn query(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
    async fn stream_events(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        Ok(ProductSurfaceStreamResponse {
            events: Vec::new(),
            next_cursor: None,
            subscription: None,
        })
    }
}

fn caller(user: &str) -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new(user).unwrap(),
        None,
        None,
    )
}

#[tokio::test]
async fn cli_thread_gate_streams_and_resolves_for_its_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let thread = ThreadId::new("cli-thread").unwrap();
    let owner = caller("owner");
    engine
        .store()
        .ensure_thread(&ThreadRecord::new(
            thread.clone(),
            scope_of(&owner),
            RuntimeId::new("claude-cli").unwrap(),
        ))
        .await
        .unwrap();
    let bridge = CliBridge::new(Duration::from_secs(10));
    let surface =
        OrchestratedProductSurface::new(Arc::new(Inner), engine, Some(Arc::clone(&bridge)));

    let run = uuid::Uuid::new_v4().to_string();
    bridge.turn_started(&thread, &run);
    let waiter = {
        let (bridge, thread, run) = (Arc::clone(&bridge), thread.clone(), run.clone());
        tokio::spawn(async move {
            bridge
                .request_approval(&thread, &run, "Bash", &json!({"command": "ls"}))
                .await
        })
    };
    while bridge.snapshot(&thread).is_some_and(|s| s.gates.is_empty()) {
        tokio::task::yield_now().await;
    }

    let stream = |who: ProductSurfaceCaller| {
        surface.stream_events(
            who,
            ProductSurfaceStreamRequest {
                stream_id: Some(thread.to_string()),
                after_cursor: None,
            },
        )
    };
    // Another user's stream never reaches the bridge.
    let foreign = stream(caller("intruder")).await.unwrap();
    assert!(foreign.events.is_empty() && foreign.subscription.is_none());
    let mine = stream(owner.clone()).await.unwrap();
    assert!(mine.subscription.is_some());
    let items = &mine.events[0]["payload"]["projection_update"]["state"]["items"];
    let gate = items
        .as_array()
        .unwrap()
        .iter()
        .find_map(|i| i.get("gate"))
        .unwrap_or_else(|| panic!("no gate in {items}"));
    assert_eq!(gate["headline"], "Allow Bash?");
    assert_eq!(gate["allow_always"], false);
    let gate_ref = gate["gate_ref"].as_str().unwrap().to_string();

    let resolve = |who: ProductSurfaceCaller| {
        surface.invoke(
            who,
            ProductSurfaceInvokeRequest {
                operation_id: CapabilityId::new(RESOLVE_GATE_COMMAND_ID).unwrap(),
                input: json!({"thread_id": thread.as_str(), "run_id": run,
                              "gate_ref": gate_ref, "resolution": "approved"}),
                activity_id: ActivityId::new(),
            },
        )
    };
    // The intruder falls through to the inner surface, which knows no such gate.
    assert!(resolve(caller("intruder")).await.is_err());
    let answered = resolve(owner).await.unwrap();
    assert_eq!(answered.output["outcome"], "resumed");
    assert_eq!(waiter.await.unwrap(), ToolDecision::Allow);
}

/// A CLI runtime whose session holds one reply.
struct OneReply;

#[async_trait]
impl TurnRuntime for OneReply {
    fn id(&self) -> RuntimeId {
        RuntimeId::new("claude-cli").unwrap()
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::default()
    }
    async fn create_thread(&self, _: CreateThread) -> Result<CreatedThread, RuntimeError> {
        Err(RuntimeError::Unsupported("create"))
    }
    async fn start_turn(
        &self,
        _: &ThreadRecord,
        _: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        Err(RuntimeError::Unsupported("turns"))
    }
    async fn steer(
        &self,
        _: &ThreadRecord,
        _: &str,
        _: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        Err(RuntimeError::Unsupported("steer"))
    }
    async fn cancel(&self, _: &ThreadRecord, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn history(
        &self,
        _: &ThreadRecord,
        _: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        Ok(vec![HistoryMessage {
            role: HistoryRole::Assistant,
            text: "Forty-two.".into(),
            tool_calls: Vec::new(),
            run_id: None,
            hidden: false,
            at: None,
            steer_id: None,
        }])
    }
}

/// A fork or delegated child the orchestrator created never reached the
/// session thread store, so the inner timeline answers 404. The surface
/// still serves its timeline, in the gateway's shape, to its owner only.
#[tokio::test]
async fn orchestrator_made_cli_thread_serves_a_timeline() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let thread = ThreadId::new("cli-fork").unwrap();
    let owner = ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new("owner").unwrap(),
        Some(AgentId::new("agent").unwrap()),
        None,
    );
    let mut record = ThreadRecord::new(
        thread.clone(),
        scope_of(&owner),
        RuntimeId::new("claude-cli").unwrap(),
    );
    record.title = Some("Side chat".into());
    engine.store().ensure_thread(&record).await.unwrap();
    engine.register_runtime(Arc::new(OneReply));
    let surface = OrchestratedProductSurface::new(Arc::new(Inner), engine, None);

    let timeline = |who: ProductSurfaceCaller| {
        surface.query(
            who,
            ProductSurfaceQueryRequest {
                view_id: crate::reborn_services::TIMELINE_VIEW.id.to_string(),
                input: json!({"thread_id": thread.as_str(), "limit": 50}),
                cursor: None,
                limit: None,
            },
        )
    };
    let page = timeline(owner).await.unwrap();
    let response: crate::reborn_services::RebornTimelineResponse =
        serde_json::from_value(page.items[0].clone()).unwrap();
    assert_eq!(response.thread.thread_id.as_str(), "cli-fork");
    assert_eq!(response.thread.title.as_deref(), Some("Side chat"));
    assert_eq!(response.messages.len(), 1);
    assert_eq!(response.messages[0].content.as_deref(), Some("Forty-two."));
    assert!(response.next_cursor.is_none());
    // Anyone else still falls through to the inner 404.
    assert!(timeline(caller("intruder")).await.is_err());
}

/// Live command output, the turn diff and a review reach a CLI thread's
/// stream as typed `cli_detail` items: the snapshot at connect carries
/// them, and later changes arrive on the subscription.
#[tokio::test]
async fn cli_thread_stream_carries_command_output_diff_and_review() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let thread = ThreadId::new("codex-thread").unwrap();
    let owner = caller("owner");
    engine
        .store()
        .ensure_thread(&ThreadRecord::new(
            thread.clone(),
            scope_of(&owner),
            RuntimeId::new("codex").unwrap(),
        ))
        .await
        .unwrap();
    let bridge = CliBridge::new(Duration::from_secs(10));
    let surface =
        OrchestratedProductSurface::new(Arc::new(Inner), engine, Some(Arc::clone(&bridge)));

    let run = uuid::Uuid::new_v4().to_string();
    bridge.turn_started(&thread, &run);
    bridge.tool_observed(&thread, &run, "shell", Some("cargo test"), "c1");
    bridge.tool_output(&thread, &run, "c1", "running 3 tests\n");
    bridge.turn_diff(&thread, &run, "diff --git a/x b/x\n");

    let mut stream = surface
        .stream_events(
            owner,
            ProductSurfaceStreamRequest {
                stream_id: Some(thread.to_string()),
                after_cursor: None,
            },
        )
        .await
        .unwrap();
    let items = stream.events[0]["payload"]["projection_update"]["state"]["items"]
        .as_array()
        .unwrap()
        .clone();
    let details: Vec<&serde_json::Value> =
        items.iter().filter_map(|i| i.get("cli_detail")).collect();
    let output = details
        .iter()
        .find(|d| d["kind"] == "tool_output")
        .unwrap_or_else(|| panic!("no output in {items:?}"));
    assert_eq!(output["body"], "running 3 tests\n");
    assert!(output["invocation_id"].is_string());
    assert!(
        details
            .iter()
            .any(|d| d["kind"] == "turn_diff" && d["body"] == "diff --git a/x b/x\n")
    );

    // A finding quoting a hardcoded key: the stream carries it redacted,
    // the workspace-relative location intact.
    bridge.review(
        &thread,
        &run,
        "- [P1] Hardcoded key — src/a.rs:1-2\n  let k = \"sk-proj-abcdefghijklmnopqrstuvwxyz0123456789\";",
    );
    let subscription = stream.subscription.take().unwrap();
    let next = tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame = &next.events[0]["payload"]["projection_update"]["state"]["items"][0];
    assert_eq!(frame["cli_detail"]["kind"], "review");
    let body = frame["cli_detail"]["body"].as_str().unwrap();
    assert!(body.contains("src/a.rs:1-2"), "{body}");
    assert!(!body.contains("sk-proj-abcdefghijklmnop"), "{body}");
}

/// `orchestrator.thread_action` reaches the runtime of the caller's own
/// thread only; a runtime without the action answers a correctable error.
#[tokio::test]
async fn thread_action_reaches_the_owners_runtime_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let thread = ThreadId::new("cli-thread").unwrap();
    let owner = caller("owner");
    engine
        .store()
        .ensure_thread(&ThreadRecord::new(
            thread.clone(),
            scope_of(&owner),
            RuntimeId::new("claude-cli").unwrap(),
        ))
        .await
        .unwrap();
    engine.register_runtime(Arc::new(OneReply));
    let surface = OrchestratedProductSurface::new(Arc::new(Inner), engine, None);
    let act = |who: ProductSurfaceCaller| {
        surface.invoke(
            who,
            ProductSurfaceInvokeRequest {
                operation_id: CapabilityId::new(
                    ironclaw_product_contracts::orchestration::ORCHESTRATOR_THREAD_ACTION_COMMAND_ID,
                )
                .unwrap(),
                input: json!({"thread_id": thread.as_str(), "action": {"action": "compact"}}),
                activity_id: ActivityId::new(),
            },
        )
    };
    assert!(act(caller("intruder")).await.is_err());
    let answer = act(owner).await.unwrap();
    assert_eq!(answer.output["ok"], false, "{}", answer.output);
    assert_eq!(answer.output["error"], "unsupported");
}

/// A CLI runtime that steers (when `steers`) and reports steer reads; its
/// history is the steers it took, in run `r1`.
struct Steering {
    steers: bool,
    taken: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl TurnRuntime for Steering {
    fn id(&self) -> RuntimeId {
        RuntimeId::new("claude-cli").unwrap()
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            steer_in_turn: self.steers,
            ..RuntimeCapabilities::default()
        }
    }
    fn reports_steer_reads(&self) -> bool {
        true
    }
    async fn create_thread(&self, _: CreateThread) -> Result<CreatedThread, RuntimeError> {
        Err(RuntimeError::Unsupported("create"))
    }
    async fn start_turn(
        &self,
        _: &ThreadRecord,
        _: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        Ok(TurnAdmission::Busy)
    }
    async fn steer(
        &self,
        _: &ThreadRecord,
        text: &str,
        _: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        self.taken.lock().unwrap().push(text.to_string());
        Ok(SteerOutcome::Steered)
    }
    async fn cancel(&self, _: &ThreadRecord, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn history(
        &self,
        _: &ThreadRecord,
        _: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        let row = |text: &str| HistoryMessage {
            role: HistoryRole::User,
            text: text.to_string(),
            tool_calls: Vec::new(),
            run_id: Some("r1".into()),
            hidden: false,
            at: None,
            steer_id: None,
        };
        // The run's prompt, then the steers it took.
        let mut rows = vec![row("start")];
        rows.extend(self.taken.lock().unwrap().iter().map(|text| row(text)));
        Ok(rows)
    }
}

/// SUBMIT_TURN into a busy CLI thread answers a typed delivery (PLAN 10):
/// `steered` with the steer's ledger id, or `queued` with the queue item on
/// a runtime that cannot steer; the notice stays for older clients. The
/// timeline marks the steer "waiting to be read" until the runtime reports
/// it read, and a steer a Stop left unread shows "Steer delivery not
/// confirmed" on its run (PLAN 2, 5).
#[tokio::test]
async fn busy_cli_submit_reports_typed_delivery_and_steer_read_state() {
    use ironclaw_orchestrator::model::now_ms;
    use ironclaw_orchestrator::runtime::{SteerId, SteerReadState, TurnEventSink, TurnOutcome};

    for steers in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
        let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
        let thread = ThreadId::new("cli-busy").unwrap();
        let owner = ProductSurfaceCaller::new(
            TenantId::new("t").unwrap(),
            UserId::new("owner").unwrap(),
            Some(AgentId::new("agent").unwrap()),
            None,
        );
        let record = ThreadRecord::new(
            thread.clone(),
            scope_of(&owner),
            RuntimeId::new("claude-cli").unwrap(),
        );
        engine.store().ensure_thread(&record).await.unwrap();
        engine
            .store()
            .set_turn_running(&thread, Some("r1"), now_ms())
            .await
            .unwrap();
        engine.register_runtime(Arc::new(Steering {
            steers,
            taken: Default::default(),
        }));
        let surface = OrchestratedProductSurface::new(Arc::new(Inner), engine.clone(), None);
        let submit = |text: &str| {
            surface.invoke(
                owner.clone(),
                ProductSurfaceInvokeRequest {
                    operation_id: CapabilityId::new(crate::reborn_services::SUBMIT_TURN_COMMAND_ID)
                        .unwrap(),
                    input: json!({"thread_id": thread.as_str(), "client_action_id": text,
                                  "content": text}),
                    activity_id: ActivityId::new(),
                },
            )
        };
        let timeline = || async {
            let page = surface
                .query(
                    owner.clone(),
                    ProductSurfaceQueryRequest {
                        view_id: crate::reborn_services::TIMELINE_VIEW.id.to_string(),
                        input: json!({"thread_id": thread.as_str(), "limit": 50}),
                        cursor: None,
                        limit: None,
                    },
                )
                .await
                .unwrap();
            page.items[0]["messages"].as_array().unwrap().clone()
        };

        let out = submit("use tabs").await.unwrap().output;
        assert_eq!(out["outcome"], "deferred_busy", "{out}");
        if !steers {
            assert_eq!(out["delivery"], "queued", "{out}");
            assert!(out["queued_id"].is_string(), "{out}");
            assert!(out["steer_id"].is_null());
            assert!(out["notice"].as_str().unwrap().starts_with("Queued"));
            continue;
        }
        assert_eq!(out["delivery"], "steered", "{out}");
        let steer_id = SteerId::new(out["steer_id"].as_str().unwrap()).unwrap();
        let rows = timeline().await;
        assert_eq!(rows[1]["steer"]["waiting_to_be_read"], true, "{rows:?}");

        engine
            .steer_updated(&thread, "r1", &steer_id, SteerReadState::Read, now_ms())
            .await;
        let rows = timeline().await;
        assert_eq!(rows[1]["steer"]["waiting_to_be_read"], false);
        assert_eq!(rows[1]["steer"]["state"], "read");

        // Unread at a Stop: a notice on the run, not a resend.
        submit("and the docs").await.unwrap();
        engine.on_user_stop(&thread).await.unwrap();
        engine
            .turn_finished(
                &thread,
                Some("r1".into()),
                TurnOutcome::ended(
                    ironclaw_orchestrator::runtime::TurnEndStatus::Cancelled,
                    None,
                ),
                now_ms(),
            )
            .await;
        let rows = timeline().await;
        let notice = &rows.last().unwrap()["steer_notices"][0];
        assert_eq!(notice["title"], "Steer delivery not confirmed", "{rows:?}");
        assert_eq!(notice["text"], "and the docs");
        assert!(
            engine
                .store()
                .queue(&thread, false)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

/// A CLI runtime that journals each steer under the engine's id, after an
/// opening prompt with the same text.
struct Journaling {
    taken: std::sync::Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl TurnRuntime for Journaling {
    fn id(&self) -> RuntimeId {
        RuntimeId::new("claude-cli").unwrap()
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            steer_in_turn: true,
            ..RuntimeCapabilities::default()
        }
    }
    fn reports_steer_reads(&self) -> bool {
        true
    }
    async fn create_thread(&self, _: CreateThread) -> Result<CreatedThread, RuntimeError> {
        Err(RuntimeError::Unsupported("create"))
    }
    async fn start_turn(
        &self,
        _: &ThreadRecord,
        _: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        Ok(TurnAdmission::Busy)
    }
    async fn steer(
        &self,
        _: &ThreadRecord,
        _: &str,
        _: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        Err(RuntimeError::Unsupported("plain steer"))
    }
    async fn steer_message(
        &self,
        _: &ThreadRecord,
        request: &ironclaw_orchestrator::runtime::SteerRequest,
    ) -> Result<SteerOutcome, RuntimeError> {
        self.taken
            .lock()
            .unwrap()
            .push((request.text.clone(), request.id.as_str().to_string()));
        Ok(SteerOutcome::Steered)
    }
    async fn cancel(&self, _: &ThreadRecord, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn history(
        &self,
        _: &ThreadRecord,
        _: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        let row = |text: &str, steer_id: Option<String>| HistoryMessage {
            role: HistoryRole::User,
            text: text.to_string(),
            tool_calls: Vec::new(),
            run_id: Some("r1".into()),
            hidden: false,
            at: None,
            steer_id,
        };
        let mut rows = vec![row("go", None)];
        for (text, id) in self.taken.lock().unwrap().iter() {
            rows.push(row(text, Some(id.clone())));
        }
        Ok(rows)
    }
}

/// The steer marker lands on the row the runtime journaled under the
/// steer's id, not on an opening prompt with the same text; it survives
/// the typed timeline (`steer_marks`, which the WebUI decodes); and a
/// retried submit with the same client action id answers the first one
/// without steering again (PLAN 10, Sol 7).
#[tokio::test]
async fn steer_markers_follow_the_steer_id_and_retries_steer_once() {
    use ironclaw_orchestrator::model::now_ms;

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let thread = ThreadId::new("cli-journal").unwrap();
    let owner = ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new("owner").unwrap(),
        Some(AgentId::new("agent").unwrap()),
        None,
    );
    let record = ThreadRecord::new(
        thread.clone(),
        scope_of(&owner),
        RuntimeId::new("claude-cli").unwrap(),
    );
    engine.store().ensure_thread(&record).await.unwrap();
    engine
        .store()
        .set_turn_running(&thread, Some("r1"), now_ms())
        .await
        .unwrap();
    let cli = Arc::new(Journaling {
        taken: Default::default(),
    });
    engine.register_runtime(cli.clone());
    let surface = OrchestratedProductSurface::new(Arc::new(Inner), engine.clone(), None);
    let submit = || {
        surface.invoke(
            owner.clone(),
            ProductSurfaceInvokeRequest {
                operation_id: CapabilityId::new(crate::reborn_services::SUBMIT_TURN_COMMAND_ID)
                    .unwrap(),
                input: json!({"thread_id": thread.as_str(), "client_action_id": "act-1",
                              "content": "go"}),
                activity_id: ActivityId::new(),
            },
        )
    };
    let first = submit().await.unwrap().output;
    let again = submit().await.unwrap().output;
    assert_eq!(first["delivery"], "steered", "{first}");
    assert_eq!(first["steer_id"], again["steer_id"], "{again}");
    assert_eq!(cli.taken.lock().unwrap().len(), 1, "a retry steered again");
    let steer_id = first["steer_id"].as_str().unwrap().to_string();

    let page = surface
        .query(
            owner.clone(),
            ProductSurfaceQueryRequest {
                view_id: crate::reborn_services::TIMELINE_VIEW.id.to_string(),
                input: json!({"thread_id": thread.as_str(), "limit": 50}),
                cursor: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    let rows = page.items[0]["messages"].as_array().unwrap().clone();
    assert!(
        rows[0].get("steer").is_none(),
        "the opening prompt is not a steer"
    );
    assert_eq!(rows[1]["steer"]["id"], steer_id.as_str());
    let typed: crate::reborn_services::RebornTimelineResponse =
        serde_json::from_value(page.items[0].clone()).unwrap();
    assert_eq!(typed.steer_marks.len(), 1);
    assert_eq!(
        typed.steer_marks[0].message_id,
        rows[1]["message_id"].as_str().unwrap()
    );
    assert_eq!(
        typed.steer_marks[0].steer.as_ref().unwrap().id,
        steer_id.as_str()
    );
}

/// The queue route takes files and Send now (GPT review of Rune's queue):
/// a queued message with an attachment lands the file and names it in the
/// queued text; `send_now` steers it into the running turn once, and a
/// second `send_now` or a `remove` of it answers `not_queued` instead of a
/// silent success.
#[tokio::test]
async fn queue_route_takes_files_and_sends_one_item_now_once() {
    use base64::Engine as _;
    use ironclaw_orchestrator::model::now_ms;

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let thread = ThreadId::new("cli-queue").unwrap();
    let owner = caller("owner");
    let record = ThreadRecord::new(
        thread.clone(),
        scope_of(&owner),
        RuntimeId::new("claude-cli").unwrap(),
    );
    engine.store().ensure_thread(&record).await.unwrap();
    engine
        .store()
        .set_turn_running(&thread, Some("r1"), now_ms())
        .await
        .unwrap();
    let steering = Arc::new(Steering {
        steers: true,
        taken: Default::default(),
    });
    engine.register_runtime(steering.clone());
    let home = tempfile::tempdir().unwrap();
    let surface = OrchestratedProductSurface::new(Arc::new(Inner), engine.clone(), None)
        .with_home(super::profiles::ProfileHome::new(home.path().to_path_buf()));
    let mutate = |input: serde_json::Value| {
        surface.invoke(
            owner.clone(),
            ProductSurfaceInvokeRequest {
                operation_id: CapabilityId::new(
                    ironclaw_product_contracts::orchestration::ORCHESTRATOR_QUEUE_MUTATE_COMMAND_ID,
                )
                .unwrap(),
                input,
                activity_id: ActivityId::new(),
            },
        )
    };
    let data = base64::engine::general_purpose::STANDARD.encode(b"hello notes");
    let out = mutate(json!({"thread_id": thread.as_str(), "action": "add",
        "text": "read the notes",
        "attachments": [{"mime_type": "text/plain", "filename": "notes.txt", "data_base64": data}]}))
    .await
    .unwrap()
    .output;
    let item = out["queuedId"].as_str().unwrap().to_string();
    let queued = out["items"][0]["text"].as_str().unwrap();
    assert!(queued.starts_with("read the notes"), "{queued}");
    assert!(queued.contains("notes.txt"), "the file is named: {queued}");

    let sent = mutate(json!({"thread_id": thread.as_str(), "action": "send_now", "item_id": item}))
        .await
        .unwrap()
        .output;
    assert_eq!(sent["delivery"], "steered", "{sent}");
    assert_eq!(steering.taken.lock().unwrap().len(), 1);
    for action in ["send_now", "remove"] {
        let again =
            mutate(json!({"thread_id": thread.as_str(), "action": action, "item_id": item}))
                .await
                .unwrap()
                .output;
        assert_eq!(again["error"], "not_queued", "{action}: {again}");
    }
    assert_eq!(steering.taken.lock().unwrap().len(), 1, "delivered once");
}

/// CLI timeline rows carry the time the runtime recorded, so a client can
/// order steers and time the work after each one.
#[test]
fn cli_timeline_rows_carry_their_time() {
    let row = |text: &str, at: Option<i64>| HistoryMessage {
        role: HistoryRole::User,
        text: text.to_string(),
        tool_calls: Vec::new(),
        run_id: Some("r1".into()),
        hidden: false,
        at,
        steer_id: None,
    };
    let thread = ThreadId::new("cli-times").unwrap();
    let rows = super::external::timeline_values(
        &thread,
        &[row("start", Some(1_763_600_000_000)), row("no time", None)],
    );
    assert_eq!(rows[0]["created_at"], "2025-11-20T00:53:20+00:00");
    assert!(rows[1]["created_at"].is_null());
}

/// Regression (ZLO "seven preambles"): a CLI run's assistant text that a
/// tool call or a later assistant row follows is commentary, flagged on the
/// wire so clients subdue and fold it; only the run's last row (the answer)
/// and a still-open last row go unflagged.
#[test]
fn cli_timeline_marks_pre_tool_assistant_rows_as_commentary() {
    let msg = |role: HistoryRole, text: &str, calls: &[&str], run: &str| HistoryMessage {
        role,
        text: text.to_string(),
        tool_calls: calls.iter().map(|c| c.to_string()).collect(),
        run_id: Some(run.into()),
        hidden: false,
        at: None,
        steer_id: None,
    };
    let thread = ThreadId::new("cli-commentary").unwrap();
    let rows = super::external::timeline_values(
        &thread,
        &[
            msg(HistoryRole::User, "spin up 2", &[], "r1"),
            msg(
                HistoryRole::Assistant,
                "I'll launch two subagents.",
                &["t3_delegate(a)"],
                "r1",
            ),
            msg(HistoryRole::Tool, "ok", &[], "r1"),
            msg(
                HistoryRole::Assistant,
                "I'll launch two fresh subagents.",
                &[],
                "r1",
            ),
            msg(HistoryRole::Assistant, "", &["t3_delegate(b)"], "r1"),
            msg(HistoryRole::Tool, "ok", &[], "r1"),
            msg(HistoryRole::Assistant, "Both are running.", &[], "r1"),
            msg(HistoryRole::User, "thanks", &[], "r2"),
            msg(HistoryRole::Assistant, "Still working on it", &[], "r2"),
        ],
    );
    let flags: Vec<(String, bool)> = rows
        .iter()
        .map(|r| {
            (
                r["content"].as_str().unwrap().to_string(),
                r["commentary"].as_bool().unwrap_or(false),
            )
        })
        .collect();
    assert_eq!(
        flags,
        vec![
            ("spin up 2".into(), false),
            ("I'll launch two subagents.".into(), true),
            ("I'll launch two fresh subagents.".into(), true),
            ("Both are running.".into(), false),
            ("thanks".into(), false),
            ("Still working on it".into(), false),
        ]
    );
    assert!(
        rows[0].get("commentary").is_none(),
        "the flag is omitted when false"
    );
}

/// Regression (review of the commentary flag): the run's last assistant
/// row is never commentary, whatever calls ride on it. Codex and Muse
/// journal the final answer with the turn's aggregate tool calls; a run
/// can end on a tool call; a steer can be followed by work and no reply.
#[test]
fn cli_timeline_never_marks_a_runs_last_assistant_row_as_commentary() {
    let msg = |role: HistoryRole, text: &str, calls: &[&str], run: &str| HistoryMessage {
        role,
        text: text.to_string(),
        tool_calls: calls.iter().map(|c| c.to_string()).collect(),
        run_id: Some(run.into()),
        hidden: false,
        at: None,
        steer_id: None,
    };
    let flags = |history: &[HistoryMessage]| -> Vec<(String, bool)> {
        let thread = ThreadId::new("cli-last-row").unwrap();
        super::external::timeline_values(&thread, history)
            .iter()
            .filter(|r| r["kind"] == "assistant")
            .map(|r| {
                (
                    r["content"].as_str().unwrap().to_string(),
                    r["commentary"].as_bool().unwrap_or(false),
                )
            })
            .collect()
    };
    // Codex/Muse: one final entry carrying every call of the turn.
    assert_eq!(
        flags(&[
            msg(HistoryRole::User, "fix it", &[], "codex"),
            msg(
                HistoryRole::Assistant,
                "Fixed: the test passes.",
                &["shell()", "apply_patch()"],
                "codex"
            ),
        ]),
        vec![("Fixed: the test passes.".into(), false)]
    );
    // Codex after a steer cut: the segment before is commentary.
    assert_eq!(
        flags(&[
            msg(HistoryRole::User, "fix it", &[], "cut"),
            msg(HistoryRole::Assistant, "Looking at it", &["shell()"], "cut"),
            msg(HistoryRole::User, "also the docs", &[], "cut"),
            msg(
                HistoryRole::Assistant,
                "Both done.",
                &["apply_patch()"],
                "cut"
            ),
        ]),
        vec![("Looking at it".into(), true), ("Both done.".into(), false)]
    );
    // A run that ends on a tool call (stopped, or a steer then work).
    assert_eq!(
        flags(&[
            msg(HistoryRole::User, "go", &[], "stop"),
            msg(
                HistoryRole::Assistant,
                "Running the suite.",
                &["bash(cargo test)"],
                "stop"
            ),
            msg(HistoryRole::Tool, "ok", &[], "stop"),
            msg(HistoryRole::User, "and clippy", &[], "stop"),
            msg(HistoryRole::Assistant, "", &["bash(cargo clippy)"], "stop"),
            msg(HistoryRole::Tool, "ok", &[], "stop"),
        ]),
        vec![("Running the suite.".into(), false)]
    );
}
