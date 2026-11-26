//! The inbox surface's unit tests (moved out of `inbox.rs` to keep it
//! under the large-file bar).

use async_trait::async_trait;
use ironclaw_host_api::ids::{ActivityId, CapabilityId, TenantId, UserId};
use ironclaw_orchestrator::model::ThreadScope;
use ironclaw_orchestrator::runtime::{TurnEventSink, TurnOutcome};
use ironclaw_product_contracts::profiles::{PROFILE_ICON_VIEW, PROFILE_VIEW, ProfileIcon};
use ironclaw_product_contracts::surface::{
    ProductSurfaceInvokeRequest, ProductSurfaceInvokeResponse, ProductSurfaceQueryPage,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use serde_json::json;

use super::*;
use crate::orchestration::OrchestratedProductSurface;

/// The undecorated surface: one session thread and one automation run.
/// The task model answers a fixed title and counts its calls.
struct Inner;

static TASK_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Exchanges about deploys after the first one in the "chat" timeline.
static LATER_EXCHANGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// The task model takes its time, so turns can complete during a call.
static SLOW_MODEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[async_trait]
impl ProductSurface for Inner {
    async fn invoke(
        &self,
        _caller: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        if request.operation_id.as_str() != TASK_COMPLETE_COMMAND.id {
            return Err(ProductSurfaceError::not_found());
        }
        TASK_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if SLOW_MODEL.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        let asked: TaskCompletionRequest = serde_json::from_value(request.input).unwrap();
        assert_eq!(asked.task, BackgroundTask::ThreadTitles);
        let text = if asked.prompt.starts_with("Current title:") {
            // Only the latest messages: the first exchange is out of range.
            assert!(!asked.prompt.contains("Plan the inbox for stage 5"));
            // A re-title reads the saved title and the latest messages.
            assert!(
                asked
                    .prompt
                    .contains("Current title:\nInbox plan for stage 5")
                    || asked
                        .prompt
                        .contains("Current title:\nDeploy pipeline rollout")
            );
            assert!(asked.prompt.contains("Ship the deploy 3"));
            "Deploy pipeline rollout"
        } else {
            assert!(asked.prompt.contains("Plan the inbox for stage 5"));
            assert!(asked.prompt.contains("Here is a plan"));
            "\"Inbox plan for stage 5.\"\n"
        };
        Ok(ProductSurfaceInvokeResponse {
            output: json!({"text": text}),
        })
    }

    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        let scope = json!({"tenant_id": "t1", "agent_id": "a1", "owner_user_id": "u1"});
        let item = match request.view_id.as_str() {
            v if v == TIMELINE_VIEW.id
                && request.input["thread_id"] == "chat"
                && caller.user_id.as_str() == "u1" =>
            {
                // Newest page: a long tool run, then the later exchanges.
                // The first exchange is one page older.
                if request.cursor.as_deref() == Some("older") {
                    json!({"messages": [
                        json!({"kind": "system", "sequence": 1, "content": "setup"}),
                        json!({"kind": "user", "sequence": 2, "content": "Plan the inbox for stage 5"}),
                    ]})
                } else {
                    let mut messages: Vec<Value> = (0..40)
                        .map(|n| {
                            json!({"kind": "tool_result", "sequence": 3 + n,
                            "content": "{\"ok\":true}"})
                        })
                        .collect();
                    messages.push(json!({"kind": "assistant", "sequence": 50,
                        "content": "Here is a plan"}));
                    for n in 1..=LATER_EXCHANGES.load(std::sync::atomic::Ordering::SeqCst) {
                        messages.push(json!({"kind": "user", "sequence": 50 + 2 * n,
                            "content": format!("Ship the deploy {n}")}));
                        messages.push(json!({"kind": "assistant", "sequence": 51 + 2 * n,
                            "content": "Deploying"}));
                    }
                    json!({"messages": messages, "next_cursor": "older"})
                }
            }
            v if v == THREADS_VIEW.id => json!({"threads": [{
                "scope": scope, "thread_id": "chat", "created_by_actor_id": "u1",
                "title": "Plan the inbox", "metadata_json": null,
                "created_at": "2026-01-02T08:00:00Z", "updated_at": "2026-01-02T09:00:00Z"
            }]}),
            v if v == AUTOMATIONS_VIEW.id => json!({"automations": [{
                "name": "Morning brief",
                "recent_runs": [{"thread_id": "auto", "status": "ok",
                    "submitted_at": "2026-01-02T07:00:00Z",
                    "completed_at": "2026-01-02T07:01:00Z"}]
            }]}),
            _ => return Err(ProductSurfaceError::not_found()),
        };
        Ok(ProductSurfaceQueryPage {
            items: vec![item],
            next_cursor: None,
        })
    }

    async fn stream_events(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
}

fn caller(user: &str) -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("t1").unwrap(),
        UserId::new(user).unwrap(),
        None,
        None,
    )
}

