//! Terminal session import through its production caller: a real engine on
//! a libSQL file with the Claude and Codex drivers registered (fake binaries),
//! fixture session folders in a temp home (never the person's own), then the
//! importer. Covers what is imported and skipped, the Settled shelf, history
//! through the drivers, idempotent reruns, growth, and a message sent to an
//! imported thread resuming the CLI's own session from its folder.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ironclaw_cli_providers::claude::{ClaudeCliConfig, ClaudeCliRuntime};
use ironclaw_cli_providers::codex::{CodexConfig, CodexRuntime};
use ironclaw_cli_providers::session_import::{CLAUDE_CODE, CODEX, ImportPaths, SessionImporter};
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::inbox::classify;
use ironclaw_orchestrator::model::{RuntimeId, ThreadRecord, ThreadScope, now_ms};
use ironclaw_orchestrator::profiles::ProfileOwner;
use ironclaw_orchestrator::runtime::{HistoryRole, TurnEventSink};
use ironclaw_orchestrator::store::Store;
use ironclaw_product_contracts::inbox::{InboxState, InboxStatus};
use ironclaw_product_contracts::session_import::{
    ImportTrigger, SessionImportCounts, SessionImportRequest,
};

const CLAUDE_SESSION: &str = "5b1d2c3e-1111-4222-8333-444455556666";
const CODEX_SESSION: &str = "01a19dc0-9936-7d50-8808-84fea8a23fa6";

fn scope() -> ThreadScope {
    ThreadScope {
        tenant_id: TenantId::new("t").unwrap(),
        user_id: UserId::new("u").unwrap(),
        agent_id: None,
        project_id: None,
    }
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Logs its argv and physical working directory, then answers one turn.
const CLAUDE: &str = r#"#!/bin/sh
{ echo "argv $*"; echo "cwd $(pwd -P)"; } >> __LOG__
read -r _line
echo '{"type":"result","subtype":"success","is_error":false,"duration_ms":1,"duration_api_ms":1,"num_turns":1,"result":"ok","session_id":"s","total_cost_usd":0,"usage":{"input_tokens":1,"output_tokens":1}}'
"#;

const CODEX_SCRIPT: &str = r#"#!/bin/sh
id_of() { printf '%s' "$1" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p'; }
reply() { printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$1" "$2"; }
emit() { printf '{"jsonrpc":"2.0","method":"%s","params":%s}\n' "$1" "$2"; }
while IFS= read -r line; do
  printf '%s\n' "$line" >> __LOG__
  id=$(id_of "$line")
  case "$line" in
    *'"method":"initialize"'*) reply "$id" '{"userAgent":"codex"}' ;;
    *'"method":"initialized"'*) ;;
    *'"method":"thread/resume"'*) reply "$id" '{"thread":{"id":"__THREAD__"},"model":"gpt-b"}' ;;
    *'"method":"thread/start"'*) reply "$id" '{"thread":{"id":"fresh"},"model":"gpt-b"}' ;;
    *'"method":"turn/start"'*)
      reply "$id" '{"turn":{"id":"tu-1","status":"inProgress"}}'
      emit item/agentMessage/delta '{"threadId":"__THREAD__","turnId":"tu-1","itemId":"m1","delta":"On it"}'
      emit turn/completed '{"threadId":"__THREAD__","turn":{"id":"tu-1","status":"completed","error":null}}' ;;
  esac
done
"#;

struct Home {
    _dir: tempfile::TempDir,
    root: PathBuf,
    app: PathBuf,
    claude_log: PathBuf,
    codex_log: PathBuf,
    claude_transcript: PathBuf,
    codex_rollout: PathBuf,
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut body = lines.join("\n");
    body.push('\n');
    std::fs::write(path, body).unwrap();
}

fn append_lines(path: &Path, lines: &[String]) {
    let mut body = std::fs::read_to_string(path).unwrap();
    for l in lines {
        body.push_str(l);
        body.push('\n');
    }
    std::fs::write(path, body).unwrap();
}

