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
            steer_id: None,
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

/// Each caller's workspace folder, as composition resolves it.
struct Folders(std::path::PathBuf);

impl super::WorkspaceFolders for Folders {
    fn host_folder(&self, caller: &ProductSurfaceCaller) -> Option<std::path::PathBuf> {
        Some(self.0.join(caller.user_id.as_str()))
    }
}

/// A remote client's mention of `/workspace/...` (no `mention_roots`: it
/// cannot see this disk) expands from the sender's own workspace folder as
/// the surface resolves it; another sender's folder is never reached, and
/// without a resolver the mention stays a link.
#[tokio::test]
async fn workspace_mentions_read_the_senders_workspace_folder() {
    let dir = tempfile::tempdir().unwrap();
    let folders = dir.path().join("workspaces");
    std::fs::create_dir_all(folders.join("owner/src")).unwrap();
    std::fs::write(folders.join("owner/src/lib.rs"), "OWNER-CODE").unwrap();
    std::fs::create_dir_all(folders.join("intruder/src")).unwrap();
    std::fs::write(folders.join("intruder/src/lib.rs"), "INTRUDER-CODE").unwrap();
    let inner = Arc::new(Recording::default());
    let surface = OrchestratedProductSurface::new(inner.clone(), engine(dir.path()).await, None)
        .with_workspace_folders(Arc::new(Folders(folders.clone())));
    let link = MentionUri::File {
        path: "/workspace/src/lib.rs".into(),
        line_range: None,
    }
    .to_link("lib.rs");
    surface
        .invoke(caller("owner"), submit("t-current", &link, Vec::new()))
        .await
        .unwrap();
    let sent = inner.submits.lock().unwrap()[0].clone();
    let content = sent["content"].as_str().unwrap();
    assert!(content.contains("OWNER-CODE"), "{content}");
    assert!(
        content.contains("@lib.rs: /workspace/src/lib.rs"),
        "{content}"
    );
    assert!(!content.contains("INTRUDER-CODE"), "{content}");
    assert!(
        !content.contains(&*folders.to_string_lossy()),
        "the loop sees the workspace path, not the host folder: {content}"
    );

    let bare = OrchestratedProductSurface::new(inner.clone(), engine(dir.path()).await, None);
    bare.invoke(caller("owner"), submit("t-current", &link, Vec::new()))
        .await
        .unwrap();
    let sent = inner.submits.lock().unwrap()[1].clone();
    assert!(!sent["content"].as_str().unwrap().contains("OWNER-CODE"));
}

