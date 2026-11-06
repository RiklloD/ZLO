//! @-mentions through the orchestrating surface's `turn.submit`: the turn
//! receives the typed text with its mentions expanded inside the granted
//! project folder, a thread mention reads only the sender's own threads,
//! nothing expands in a thread the sender cannot read, credential files
//! never expand, and `mention_roots` never reaches the inner surface.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_host_api::ids::{ActivityId, AgentId, CapabilityId, TenantId, ThreadId, UserId};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::model::{RuntimeId, ThreadRecord};
use ironclaw_orchestrator::runtime::{
    CreateThread, CreatedThread, HistoryMessage, HistoryRole, RuntimeCapabilities, RuntimeError,
    SteerHow, SteerOutcome, TurnAdmission, TurnInput, TurnRuntime,
};
use ironclaw_product_contracts::mention::{
    MENTION_CONTEXT_OPEN, MentionUri, strip_mention_context,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use serde_json::{Value, json};

use super::surface::{OrchestratedProductSurface, scope_of};
use crate::reborn_services::{SUBMIT_TURN_COMMAND_ID, TIMELINE_VIEW};

/// IronClaw's own surface: records each submit, serves every caller their
/// own `t-current`, and `t-mine`'s timeline to its owner only.
#[derive(Default)]
struct Recording {
    submits: Mutex<Vec<Value>>,
}

#[async_trait]
impl ProductSurface for Recording {
    async fn invoke(
        &self,
        _: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        if request.operation_id.as_str() == SUBMIT_TURN_COMMAND_ID {
            self.submits.lock().unwrap().push(request.input);
        }
        Ok(ProductSurfaceInvokeResponse { output: json!({}) })
    }
    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        if request.view_id == TIMELINE_VIEW.id && request.input["thread_id"] == "t-current" {
            let page = json!({"messages": [], "summary_artifacts": []});
            return Ok(serde_json::from_value(json!({"items": [page]})).unwrap());
        }
        let mine = request.view_id == TIMELINE_VIEW.id
            && request.input["thread_id"] == "t-mine"
            && caller.user_id.as_str() == "owner";
        if !mine {
            return Err(ProductSurfaceError::not_found());
        }
        let page = json!({
            "messages": [
                {"kind": "user", "content": "how do cookies expire?"},
                {"kind": "tool_result_reference", "content": "raw tool row"},
                {"kind": "assistant", "content": "After the max-age."},
            ],
            "summary_artifacts": [{"content": "Login cookie work."}],
        });
        Ok(serde_json::from_value(json!({"items": [page], "next_cursor": null})).unwrap())
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
async fn submit_expands_mentions_inside_the_root_and_the_senders_threads_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let inner = Arc::new(Recording::default());
    let surface = OrchestratedProductSurface::new(inner.clone(), engine, None);

    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("notes.md"), "PROJECT-NOTES").unwrap();
    std::fs::write(dir.path().join("outside.md"), "OUTSIDE-SECRET").unwrap();
    // Credentials with no token prefix a leak scanner would recognize.
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::write(
        project.join(".git/config"),
        "[remote \"origin\"]\n\turl = https://me:plainpassword42@example.com/r.git\n",
    )
    .unwrap();
    std::fs::create_dir_all(project.join(".claude")).unwrap();
    std::fs::write(
        project.join(".claude/.credentials.json"),
        "{\"pw\":\"swordfish77\"}",
    )
    .unwrap();
    let file = |path: std::path::PathBuf| {
        MentionUri::File {
            path: path.to_string_lossy().into_owned(),
            line_range: None,
        }
        .to_link("f")
    };
    let typed = format!(
        "compare {} {} {} {} with {}",
        file(project.join("notes.md")),
        file(dir.path().join("outside.md")),
        file(project.join(".git/config")),
        file(project.join(".claude/.credentials.json")),
        MentionUri::Thread {
            id: "t-mine".into()
        }
        .to_link("Cookies"),
    );

    let send = |who: ProductSurfaceCaller| {
        let typed = typed.clone();
        let roots = vec![project.to_string_lossy().into_owned()];
        let surface = &surface;
        async move {
            surface
                .invoke(
                    who,
                    ProductSurfaceInvokeRequest {
                        operation_id: CapabilityId::new(SUBMIT_TURN_COMMAND_ID).unwrap(),
                        input: json!({
                            "thread_id": "t-current",
                            "client_action_id": "a1",
                            "content": typed,
                            "mention_roots": roots,
                        }),
                        activity_id: ActivityId::new(),
                    },
                )
                .await
                .unwrap();
        }
    };

    send(caller("owner")).await;
    let sent = inner.submits.lock().unwrap()[0].clone();
    assert!(sent.get("mention_roots").is_none(), "{sent}");
    let content = sent["content"].as_str().unwrap();
    assert_eq!(strip_mention_context(content), typed);
    let context = &content[content.find(MENTION_CONTEXT_OPEN).unwrap()..];
    assert!(context.contains("PROJECT-NOTES"), "{context}");
    assert!(!context.contains("OUTSIDE-SECRET"), "{context}");
    assert!(context.contains("outside the project folder"), "{context}");
    assert!(context.contains("Login cookie work."), "{context}");
    assert!(
        context.contains("User: how do cookies expire?"),
        "{context}"
    );
    assert!(
        context.contains("Assistant: After the max-age."),
        "{context}"
    );
    assert!(!context.contains("raw tool row"), "{context}");
    for secret in ["plainpassword42", "swordfish77"] {
        assert!(!context.contains(secret), "{secret} leaked: {context}");
    }
    assert_eq!(context.matches("sensitive file").count(), 2, "{context}");

    // Another user naming the same thread reads nothing from it.
    send(caller("intruder")).await;
    let sent = inner.submits.lock().unwrap()[1].clone();
    let content = sent["content"].as_str().unwrap();
    assert!(!content.contains("cookies expire"), "{content}");
    assert!(
        content.contains("@Cookies: thread not available."),
        "{content}"
    );
}