fn claude_row(kind: &str, session: &str, cwd: &Path, at: &str, content: &str) -> String {
    serde_json::json!({
        "type": kind, "uuid": format!("{kind}-{at}"), "sessionId": session,
        "cwd": cwd, "timestamp": at,
        "message": {"role": kind, "content": [{"type": "text", "text": content}]},
    })
    .to_string()
}

fn codex_meta(id: &str, cwd: &Path, originator: &str, source: serde_json::Value) -> String {
    serde_json::json!({
        "timestamp": "2026-10-02T09:00:00.000Z", "type": "session_meta",
        "payload": {"id": id, "timestamp": "2026-10-02T09:00:00.000Z", "cwd": cwd,
                    "originator": originator, "source": source},
    })
    .to_string()
}

fn codex_message(role: &str, at: &str, text: &str) -> String {
    let kind = if role == "user" {
        "input_text"
    } else {
        "output_text"
    };
    serde_json::json!({
        "timestamp": at, "type": "response_item",
        "payload": {"type": "message", "role": role, "content": [{"type": kind, "text": text}]},
    })
    .to_string()
}

/// A home with: a terminal Claude session, one this app started, one with
/// nothing but malformed lines, one under the app's own folder and a
/// subagent file; a terminal Codex rollout, one this app started and a
/// spawned one.
fn home() -> Home {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let app = root.join("work").join("app");
    std::fs::create_dir_all(&app).unwrap();
    let projects = root.join(".claude").join("projects");
    let app_dir = projects.join(claude_codes::transcript::encode_project_dir(&app));

    let claude_transcript = app_dir.join(format!("{CLAUDE_SESSION}.jsonl"));
    write_lines(
        &claude_transcript,
        &[
            claude_row(
                "user",
                CLAUDE_SESSION,
                &app,
                "2026-10-01T10:00:00.000Z",
                "Fix the flaky login test",
            ),
            "{torn json".to_string(),
            claude_row(
                "assistant",
                CLAUDE_SESSION,
                &app,
                "2026-10-01T10:00:30.000Z",
                "Fixed: a race in the fixture.",
            ),
        ],
    );
    write_lines(
        &app_dir.join("zlo-own.jsonl"),
        &[claude_row(
            "user",
            "zlo-own",
            &app,
            "2026-10-01T09:00:00.000Z",
            "mine",
        )],
    );
    write_lines(
        &app_dir.join("empty-1.jsonl"),
        &["not json".into(), "{}".into()],
    );
    write_lines(
        &app_dir.join("agent-a1b2.jsonl"),
        &[claude_row(
            "user",
            "agent-a1b2",
            &app,
            "2026-10-01T09:00:00.000Z",
            "subtask",
        )],
    );
    let own = root.join(".ironclaw").join("profiles").join("default");
    write_lines(
        &projects
            .join(claude_codes::transcript::encode_project_dir(&own))
            .join("own-2.jsonl"),
        &[claude_row(
            "user",
            "own-2",
            &own,
            "2026-10-01T09:00:00.000Z",
            "profile turn",
        )],
    );

    let day = root
        .join(".codex")
        .join("sessions")
        .join("2026")
        .join("10")
        .join("02");
    let codex_rollout = day.join(format!("rollout-2026-10-02T09-00-00-{CODEX_SESSION}.jsonl"));
    write_lines(
        &codex_rollout,
        &[
            codex_meta(
                CODEX_SESSION,
                &app,
                "codex_cli_rs",
                serde_json::json!("cli"),
            ),
            codex_message(
                "user",
                "2026-10-02T09:00:01.000Z",
                "<environment_context>\n<cwd>x</cwd>\n</environment_context>",
            ),
            codex_message("user", "2026-10-02T09:00:02.000Z", "Why is CI red?"),
            "garbage".to_string(),
            codex_message(
                "assistant",
                "2026-10-02T09:00:09.000Z",
                "A snapshot drifted.",
            ),
        ],
    );
    let ours = "01a19dc0-0000-7000-8000-000000000001";
    write_lines(
        &day.join(format!("rollout-2026-10-02T08-00-00-{ours}.jsonl")),
        &[
            codex_meta(ours, &app, "ironclaw", serde_json::json!("vscode")),
            codex_message("user", "2026-10-02T08:00:02.000Z", "from the app"),
        ],
    );
    let spawned = "01a19dc0-0000-7000-8000-000000000002";
    write_lines(
        &day.join(format!("rollout-2026-10-02T08-30-00-{spawned}.jsonl")),
        &[
            codex_meta(
                spawned,
                &app,
                "codex_cli_rs",
                serde_json::json!({"subagent": {}}),
            ),
            codex_message("user", "2026-10-02T08:30:02.000Z", "child task"),
        ],
    );
    Home {
        claude_log: root.join("claude.log"),
        codex_log: root.join("codex.log"),
        claude_transcript,
        codex_rollout,
        app,
        root,
        _dir: dir,
    }
}