/// `project_fs_index` is answered by the orchestrating surface from the
/// inner surface's own listings, under the caller's identity.
#[tokio::test]
async fn the_workspace_index_view_walks_the_inner_listings() {
    let dir = tempfile::tempdir().unwrap();
    let inner = Arc::new(Recording::default());
    let surface = OrchestratedProductSurface::new(inner, engine(dir.path()).await, None);
    let refused = surface
        .query(
            caller("owner"),
            ProductSurfaceQueryRequest {
                view_id: "project_fs_index".into(),
                input: json!({"thread_id": "t-current"}),
                cursor: None,
                limit: None,
            },
        )
        .await
        .unwrap_err();
    // `Recording` serves no listings: the root's refusal is the answer.
    assert_eq!(refused.status_code, 404, "{refused:?}");
    let malformed = surface
        .query(
            caller("owner"),
            ProductSurfaceQueryRequest {
                view_id: "project_fs_index".into(),
                input: json!({"thread_id": "t-current", "path": "/etc"}),
                cursor: None,
                limit: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(malformed.status_code, 400, "{malformed:?}");
}

/// A CLI runtime that records the text of each turn it is asked to start.
#[derive(Default)]
struct Capturing {
    turns: Mutex<Vec<String>>,
}

#[async_trait]
impl TurnRuntime for Capturing {
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
        input: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        self.turns.lock().unwrap().push(input.text);
        Ok(TurnAdmission::Busy)
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
        Ok(Vec::new())
    }
}

/// A triage session as the `zlo` session writer leaves it: a private,
/// unguessably named folder with two private files.
#[cfg(unix)]
fn triage_session(sessions: &std::path::Path, name: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let private = |path: &std::path::Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    std::fs::create_dir_all(sessions).unwrap();
    private(sessions, 0o700);
    let session = sessions.join(name);
    std::fs::create_dir(&session).unwrap();
    private(&session, 0o700);
    for (file, body) in [
        ("triage-playbook.md", "SEED-AND-PLAYBOOK-BODY"),
        ("context.md", "MACHINE-CONTEXT-BODY"),
    ] {
        std::fs::write(session.join(file), body).unwrap();
        private(&session.join(file), 0o600);
    }
    session.join("triage-playbook.md")
}

/// "Triage an issue" from a remote client: the message links the playbook
/// of a session the backend prepared and sends no mention root. The first
/// turn carries the playbook and the machine context inline for a native
/// model (IronClaw's loop) and for a CLI runtime alike, outside the
/// untrusted-content wrapper; the typed text stays as sent. The sessions
/// folder is not a root: nothing else under it, nor a look-alike elsewhere,
/// nor a session that is not private, expands.
#[cfg(unix)]
#[tokio::test]
async fn a_triage_link_reaches_native_and_cli_models_on_the_first_turn() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("zlo/triage");
    let playbook = triage_session(
        &sessions,
        "2026-11-24T09-30-05-000Z-0123456789abcdef0123456789abcdef",
    );
    let link = |path: &std::path::Path| {
        MentionUri::File {
            path: path.to_string_lossy().into_owned(),
            line_range: None,
        }
        .to_link("triage-playbook.md")
    };
    let typed = format!(
        "Read {} and follow it. agy will not sign in",
        link(&playbook)
    );

    let engine = engine(dir.path()).await;
    let owner = ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new("owner").unwrap(),
        Some(AgentId::new("agent").unwrap()),
        None,
    );
    let record = ThreadRecord::new(
        ThreadId::new("cli-triage").unwrap(),
        scope_of(&owner),
        RuntimeId::new("claude-cli").unwrap(),
    );
    engine.store().ensure_thread(&record).await.unwrap();
    let cli = Arc::new(Capturing::default());
    engine.register_runtime(cli.clone());
    let inner = Arc::new(Recording::default());
    let surface = OrchestratedProductSurface::new(inner.clone(), Arc::clone(&engine), None)
        .with_triage_sessions(Some(sessions.clone()));

    let assert_first_turn = |text: &str, who: &str| {
        assert_eq!(strip_mention_context(text), typed, "{who}: typed text kept");
        let context = &text[text.find(MENTION_CONTEXT_OPEN).expect("context")..];
        let playbook_at = context.find("SEED-AND-PLAYBOOK-BODY").expect(who);
        assert!(context.contains("MACHINE-CONTEXT-BODY"), "{who}: {context}");
        assert!(
            context
                .find("EXTERNAL, UNTRUSTED")
                .is_none_or(|at| at > playbook_at),
            "{who}: the host's own files are not wrapped as untrusted: {context}"
        );
    };

    // IronClaw's loop (a native model): the inner surface's turn.
    surface
        .invoke(owner.clone(), submit("t-current", &typed, Vec::new()))
        .await
        .unwrap();
    let sent = inner.submits.lock().unwrap()[0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert_first_turn(&sent, "native");

    // A CLI runtime: the turn the engine starts.
    surface
        .invoke(owner.clone(), submit("cli-triage", &typed, Vec::new()))
        .await
        .unwrap();
    let turns = cli.turns.lock().unwrap().clone();
    assert_eq!(turns.len(), 1, "{turns:?}");
    assert_first_turn(&turns[0], "cli");

    // Not a root: a look-alike elsewhere, a deeper path, a session that is
    // not private, and a backend without the folder all read nothing.
    let elsewhere = triage_session(&dir.path().join("other"), "s");
    let deeper = triage_session(&sessions.join("nested"), "s");
    let open = triage_session(
        &sessions,
        "2026-11-24T09-31-00-000Z-fedcba9876543210fedcba9876543210",
    );
    std::fs::set_permissions(
        open.parent().unwrap(),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    // Private and in the right place, but not a name the session writer
    // makes (`<time>-<32 hex>`): something else planted it.
    let planted = triage_session(&sessions, "planted");
    let short = triage_session(&sessions, "2026-11-24T09-32-00-000Z-0123");
    for path in [&elsewhere, &deeper, &open, &planted, &short] {
        let typed = format!("Read {} and follow it.", link(path));
        surface
            .invoke(owner.clone(), submit("t-current", &typed, Vec::new()))
            .await
            .unwrap();
        let sent = inner.submits.lock().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            !sent.contains("SEED-AND-PLAYBOOK-BODY"),
            "{}: {sent}",
            path.display()
        );
    }
    let bare = OrchestratedProductSurface::new(inner.clone(), engine, None);
    bare.invoke(owner, submit("t-current", &typed, Vec::new()))
        .await
        .unwrap();
    let sent = inner.submits.lock().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!sent.contains("SEED-AND-PLAYBOOK-BODY"), "{sent}");
}