async fn list_of(surface: &OrchestratedProductSurface, user: &str) -> InboxListResponse {
    let page = surface
        .query(
            // safety: a ProductSurface read in a test, not a DB statement
            caller(user),
            ProductSurfaceQueryRequest {
                view_id: INBOX_VIEW.id.into(),
                input: json!({}),
                cursor: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    serde_json::from_value(page.items[0].clone()).unwrap()
}

async fn surface_goal(
    surface: &OrchestratedProductSurface,
    thread: &str,
) -> ironclaw_product_contracts::inbox::GoalRecord {
    let page = surface
        .query(
            // safety: a ProductSurface read in a test, not a DB statement
            caller("u1"),
            ProductSurfaceQueryRequest {
                view_id: THREAD_GOAL_VIEW.id.into(),
                input: json!({"thread_id": thread}),
                cursor: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    serde_json::from_value::<ThreadGoalResponse>(page.items[0].clone())
        .unwrap()
        .goal
        .unwrap()
}

async fn invoke(
    surface: &OrchestratedProductSurface,
    operation: &str,
    input: Value,
) -> Result<Value, ProductSurfaceError> {
    surface
        .invoke(
            caller("u1"),
            ProductSurfaceInvokeRequest {
                operation_id: CapabilityId::new(operation).unwrap(),
                input,
                activity_id: ActivityId::new(),
            },
        )
        .await
        .map(|r| r.output)
}

fn bare(
    id: &str,
    parent: Option<&str>,
    state: InboxState,
    status: Option<InboxStatus>,
) -> InboxRow {
    InboxRow {
        thread_id: id.into(),
        title: None,
        profile_id: DEFAULT_PROFILE_ID.into(),
        state,
        status,
        engines: Vec::new(),
        last_activity_at: 0,
        snooze_until: None,
        settled_at: None,
        limit_reset_at: None,
        working_since: None,
        parent_thread_id: parent.map(str::to_string),
        automation: false,
        worktree_branch: None,
        title_pending: false,
        imported: None,
    }
}

#[test]
fn a_waiting_subagent_brings_every_thread_above_it_back() {
    let working = Some(InboxStatus::Working);
    let mut rows = vec![
        bare("root", None, InboxState::Working, working),
        bare(
            "mid",
            Some("root"),
            InboxState::Working,
            Some(InboxStatus::Background),
        ),
        bare(
            "leaf",
            Some("mid"),
            InboxState::Inbox,
            Some(InboxStatus::NeedsInput),
        ),
        bare("pinned", None, InboxState::Pinned, working),
        bare(
            "kid",
            Some("pinned"),
            InboxState::Inbox,
            Some(InboxStatus::NeedsInput),
        ),
        bare("busy", None, InboxState::Working, working),
        bare("calm", Some("busy"), InboxState::Working, working),
    ];
    surface_waiting_subagents(&mut rows);
    let at = |id: &str| {
        let r = rows.iter().find(|r| r.thread_id == id).unwrap();
        (r.state, r.status)
    };
    assert_eq!(at("root"), (InboxState::Inbox, working));
    assert_eq!(
        at("mid"),
        (InboxState::Inbox, Some(InboxStatus::Background)),
        "statuses stay their own; only the shelf moves"
    );
    assert_eq!(
        at("leaf"),
        (InboxState::Inbox, Some(InboxStatus::NeedsInput))
    );
    assert_eq!(
        at("pinned"),
        (InboxState::Pinned, working),
        "a pin stays a pin"
    );
    assert_eq!(
        at("busy"),
        (InboxState::Working, working),
        "working children change nothing"
    );
}

#[tokio::test]
async fn inbox_joins_sessions_automations_and_engine_threads_and_persists_marks() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let scope = ThreadScope {
        tenant_id: TenantId::new("t1").unwrap(),
        user_id: UserId::new("u1").unwrap(),
        agent_id: None,
        project_id: None,
    };
    engine
        .ensure_thread(&ThreadId::new("chat").unwrap(), &scope, None)
        .await
        .unwrap();
    let surface = OrchestratedProductSurface::new(Arc::new(Inner), Arc::clone(&engine), None);

    let list = list_of(&surface, "u1").await;
    let chat = list.rows.iter().find(|r| r.thread_id == "chat").unwrap();
    assert_eq!(chat.title.as_deref(), Some("Plan the inbox"));
    assert_eq!(chat.profile_id, DEFAULT_PROFILE_ID);
    assert_eq!(chat.engines[0].runtime_id, "ironclaw");
    let auto = list.rows.iter().find(|r| r.thread_id == "auto").unwrap();
    assert!(auto.automation, "automation runs join the inbox");
    assert_eq!(auto.title.as_deref(), Some("Morning brief"));
    assert_eq!(list.profiles[0].profile_id, DEFAULT_PROFILE_ID);

    // Settle the automation run and pin the chat; the answer is the new list.
    let out = invoke(
        &surface,
        INBOX_MUTATE_COMMAND_ID,
        json!({"thread_id": "auto", "action": "settle"}),
    )
    .await
    .unwrap();
    let after: InboxListResponse = serde_json::from_value(out).unwrap();
    let auto = after.rows.iter().find(|r| r.thread_id == "auto").unwrap();
    assert_eq!(auto.state, InboxState::Settled);
    assert_eq!(
        after.rows.last().unwrap().thread_id,
        "auto",
        "settled sorts last"
    );
    invoke(
        &surface,
        INBOX_MUTATE_COMMAND_ID,
        json!({"thread_id": "chat", "action": "pin"}),
    )
    .await
    .unwrap();
    let reread = list_of(&surface, "u1").await;
    assert_eq!(reread.rows[0].thread_id, "chat");
    assert_eq!(reread.rows[0].state, InboxState::Pinned);

    // A thread outside the caller's inbox gets no record.
    let refused = invoke(
        &surface,
        INBOX_MUTATE_COMMAND_ID,
        json!({"thread_id": "someone-else", "action": "pin"}),
    )
    .await;
    assert!(refused.is_err(), "unknown thread is a 404");

    // The worktree route: a thread without one, and a backend without
    // worktree isolation, answer model-visibly; a foreign thread is a 404.
    assert!(chat.worktree_branch.is_none());
    let wt = ironclaw_product_contracts::orchestration::ORCHESTRATOR_WORKTREE_COMMAND_ID;
    let status = invoke(
        &surface,
        wt,
        json!({"thread_id": "chat", "action": "status"}),
    )
    .await
    .unwrap();
    assert_eq!(status["error"], "no_worktree", "{status}");
    let create = invoke(
        &surface,
        wt,
        json!({"thread_id": "chat", "action": "create"}),
    )
    .await
    .unwrap();
    assert_eq!(create["error"], "worktree_unavailable", "{create}");
    let foreign = invoke(
        &surface,
        wt,
        json!({"thread_id": "someone-else", "action": "status"}),
    )
    .await;
    assert!(
        foreign.is_err(),
        "a thread the caller does not own is a 404"
    );

    // Goals live on engine threads.
    let out = invoke(
        &surface,
        THREAD_GOAL_SET_COMMAND_ID,
        json!({"thread_id": "chat", "objective": "Ship stage 5"}),
    )
    .await
    .unwrap();
    let goal: ThreadGoalResponse = serde_json::from_value(out).unwrap();
    assert_eq!(goal.goal.unwrap().objective, "Ship stage 5");
    // `/goal <text> --check "<cmd>"`: objective and check in one request.
    let out = invoke(
        &surface,
        THREAD_GOAL_SET_COMMAND_ID,
        json!({"thread_id": "chat", "objective": "Tests pass",
               "check": {"command": "cargo test -p foo", "max_attempts": 3}}),
    )
    .await
    .unwrap();
    let goal = serde_json::from_value::<ThreadGoalResponse>(out)
        .unwrap()
        .goal
        .unwrap();
    let check = goal.check.unwrap();
    assert_eq!(
        (
            goal.objective.as_str(),
            check.command.as_str(),
            check.max_attempts
        ),
        ("Tests pass", "cargo test -p foo", 3)
    );
    // "Run check now" names the revision it was shown; a stale one is
    // refused through the surface too.
    let shown = check.revision;
    invoke(
        &surface,
        THREAD_GOAL_SET_COMMAND_ID,
        json!({"thread_id": "chat", "check": {"command": "cargo test -p bar"}}),
    )
    .await
    .unwrap();
    let stale = invoke(
        &surface,
        THREAD_GOAL_SET_COMMAND_ID,
        json!({"thread_id": "chat", "check_action": "run_now", "check_revision": shown}),
    )
    .await;
    assert!(
        stale.as_ref().map_or(true, |out| out["ok"] == json!(false)),
        "a run_now for a revision the person never saw was accepted: {stale:?}"
    );
    let after = surface_goal(&surface, "chat").await.check.unwrap();
    assert_eq!(
        (after.command.as_str(), after.status),
        (
            "cargo test -p bar",
            ironclaw_product_contracts::inbox::GoalCheckStatus::NotRun
        ),
        "the replaced check was queued under the old approval"
    );
    // The strip's buttons act on it without touching the objective.
    let out = invoke(
        &surface,
        THREAD_GOAL_SET_COMMAND_ID,
        json!({"thread_id": "chat", "check_action": "stop_retrying"}),
    )
    .await
    .unwrap();
    let goal = serde_json::from_value::<ThreadGoalResponse>(out)
        .unwrap()
        .goal
        .unwrap();
    assert_eq!(goal.objective, "Tests pass");
    assert_eq!(
        goal.check.unwrap().status,
        ironclaw_product_contracts::inbox::GoalCheckStatus::Stopped
    );
    let out = invoke(
        &surface,
        THREAD_GOAL_SET_COMMAND_ID,
        json!({"thread_id": "chat", "clear_check": true}),
    )
    .await
    .unwrap();
    let goal = serde_json::from_value::<ThreadGoalResponse>(out)
        .unwrap()
        .goal
        .unwrap();
    assert_eq!((goal.objective.as_str(), goal.check), ("Tests pass", None));
    let page = surface
        .query(
            // safety: a ProductSurface read in a test, not a DB statement
            caller("u2"),
            ProductSurfaceQueryRequest {
                view_id: THREAD_GOAL_VIEW.id.into(),
                input: json!({"thread_id": "chat"}),
                cursor: None,
                limit: None,
            },
        )
        .await;
    assert!(page.is_err(), "another user cannot read the goal");
}

/// The new-thread picker shows where a profile works: every inbox
/// profile carries its effective folder, under the profile home.
#[tokio::test]
async fn inbox_profiles_carry_their_working_folder() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let scope = ThreadScope {
        tenant_id: TenantId::new("t1").unwrap(),
        user_id: UserId::new("u1").unwrap(),
        agent_id: None,
        project_id: None,
    };
    engine
        .ensure_thread(&ThreadId::new("chat").unwrap(), &scope, None)
        .await
        .unwrap();
    let surface = OrchestratedProductSurface::new(Arc::new(Inner), Arc::clone(&engine), None)
        .with_home(super::super::profiles::ProfileHome::new(
            dir.path().to_path_buf(),
        ));

    let list = list_of(&surface, "u1").await;
    let default = list
        .profiles
        .iter()
        .find(|p| p.profile_id == DEFAULT_PROFILE_ID)
        .unwrap();
    let folder = default.folder.as_deref().expect("a folder");
    assert!(
        std::path::Path::new(folder).starts_with(dir.path()),
        "{folder} lives under the profile home"
    );
}

/// Like t3code's project icons: a profile whose folder has a favicon
/// lists an icon revision, and `profiles.icon` serves that file; once it is
/// gone the list drops the revision and the view answers `no_profile_icon`.
#[tokio::test]
async fn a_profile_folder_logo_is_listed_and_served() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let surface = OrchestratedProductSurface::new(Arc::new(Inner), Arc::clone(&engine), None)
        .with_home(super::super::profiles::ProfileHome::new(
            dir.path().to_path_buf(),
        ));
    let default_of = |list: InboxListResponse| {
        list.profiles
            .into_iter()
            .find(|p| p.profile_id == DEFAULT_PROFILE_ID)
            .unwrap()
    };
    let icon_view = || async {
        surface
            .query(
                // safety: a ProductSurface read in a test, not a DB statement
                caller("u1"),
                ProductSurfaceQueryRequest {
                    view_id: PROFILE_ICON_VIEW.id.into(),
                    input: json!({"profile_id": DEFAULT_PROFILE_ID}),
                    cursor: None,
                    limit: None,
                },
            )
            .await
            .unwrap()
            .items[0]
            .clone()
    };

    // The folder from `profiles.get`: a list read now would cache "no logo"
    // for a minute, as t3code's resolver does.
    let info = surface
        .query(
            // safety: a ProductSurface read in a test, not a DB statement
            caller("u1"),
            ProductSurfaceQueryRequest {
                view_id: PROFILE_VIEW.id.into(),
                input: json!({"profile_id": DEFAULT_PROFILE_ID}),
                cursor: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    let folder = info.items[0]["effective_folder"]
        .as_str()
        .expect("a folder")
        .to_string();
    std::fs::create_dir_all(&folder).unwrap();
    let logo = std::path::Path::new(&folder).join("public/favicon.svg");
    std::fs::create_dir_all(logo.parent().unwrap()).unwrap();
    std::fs::write(&logo, b"<svg/>").unwrap();

    let revision = default_of(list_of(&surface, "u1").await)
        .icon_revision
        .expect("the folder's logo is listed");
    let icon: ProfileIcon = serde_json::from_value(icon_view().await).unwrap();
    assert_eq!(icon.mime, "image/svg+xml");
    assert_eq!(icon.data_base64, "PHN2Zy8+");
    assert_eq!(icon.revision, revision);

    std::fs::remove_file(&logo).unwrap();
    assert_eq!(
        default_of(list_of(&surface, "u1").await).icon_revision,
        None
    );
    assert_eq!(icon_view().await["error"], "no_profile_icon");
}

#[tokio::test]
async fn completed_turns_title_a_thread_and_retitle_it_every_three_replies() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let scope = ThreadScope {
        tenant_id: TenantId::new("t1").unwrap(),
        user_id: UserId::new("u1").unwrap(),
        agent_id: None,
        project_id: None,
    };
    let chat = ThreadId::new("chat").unwrap();
    engine.ensure_thread(&chat, &scope, None).await.unwrap();
    let orchestrated = Arc::new(OrchestratedProductSurface::new(
        Arc::new(Inner),
        Arc::clone(&engine),
        None,
    ));
    let surface: Arc<dyn ProductSurface> = orchestrated.clone();
    let before = TASK_CALLS.load(std::sync::atomic::Ordering::SeqCst);
    let calls = || TASK_CALLS.load(std::sync::atomic::Ordering::SeqCst) - before;
    let title = || async {
        let list = list_of(&orchestrated, "u1").await;
        let row = list
            .rows
            .into_iter()
            .find(|r| r.thread_id == "chat")
            .unwrap();
        (row.title, row.state)
    };
    let finish = |run: &str, outcome: TurnOutcome| {
        let (engine, chat, run) = (Arc::clone(&engine), chat.clone(), run.to_string());
        async move {
            engine
                .turn_finished(&chat, Some(run), outcome, now_ms())
                .await
        }
    };
    let settled = |titler: Arc<ThreadTitler>| async move {
        for _ in 0..300 {
            if titler.running().is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("title work did not finish");
    };

    // Pinned first: a pinned thread never shows as Working in the
    // sidebar, and is titled all the same.
    invoke(
        &orchestrated,
        INBOX_MUTATE_COMMAND_ID,
        json!({"thread_id": "chat", "action": "pin"}),
    )
    .await
    .unwrap();

    // Replayed at startup before the titler is wired: a failed turn
    // (not a reply) and the first reply.
    finish(
        "r0",
        TurnOutcome::ended(ironclaw_orchestrator::runtime::TurnEndStatus::Failed, None),
    )
    .await;
    finish("r1", TurnOutcome::completed(None)).await;
    assert_eq!(calls(), 0);

    // Wiring scans recent replies: titled from the first exchange,
    // which sits a page behind a long tool run. The pin is kept.
    let moment = std::time::Duration::from_millis(300);
    let hints = crate::inbox_hints::InboxHintBus::with_coalesce_window(moment / 300);
    let (tenant, user) = (scope.tenant_id.clone(), scope.user_id.clone());
    let stream = hints.subscribe_for_caller(&ProductSurfaceCaller::new(tenant, user, None, None));
    let hints = Arc::new(hints);
    // Whether a hint reaches the person's stream within a moment.
    let hinted = || async { tokio::time::timeout(moment, stream.next()).await.is_ok() };
    let titler = title_on_completion(&engine, &surface, Some(hints));
    for _ in 0..300 {
        if calls() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    settled(Arc::clone(&titler)).await;
    // One completed turn, as the journal observer reports it; waits for
    // the spawned title work.
    let reply = |run: &str| {
        let (finish, settled, titler) = (finish, settled, Arc::clone(&titler));
        let run = run.to_string();
        async move {
            finish(&run, TurnOutcome::completed(None)).await;
            settled(titler).await;
        }
    };
    assert_eq!(calls(), 1);
    assert!(hinted().await, "a hint for the saved title");
    assert_eq!(
        title().await,
        (
            Some("Inbox plan for stage 5".to_string()),
            InboxState::Pinned
        )
    );

    // Two more replies, one logged three times by a replay: not due,
    // no model call.
    LATER_EXCHANGES.store(2, std::sync::atomic::Ordering::SeqCst);
    reply("r2").await;
    reply("r3").await;
    reply("r3").await;
    reply("r3").await;
    assert_eq!(calls(), 1);
    assert!(!hinted().await, "no title saved, no hint");

    // The third reply since the title: the model re-reads the latest
    // messages and the inbox follows the new topic.
    LATER_EXCHANGES.store(3, std::sync::atomic::Ordering::SeqCst);
    reply("r4").await;
    assert_eq!(calls(), 2);
    assert!(hinted().await, "the new title is hinted");
    assert_eq!(title().await.0.as_deref(), Some("Deploy pipeline rollout"));

    // A slower request that read the title before that one cannot
    // overwrite it or move its count back.
    let owner = ProfileOwner::from(&scope);
    let stale = SavedTitle {
        title: "Inbox plan, stale".into(),
        replies: Some(2),
    };
    let written = engine
        .store()
        .inbox_set_title(&owner, &chat, &stale, Some(1), now_ms())
        .await
        .unwrap();
    assert!(!written);
    let saved = engine
        .store()
        .inbox_title(&owner, &chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.title, "Deploy pipeline rollout");
    assert_eq!(saved.replies, Some(4));

    // The count restarts from that reply.
    reply("r5").await;
    reply("r6").await;
    assert_eq!(calls(), 2);

    // Reply 7 is due and its call is slow. Replies 8 to 10 complete
    // during it; the sidebar sees the title pending; once the call
    // ends, the re-title due at reply 10 still runs.
    SLOW_MODEL.store(true, std::sync::atomic::Ordering::SeqCst);
    finish("r7", TurnOutcome::completed(None)).await;
    for _ in 0..300 {
        if calls() == 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    for run in ["r8", "r9", "r10"] {
        finish(run, TurnOutcome::completed(None)).await;
    }
    let pending = list_of(&orchestrated, "u1").await;
    assert!(
        pending
            .rows
            .iter()
            .any(|r| r.thread_id == "chat" && r.title_pending)
    );
    settled(Arc::clone(&titler)).await;
    SLOW_MODEL.store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(calls(), 4);
    let saved = engine
        .store()
        .inbox_title(&owner, &chat)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.replies, Some(10));
    let done = list_of(&orchestrated, "u1").await;
    assert!(done.rows.iter().all(|r| !r.title_pending));
}

#[test]
fn titles_are_cleaned_to_one_bounded_line() {
    assert_eq!(
        clean_title("  **\"Fix login loop.\"**\nextra").as_deref(),
        Some("Fix login loop")
    );
    assert_eq!(
        clean_title("Title: Budget review").as_deref(),
        Some("Budget review")
    );
    assert_eq!(clean_title("  \n "), None);
    assert_eq!(
        clean_title(&"é".repeat(200)).map(|t| t.chars().count()),
        Some(80)
    );
}