async fn engine(home: &Home) -> Arc<Engine> {
    let db = Arc::new(
        libsql::Builder::new_local(home.root.join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open(Store::new(runtime), RuntimeId::new("ironclaw").unwrap())
        .await
        .unwrap();
    let sink = Arc::downgrade(&engine) as std::sync::Weak<dyn TurnEventSink>;
    let claude_bin = script(
        &home.root,
        "claude",
        &CLAUDE.replace("__LOG__", &home.claude_log.display().to_string()),
    );
    let claude = ClaudeCliRuntime::new(
        ClaudeCliConfig {
            runtime_id: "claude-cli".into(),
            display_name: "Claude CLI".into(),
            binary: claude_bin,
            config_dir: None,
            work_dir: home.root.join(".ironclaw").join("claude-cli"),
            login_file: home.root.join(".claude.json"),
            projects_dir: home.root.join(".claude").join("projects"),
            usage_file: None,
        },
        sink.clone(),
        None,
    )
    .unwrap();
    engine.register_runtime(Arc::new(claude));
    let codex_bin = script(
        &home.root,
        "codex",
        &CODEX_SCRIPT
            .replace("__LOG__", &home.codex_log.display().to_string())
            .replace("__THREAD__", CODEX_SESSION),
    );
    let codex = CodexRuntime::new(
        CodexConfig {
            runtime_id: "codex".into(),
            display_name: "Codex CLI".into(),
            binary: codex_bin,
            work_dir: home.root.join(".ironclaw").join("codex").join("work"),
            state_dir: home.root.join(".ironclaw").join("codex"),
            login_file: home.root.join("auth.json"),
            reasoning_effort: None,
        },
        sink,
        None,
    )
    .unwrap();
    engine.register_runtime(Arc::new(codex));
    engine
}

fn importer(home: &Home) -> Arc<SessionImporter> {
    Arc::new(SessionImporter::new(ImportPaths {
        claude_projects: home.root.join(".claude").join("projects"),
        codex_sessions: home.root.join(".codex").join("sessions"),
        codex_state: home.root.join(".ironclaw").join("codex"),
        own_roots: vec![home.root.join(".ironclaw")],
    }))
}

async fn imported_thread(engine: &Engine, native: &str) -> ThreadRecord {
    let owner = ProfileOwner::from(&scope());
    let sources = engine
        .store()
        .imported_thread_sources(&owner)
        .await
        .unwrap();
    for id in sources.keys() {
        let t = engine
            .store()
            .thread(&ThreadId::new(id.as_str()).unwrap())
            .await
            .unwrap()
            .unwrap();
        if t.native_session_id.as_deref() == Some(native) {
            return t;
        }
    }
    panic!("no imported thread for {native}");
}

async fn inbox(engine: &Engine, thread: &ThreadId) -> (InboxState, Option<InboxStatus>) {
    let owner = ProfileOwner::from(&scope());
    let store = engine.store();
    let threads = store.inbox_threads(&owner).await.unwrap();
    let t = threads.iter().find(|t| &t.thread_id == thread).unwrap();
    let records = store.inbox_records(&owner).await.unwrap();
    classify(
        &t.facts,
        records.get(thread.as_str()),
        store.inbox_epoch().await.unwrap(),
        now_ms(),
    )
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

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::test]
async fn terminal_sessions_import_settled_once_grow_and_resume_natively() {
    let home = home();
    let engine = engine(&home).await;
    // A Claude session this app started already has its thread.
    let mut own = ThreadRecord::new(
        ThreadId::new("own-thread").unwrap(),
        scope(),
        RuntimeId::new("claude-cli").unwrap(),
    );
    own.native_session_id = Some("zlo-own".into());
    engine.store().ensure_thread(&own).await.unwrap();

    let importer = importer(&home);
    let counts = importer
        .run(&engine, &scope(), ImportTrigger::Manual)
        .await
        .unwrap();
    assert_eq!(
        counts[CLAUDE_CODE],
        SessionImportCounts {
            found: 3,
            imported: 1,
            skipped: 2,
            ..SessionImportCounts::default()
        },
        "the app's own session and the empty one are skipped"
    );
    assert_eq!(
        counts[CODEX],
        SessionImportCounts {
            found: 3,
            imported: 1,
            skipped: 2,
            ..SessionImportCounts::default()
        },
        "the app's own rollout and the spawned one are skipped"
    );

    // Real threads on the CLI runtimes, keyed by the native ids, original
    // times, and in the Settled shelf with nothing to see.
    let claude = imported_thread(&engine, CLAUDE_SESSION).await;
    assert_eq!(claude.runtime_id.as_str(), "claude-cli");
    assert_eq!(claude.title.as_deref(), Some("Fix the flaky login test"));
    assert_eq!(claude.created_at, 1_790_848_800_000);
    let codex = imported_thread(&engine, CODEX_SESSION).await;
    assert_eq!(codex.runtime_id.as_str(), "codex");
    assert_eq!(codex.title.as_deref(), Some("Why is CI red?"));
    for t in [&claude, &codex] {
        assert_eq!(
            inbox(&engine, &t.thread_id).await,
            (InboxState::Settled, None)
        );
    }
    let sources = engine
        .store()
        .imported_thread_sources(&ProfileOwner::from(&scope()))
        .await
        .unwrap();
    assert_eq!(sources[claude.thread_id.as_str()], CLAUDE_CODE);
    assert_eq!(sources[codex.thread_id.as_str()], CODEX);

    // History through the drivers, original timestamps on each message.
    let history = engine.external_history(&claude).await.unwrap();
    assert_eq!(
        history.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
        ["Fix the flaky login test", "Fixed: a race in the fixture."]
    );
    assert_eq!(history[0].at, Some(1_790_848_800_000));
    let history = engine.external_history(&codex).await.unwrap();
    let visible: Vec<_> = history
        .iter()
        .filter(|m| !m.hidden)
        .map(|m| (m.role, m.text.as_str()))
        .collect();
    assert_eq!(
        visible,
        [
            (HistoryRole::User, "Why is CI red?"),
            (HistoryRole::Assistant, "A snapshot drifted.")
        ]
    );
    assert!(history.iter().all(|m| m.at.is_some()));

    // A rerun changes nothing.
    let again = importer
        .run(&engine, &scope(), ImportTrigger::Manual)
        .await
        .unwrap();
    assert_eq!(again[CLAUDE_CODE].imported + again[CODEX].imported, 0);
    assert_eq!(again[CLAUDE_CODE].unchanged, 1);
    assert_eq!(again[CODEX].unchanged, 1);
    assert_eq!(engine.external_history(&codex).await.unwrap().len(), 3);

    // Both sessions went on in the terminal: new messages come in, the
    // threads stay settled.
    tokio::time::sleep(Duration::from_millis(20)).await;
    append_lines(
        &home.claude_transcript,
        &[claude_row(
            "user",
            CLAUDE_SESSION,
            &home.app,
            "2026-10-01T11:00:00.000Z",
            "and signup",
        )],
    );
    append_lines(
        &home.codex_rollout,
        &[
            codex_message("user", "2026-10-02T10:00:00.000Z", "Now the lint job"),
            codex_message("assistant", "2026-10-02T10:00:05.000Z", "Fixed too."),
        ],
    );
    let grown = importer
        .run(&engine, &scope(), ImportTrigger::Manual)
        .await
        .unwrap();
    assert_eq!((grown[CLAUDE_CODE].updated, grown[CODEX].updated), (1, 1));
    let history = engine.external_history(&codex).await.unwrap();
    assert_eq!(history.len(), 5, "growth appended once");
    assert_eq!(history[4].text, "Fixed too.");
    for t in [&claude, &codex] {
        assert_eq!(
            inbox(&engine, &t.thread_id).await,
            (InboxState::Settled, None)
        );
    }

    // A message sent here resumes the CLI's own session from its folder,
    // and the thread leaves the shelf like any thread.
    let app = std::fs::canonicalize(&home.app).unwrap();
    let claude = imported_thread(&engine, CLAUDE_SESSION).await;
    engine.user_turn(&claude, "continue").await.unwrap();
    wait_for(|| read(&home.claude_log).contains("cwd ")).await;
    let log = read(&home.claude_log);
    assert!(log.contains(&format!("--resume {CLAUDE_SESSION}")), "{log}");
    assert!(log.contains(&format!("cwd {}", app.display())), "{log}");
    assert_ne!(
        inbox(&engine, &claude.thread_id).await.0,
        InboxState::Settled
    );

    let codex = imported_thread(&engine, CODEX_SESSION).await;
    engine.user_turn(&codex, "continue").await.unwrap();
    wait_for(|| read(&home.codex_log).contains("turn/start")).await;
    let log = read(&home.codex_log);
    assert!(log.contains(r#""method":"thread/resume""#), "{log}");
    assert!(
        log.contains(&format!(r#""threadId":"{CODEX_SESSION}""#)),
        "{log}"
    );
    assert!(
        log.contains(&format!(r#""cwd":"{}""#, app.display())),
        "{log}"
    );
    assert_ne!(
        inbox(&engine, &codex.thread_id).await.0,
        InboxState::Settled
    );
}

#[tokio::test]
async fn settings_and_triggers_decide_what_runs() {
    let home = home();
    let engine = engine(&home).await;
    let importer = importer(&home);
    // Codex off: only Claude sessions come in; the launch run happens once.
    let status = importer
        .request(
            &engine,
            &scope(),
            &SessionImportRequest {
                sources: [(CODEX.to_string(), false)].into(),
                run: Some(ImportTrigger::Launch),
                ..SessionImportRequest::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !status
            .sources
            .iter()
            .find(|s| s.source == CODEX)
            .unwrap()
            .enabled
    );
    wait_for(|| !importer.is_running()).await;
    let status = importer
        .status(&engine, &ProfileOwner::from(&scope()))
        .await
        .unwrap();
    assert!(status.last_run_at.is_some());
    let claude = status
        .sources
        .iter()
        .find(|s| s.source == CLAUDE_CODE)
        .unwrap();
    assert!(claude.available && claude.enabled);
    // No thread owns `zlo-own` here, so it is a terminal session too.
    assert_eq!(claude.counts.unwrap().imported, 2);
    assert_eq!(
        status
            .sources
            .iter()
            .find(|s| s.source == CODEX)
            .unwrap()
            .counts,
        None
    );

    // A second launch is not due; a background run waits its interval.
    let before = status.last_run_at;
    for trigger in [ImportTrigger::Launch, ImportTrigger::Background] {
        importer
            .request(
                &engine,
                &scope(),
                &SessionImportRequest {
                    run: Some(trigger),
                    ..SessionImportRequest::default()
                },
            )
            .await
            .unwrap();
        wait_for(|| !importer.is_running()).await;
    }
    let status = importer
        .status(&engine, &ProfileOwner::from(&scope()))
        .await
        .unwrap();
    assert_eq!(status.last_run_at, before);

    // An unknown source is a correctable error.
    let bad = importer
        .request(
            &engine,
            &scope(),
            &SessionImportRequest {
                sources: [("cursor".to_string(), true)].into(),
                ..SessionImportRequest::default()
            },
        )
        .await;
    assert!(bad.is_err());
}