async fn engine(dir: &std::path::Path) -> Arc<Engine> {
    let db = Arc::new(
        libsql::Builder::new_local(dir.join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    Engine::open_libsql(runtime, "ironclaw").await.unwrap()
}

fn submit(thread: &str, content: &str, roots: Vec<String>) -> ProductSurfaceInvokeRequest {
    ProductSurfaceInvokeRequest {
        operation_id: CapabilityId::new(SUBMIT_TURN_COMMAND_ID).unwrap(),
        input: json!({
            "thread_id": thread,
            "client_action_id": "a1",
            "content": content,
            "mention_roots": roots,
        }),
        activity_id: ActivityId::new(),
    }
}

/// A message sent into a thread the sender cannot read expands nothing:
/// the probe runs before any disk read.
#[tokio::test]
async fn nothing_expands_in_a_thread_the_sender_cannot_read() {
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(Recording::default());
    let surface = OrchestratedProductSurface::new(inner.clone(), engine(dir.path()).await, None);
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("notes.md"), "PROJECT-NOTES").unwrap();
    let link = MentionUri::File {
        path: project.join("notes.md").to_string_lossy().into_owned(),
        line_range: None,
    }
    .to_link("notes.md");
    surface
        .invoke(
            caller("owner"),
            submit(
                "t-foreign",
                &link,
                vec![project.to_string_lossy().into_owned()],
            ),
        )
        .await
        .unwrap();
    let sent = inner.submits.lock().unwrap()[0].clone();
    assert_eq!(sent["content"], link.as_str(), "{sent}");
}

/// A CLI runtime whose session history holds one line.
struct History;

#[async_trait]
impl TurnRuntime for History {
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
            text: "CLI-SESSION-ANSWER".into(),
            tool_calls: Vec::new(),
            run_id: None,
            hidden: false,
            at: None,
        }])
    }
}

/// The production read path, not a fake: a CLI thread's history is served
/// by the orchestrator engine under the caller's scope. Its owner's mention
/// carries it, also from another of the owner's agent profiles (engine
/// ownership is tenant + user: one person's threads span their profiles);
/// the same mention from another user carries nothing.
#[tokio::test]
async fn cli_thread_mentions_use_the_engines_caller_scoped_read() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path()).await;
    let owner = ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new("owner").unwrap(),
        Some(AgentId::new("agent").unwrap()),
        None,
    );
    let mut record = ThreadRecord::new(
        ThreadId::new("cli-mine").unwrap(),
        scope_of(&owner),
        RuntimeId::new("claude-cli").unwrap(),
    );
    record.title = Some("CLI work".into());
    engine.store().ensure_thread(&record).await.unwrap();
    engine.register_runtime(Arc::new(History));
    let inner = Arc::new(Recording::default());
    let surface = OrchestratedProductSurface::new(inner.clone(), engine, None);
    let link = MentionUri::Thread {
        id: "cli-mine".into(),
    }
    .to_link("CLI work");

    let other_profile = ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new("owner").unwrap(),
        Some(AgentId::new("other-agent").unwrap()),
        None,
    );
    for who in [owner, caller("intruder"), other_profile] {
        surface
            .invoke(who, submit("t-current", &link, Vec::new()))
            .await
            .unwrap();
    }
    let sent = inner.submits.lock().unwrap().clone();
    let content = |ix: usize| sent[ix]["content"].as_str().unwrap().to_string();
    assert!(content(0).contains("CLI-SESSION-ANSWER"), "{}", content(0));
    assert!(content(0).contains("thread \"CLI work\""), "{}", content(0));
    for ix in [0, 2] {
        assert!(
            content(ix).contains("CLI-SESSION-ANSWER"),
            "{}",
            content(ix)
        );
    }
    assert!(!content(1).contains("CLI-SESSION-ANSWER"), "{}", content(1));
    assert!(
        content(1).contains("thread not available"),
        "{}",
        content(1)
    );
}

/// Near the native 64 KiB message cap, what reaches the turn stays within
/// it: the context shrinks to what fits, or is left out whole.
#[tokio::test]
async fn submit_near_the_message_cap_stays_within_it() {
    const CAP: usize = 64 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(Recording::default());
    let surface = OrchestratedProductSurface::new(inner.clone(), engine(dir.path()).await, None);
    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("big.txt"), "data ".repeat(10_000)).unwrap();
    let link = MentionUri::File {
        path: project.join("big.txt").to_string_lossy().into_owned(),
        line_range: None,
    }
    .to_link("big.txt");
    let roots = vec![project.to_string_lossy().into_owned()];
    for spare in [10, 500, 3_000, 30_000] {
        let typed = format!("{}{link}", "x".repeat(CAP - spare - link.len()));
        surface
            .invoke(caller("owner"), submit("t-current", &typed, roots.clone()))
            .await
            .unwrap();
    }
    for (ix, sent) in inner.submits.lock().unwrap().iter().enumerate() {
        let content = sent["content"].as_str().unwrap();
        assert!(content.len() <= CAP, "send {ix}: {} > {CAP}", content.len());
    }
}