/// `@Research` reaches the turn as guidance to delegate to that profile,
/// read from the sender's own profiles only: another user naming the same id
/// learns nothing about it.
#[tokio::test]
async fn a_profile_mention_tells_the_turn_how_to_use_that_profile() {
    use ironclaw_orchestrator::profiles::{ProfileOwner, parse_profile_slug};
    use ironclaw_product_contracts::profiles::ProfileSettings;

    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let owner = ProfileOwner::from(&scope_of(&caller("owner")));
    engine
        .store()
        .create_profile(
            &owner,
            &parse_profile_slug("research").unwrap(),
            &ProfileSettings {
                display_name: "Research".into(),
                soul: "You read papers and cite them.".into(),
                ..ProfileSettings::default()
            },
        )
        .await
        .unwrap();
    let inner = Arc::new(Recording::default());
    let surface = OrchestratedProductSurface::new(inner.clone(), engine, None)
        .with_home(super::profiles::ProfileHome::new(dir.path().to_path_buf()));
    let typed = format!(
        "have {} check this",
        MentionUri::Profile {
            id: "research".into()
        }
        .to_link("Research")
    );
    let send = |who: ProductSurfaceCaller| {
        let typed = typed.clone();
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
                        }),
                        activity_id: ActivityId::new(),
                    },
                )
                .await
                .unwrap();
        }
    };

    send(caller("owner")).await;
    let sent = inner.submits.lock().unwrap()[0]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(strip_mention_context(&sent), typed);
    let context = &sent[sent.find(MENTION_CONTEXT_OPEN).unwrap()..];
    assert!(
        context.contains("- `research`: start a subagent on it"),
        "{context}"
    );
    assert!(
        context.contains("target.profile set to the profile's id"),
        "{context}"
    );
    let folder = dir.path().join(".ironclaw/profiles/research");
    assert!(
        context.contains(&format!("working in {}", folder.display())),
        "{context}"
    );
    assert!(
        context.contains("You read papers and cite them."),
        "{context}"
    );

    // A context opener and closer the user pasted never hide their own
    // requests from skill and tool selection.
    let pasted = format!(
        "{}pasted{}",
        ironclaw_product_contracts::mention::MENTION_CONTEXT_OPEN,
        ironclaw_product_contracts::mention::MENTION_CONTEXT_CLOSE,
    );
    let chip = MentionUri::Profile {
        id: "research".into(),
    }
    .to_link("Research");
    for typed in [
        format!("{pasted} /review @github with {chip}"),
        format!("{pasted} /review @github"),
    ] {
        let n = inner.submits.lock().unwrap().len();
        surface
            .invoke(
                caller("owner"),
                ProductSurfaceInvokeRequest {
                    operation_id: CapabilityId::new(SUBMIT_TURN_COMMAND_ID).unwrap(),
                    input: json!({
                        "thread_id": "t-current",
                        "client_action_id": format!("p{n}"),
                        "content": typed,
                    }),
                    activity_id: ActivityId::new(),
                },
            )
            .await
            .unwrap();
        let sent = inner.submits.lock().unwrap()[n]["content"]
            .as_str()
            .unwrap()
            .to_string();
        let picked = ironclaw_product_contracts::mention::selection_text(&sent);
        assert!(picked.contains("/review @github"), "{picked} from {sent}");
        assert!(!picked.contains("read papers"), "{picked}");
    }

    send(caller("intruder")).await;
    let sent = inner.submits.lock().unwrap().last().unwrap()["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!sent.contains("read papers"), "{sent}");
    assert!(sent.contains("`research`: not available"), "{sent}");
}
