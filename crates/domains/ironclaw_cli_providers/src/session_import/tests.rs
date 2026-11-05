//! Import failure, crash-consistency, privacy and budget tests over a real
//! engine and libSQL file, with the Codex and Claude drivers registered
//! (never started). Failures are injected at each write before the ledger
//! commit.

use std::os::unix::fs::PermissionsExt;
use std::sync::Weak;

use ironclaw_host_api::ids::{TenantId, UserId};
use ironclaw_orchestrator::runtime::TurnEventSink;

use super::*;
use crate::claude::{ClaudeCliConfig, ClaudeCliRuntime};
use crate::codex::{CodexConfig, CodexRuntime, cwd_file_in, thread_file_in, transcript_in};

const CODEX_ID: &str = "01a19dc0-9936-7d50-8808-84fea8a23fa6";

struct Harness {
    _dir: tempfile::TempDir,
    root: PathBuf,
    app: PathBuf,
    rollout: PathBuf,
    engine: Arc<Engine>,
    importer: SessionImporter,
}

fn scope() -> ThreadScope {
    ThreadScope {
        tenant_id: TenantId::new("t").unwrap(),
        user_id: UserId::new("u").unwrap(),
        agent_id: None,
        project_id: None,
    }
}

fn message(role: &str, at: &str, text: &str) -> String {
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

fn write(path: &Path, lines: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
}

fn append(path: &Path, lines: &[String]) {
    let mut body = std::fs::read_to_string(path).unwrap();
    for l in lines {
        body.push_str(l);
        body.push('\n');
    }
    std::fs::write(path, body).unwrap();
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let app = root.join("app");
    std::fs::create_dir_all(&app).unwrap();
    let rollout = root
        .join(".codex/sessions/2026/10/02")
        .join(format!("rollout-2026-10-02T09-00-00-{CODEX_ID}.jsonl"));
    let meta = serde_json::json!({
        "timestamp": "2026-10-02T09:00:00.000Z", "type": "session_meta",
        "payload": {"id": CODEX_ID, "cwd": app, "originator": "codex_cli_rs", "source": "cli"},
    })
    .to_string();
    write(
        &rollout,
        &[
            meta,
            message("user", "2026-10-02T09:00:02.000Z", "Why is CI red?"),
            message(
                "assistant",
                "2026-10-02T09:00:09.000Z",
                "A snapshot drifted.",
            ),
        ],
    );
    let claude_dir = root
        .join(".claude/projects")
        .join(claude_codes::transcript::encode_project_dir(&app));
    let row = serde_json::json!({
        "type": "user", "sessionId": "c1", "cwd": app, "timestamp": "2026-10-01T10:00:00Z",
        "message": {"role": "user", "content": "Tidy the README"},
    })
    .to_string();
    write(&claude_dir.join("c1.jsonl"), &[row]);

    let db = Arc::new(
        libsql::Builder::new_local(root.join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open(Store::new(runtime), RuntimeId::new("ironclaw").unwrap())
        .await
        .unwrap();
    let sink = Arc::downgrade(&engine) as Weak<dyn TurnEventSink>;
    let state = root.join(".ironclaw/codex");
    let codex = CodexRuntime::new(
        CodexConfig {
            runtime_id: "codex".into(),
            display_name: "Codex CLI".into(),
            binary: root.join("codex"),
            work_dir: state.join("work"),
            state_dir: state.clone(),
            login_file: root.join("auth.json"),
            reasoning_effort: None,
        },
        sink.clone(),
        None,
    )
    .unwrap();
    engine.register_runtime(Arc::new(codex));
    let work = root.join(".ironclaw/claude-cli");
    let claude = ClaudeCliRuntime::new(
        ClaudeCliConfig {
            runtime_id: "claude-cli".into(),
            display_name: "Claude CLI".into(),
            binary: root.join("claude"),
            config_dir: None,
            work_dir: work.clone(),
            login_file: root.join(".claude.json"),
            projects_dir: root.join(".claude/projects"),
            usage_file: None,
        },
        sink,
        None,
    )
    .unwrap();
    engine.register_runtime(Arc::new(claude));
    let importer = SessionImporter::new(ImportPaths {
        claude_projects: root.join(".claude/projects"),
        codex_sessions: root.join(".codex/sessions"),
        codex_state: state,
        claude_imports: crate::claude::imports_dir(&work),
        own_roots: vec![root.join(".ironclaw")],
    });
    Harness {
        _dir: dir,
        root,
        app,
        rollout,
        engine,
        importer,
    }
}

impl Harness {
    async fn run(&self) -> BTreeMap<String, SessionImportCounts> {
        self.importer
            .run(&self.engine, &scope(), ImportTrigger::Manual)
            .await
            .unwrap()
    }

    fn journal(&self) -> PathBuf {
        transcript_in(&self.root.join(".ironclaw/codex"), CODEX_ID)
    }

    /// The Codex thread's history texts, through the driver.
    async fn codex_history(&self) -> Vec<String> {
        let owner = ProfileOwner::from(&scope());
        let ledger = self
            .engine
            .store()
            .import_ledger(&owner, CODEX)
            .await
            .unwrap();
        let entry = &ledger[CODEX_ID];
        let thread = self
            .engine
            .store()
            .thread(&entry.thread_id)
            .await
            .unwrap()
            .unwrap();
        self.engine
            .external_history(&thread)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.text)
            .collect()
    }
}

#[tokio::test]
async fn an_interrupted_new_codex_import_is_retried_not_skipped() {
    for step in [Step::Transcript, Step::ThreadFile, Step::Workspace] {
        let h = harness().await;
        h.importer.fail_at(Some(step));
        let failed = h.run().await;
        assert_eq!(failed[CODEX].failed, 1, "{step:?}");
        assert_eq!(failed[CODEX].imported, 0, "{step:?}");
        let owner = ProfileOwner::from(&scope());
        assert!(
            h.engine
                .store()
                .import_ledger(&owner, CODEX)
                .await
                .unwrap()
                .is_empty()
        );

        // What was written before the failure does not make the session
        // look like one the app started: the retry imports it, once.
        h.importer.fail_at(None);
        let retried = h.run().await;
        assert_eq!(retried[CODEX].imported, 1, "{step:?}: {retried:?}");
        assert_eq!(retried[CODEX].skipped, 0, "{step:?}");
        assert_eq!(
            h.codex_history().await,
            ["Why is CI red?", "A snapshot drifted."],
            "{step:?}"
        );
    }
}

#[tokio::test]
async fn an_interrupted_append_is_not_written_twice() {
    let h = harness().await;
    h.run().await;
    append(
        &h.rollout,
        &[
            message("user", "2026-10-02T10:00:00.000Z", "Now the lint job"),
            message("assistant", "2026-10-02T10:00:05.000Z", "Fixed too."),
        ],
    );
    // The batch reaches the transcript, the ledger does not move.
    h.importer.fail_at(Some(Step::Appended));
    assert_eq!(h.run().await[CODEX].failed, 1);
    h.importer.fail_at(None);
    let counts = h.run().await;
    assert_eq!(counts[CODEX].updated, 1);
    assert_eq!(
        h.codex_history().await,
        [
            "Why is CI red?",
            "A snapshot drifted.",
            "Now the lint job",
            "Fixed too."
        ]
    );
    // And the next run finds nothing new.
    assert_eq!(h.run().await[CODEX].unchanged, 1);
    assert_eq!(h.codex_history().await.len(), 4);
}

#[tokio::test]
async fn a_torn_append_is_closed_off_and_completed_once() {
    let h = harness().await;
    h.run().await;
    append(
        &h.rollout,
        &[
            message("user", "2026-10-02T10:00:00.000Z", "Now the lint job"),
            message("assistant", "2026-10-02T10:00:05.000Z", "Fixed too."),
        ],
    );
    // A crash mid-write left half a line at the end of the copy.
    let mut body = std::fs::read_to_string(h.journal()).unwrap();
    body.push_str(r#"{"role":"user","text":"Now the li"#);
    std::fs::write(h.journal(), body).unwrap();

    assert_eq!(h.run().await[CODEX].updated, 1);
    assert_eq!(
        h.codex_history().await,
        [
            "Why is CI red?",
            "A snapshot drifted.",
            "Now the lint job",
            "Fixed too."
        ]
    );
    // The fragment stays a line of its own; every other line parses.
    let raw = std::fs::read_to_string(h.journal()).unwrap();
    let broken: Vec<_> = raw
        .lines()
        .filter(|l| serde_json::from_str::<serde_json::Value>(l).is_err())
        .collect();
    assert_eq!(broken, [r#"{"role":"user","text":"Now the li"#]);
}

#[tokio::test]
async fn imported_files_are_private() {
    let h = harness().await;
    let counts = h.run().await;
    assert_eq!(
        (counts[CODEX].imported, counts[CLAUDE_CODE].imported),
        (1, 1)
    );
    let state = h.root.join(".ironclaw/codex");
    let imports = crate::claude::imports_dir(&h.root.join(".ironclaw/claude-cli"));
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    for file in [
        h.journal(),
        thread_file_in(&state, CODEX_ID),
        cwd_file_in(&state, CODEX_ID),
        imports.join("c1.json"),
    ] {
        assert_eq!(mode(&file), 0o600, "{}", file.display());
    }
    for dir in [state.join("transcripts"), state.join("threads"), imports] {
        assert_eq!(mode(&dir), 0o700, "{}", dir.display());
    }
}

#[tokio::test]
async fn a_long_rollout_imports_in_bounded_batches_across_runs() {
    let mut h = harness().await;
    let mut more = Vec::new();
    let mut expected = vec![
        "Why is CI red?".to_string(),
        "A snapshot drifted.".to_string(),
    ];
    for n in 0..12 {
        let at = format!("2026-10-02T11:{n:02}:00.000Z");
        more.push(message("user", &at, &format!("step {n}")));
        more.push(message("assistant", &at, &format!("done {n}")));
        expected.push(format!("step {n}"));
        expected.push(format!("done {n}"));
    }
    append(&h.rollout, &more);
    // Two messages per batch, and a budget spent by the first batch: each
    // run takes a batch, records it, and leaves the rest for the next.
    h.importer.set_limits(Limits {
        bytes: 1,
        batch: 2,
        ..Limits::default()
    });
    let mut runs = 0;
    loop {
        runs += 1;
        let counts = h.run().await;
        assert_eq!(counts[CODEX].failed, 0);
        if counts[CODEX].deferred == 0 && runs > 1 {
            break;
        }
        assert!(runs < 40, "the import must progress");
    }
    assert!(runs > 5, "the work was spread over runs ({runs})");
    assert_eq!(
        h.codex_history().await,
        expected,
        "every message once, in order"
    );
    let _ = &h.app;
}
