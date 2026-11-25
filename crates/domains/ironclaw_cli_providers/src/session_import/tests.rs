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
    /// Built but not registered yet (`harness_without_runtimes`).
    pending: Vec<Arc<dyn ironclaw_orchestrator::runtime::TurnRuntime>>,
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
    let mut h = harness_without_runtimes().await;
    h.install_clis();
    h
}

/// A laptop whose CLIs the backend has not found yet.
async fn harness_without_runtimes() -> Harness {
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
            autocompact: None,
            artifacts: false,
            skills: Default::default(),
        },
        sink,
        None,
    )
    .unwrap();
    let pending: Vec<Arc<dyn ironclaw_orchestrator::runtime::TurnRuntime>> =
        vec![Arc::new(codex), Arc::new(claude)];
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
        pending,
    }
}

impl Harness {
    /// The backend found the CLIs (boot, or the live rescan).
    fn install_clis(&mut self) {
        for runtime in self.pending.drain(..) {
            self.engine.register_runtime(runtime);
        }
    }

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

#[tokio::test]
async fn a_spent_budget_on_one_source_does_not_defer_the_other() {
    let mut h = harness().await;
    // The first source read spends the whole byte budget.
    h.importer.set_limits(Limits {
        bytes: 1,
        ..Limits::default()
    });
    let counts = h.run().await;
    assert_eq!(counts[CLAUDE_CODE].imported, 1);
    assert_eq!(counts[CODEX].deferred, 0, "{counts:?}");
    assert_eq!(counts[CODEX].imported, 1, "{counts:?}");
}

#[tokio::test]
async fn a_launch_that_found_no_cli_does_not_use_up_the_launch_import() {
    let mut h = harness_without_runtimes().await;
    let importer = Arc::new(std::mem::replace(
        &mut h.importer,
        SessionImporter::new(ImportPaths::from_home(&h.root)),
    ));
    let launch = SessionImportRequest {
        run: Some(ImportTrigger::Launch),
        ..SessionImportRequest::default()
    };
    let wait = |importer: Arc<SessionImporter>| async move {
        for _ in 0..400 {
            if !importer.is_running() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the run did not end");
    };
    let first = importer
        .request(&h.engine, &scope(), &launch)
        .await
        .unwrap();
    assert!(first.started, "the first launch runs");
    wait(Arc::clone(&importer)).await;
    // Nothing was scannable, so nothing was imported or counted.
    let status = importer
        .status(&h.engine, &ProfileOwner::from(&scope()))
        .await
        .unwrap();
    assert!(status.last_run_at.is_some());
    assert!(status.sources.iter().all(|s| s.counts.is_none()));

    // The CLIs turn up (an install, then the live rescan or a relaunch).
    h.install_clis();
    let again = importer
        .request(&h.engine, &scope(), &launch)
        .await
        .unwrap();
    assert!(again.started, "a source never scanned makes the launch due");
    wait(Arc::clone(&importer)).await;
    let status = importer
        .status(&h.engine, &ProfileOwner::from(&scope()))
        .await
        .unwrap();
    let imported = |id: &str| {
        status
            .sources
            .iter()
            .find(|s| s.source == id)
            .and_then(|s| s.counts)
            .map(|c| c.imported)
    };
    assert_eq!(imported(CLAUDE_CODE), Some(1));
    assert_eq!(imported(CODEX), Some(1));

    // Both scanned: a further launch is not due.
    let third = importer
        .request(&h.engine, &scope(), &launch)
        .await
        .unwrap();
    assert!(!third.started);
}

#[tokio::test]
async fn one_import_request_goes_on_until_nothing_is_deferred() {
    let mut h = harness().await;
    let mut expected = vec![
        "Why is CI red?".to_string(),
        "A snapshot drifted.".to_string(),
    ];
    let mut more = Vec::new();
    for n in 0..4 {
        let at = format!("2026-10-02T11:{n:02}:00.000Z");
        more.push(message("user", &at, &format!("step {n}")));
        expected.push(format!("step {n}"));
    }
    append(&h.rollout, &more);
    // One batch of two per round: the rollout needs several rounds.
    h.importer.set_limits(Limits {
        bytes: 1,
        batch: 2,
        ..Limits::default()
    });
    let importer = Arc::new(std::mem::replace(
        &mut h.importer,
        SessionImporter::new(ImportPaths::from_home(&h.root)),
    ));
    let manual = SessionImportRequest {
        run: Some(ImportTrigger::Manual),
        ..SessionImportRequest::default()
    };
    assert!(
        importer
            .request(&h.engine, &scope(), &manual)
            .await
            .unwrap()
            .started
    );
    for _ in 0..400 {
        if !importer.is_running() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!importer.is_running());
    let status = importer
        .status(&h.engine, &ProfileOwner::from(&scope()))
        .await
        .unwrap();
    let codex = status.sources.iter().find(|s| s.source == CODEX).unwrap();
    assert_eq!(codex.counts.unwrap().deferred, 0, "{status:?}");
    // The request's result counts the thread its first round brought in.
    assert_eq!(codex.counts.unwrap().imported, 1, "{status:?}");
    assert_eq!(h.codex_history().await, expected);
}

// ── when a run is due ─────────────────────────────────────────────────

#[test]
fn launch_runs_once_and_background_waits_its_interval() {
    let both = [CLAUDE_CODE, CODEX];
    let mut state = SessionImportState::default();
    assert!(due(&state, ImportTrigger::Launch, 0, &both));
    assert!(due(&state, ImportTrigger::Background, 0, &both));
    state.last_run_at = Some(1_000);
    for id in both {
        state
            .last_counts
            .insert(id.to_string(), SessionImportCounts::default());
    }
    assert!(!due(&state, ImportTrigger::Launch, 2_000, &both));
    assert!(!due(&state, ImportTrigger::Background, 2_000, &both));
    assert!(due(
        &state,
        ImportTrigger::Background,
        1_000 + BACKGROUND_RESCAN_MIN_MS,
        &both
    ));
    assert!(due(&state, ImportTrigger::Manual, 2_000, &both));
    state.settings.background = false;
    assert!(!due(
        &state,
        ImportTrigger::Background,
        1_000 + BACKGROUND_RESCAN_MIN_MS,
        &both
    ));
}

#[test]
fn a_source_never_scanned_makes_the_launch_due_again() {
    let mut state = SessionImportState::default();
    state.last_run_at = Some(1_000);
    state
        .last_counts
        .insert(CLAUDE_CODE.to_string(), SessionImportCounts::default());
    assert!(!due(&state, ImportTrigger::Launch, 2_000, &[CLAUDE_CODE]));
    // Codex was found since: its sessions were never read.
    assert!(due(
        &state,
        ImportTrigger::Launch,
        2_000,
        &[CLAUDE_CODE, CODEX]
    ));
    assert!(due(
        &state,
        ImportTrigger::Background,
        2_000,
        &[CLAUDE_CODE, CODEX]
    ));
    // A first launch that found no CLI at all is not the launch import.
    let mut none = SessionImportState::default();
    none.last_run_at = Some(1_000);
    assert!(!due(&none, ImportTrigger::Launch, 2_000, &[]));
    assert!(due(&none, ImportTrigger::Launch, 2_000, &[CODEX]));
}

#[test]
fn work_left_deferred_is_picked_up_without_waiting_the_interval() {
    let mut state = SessionImportState::default();
    state.last_run_at = Some(1_000);
    state.last_counts.insert(
        CODEX.to_string(),
        SessionImportCounts {
            deferred: 3,
            ..SessionImportCounts::default()
        },
    );
    assert!(due(&state, ImportTrigger::Background, 2_000, &[CODEX]));
    assert!(!due(&state, ImportTrigger::Launch, 2_000, &[CODEX]));
    state.settings.background = false;
    assert!(!due(&state, ImportTrigger::Background, 2_000, &[CODEX]));
}

#[test]
fn work_deferred_by_a_source_that_cannot_run_now_does_not_keep_runs_due() {
    let mut state = SessionImportState::default();
    state.last_run_at = Some(1_000);
    for id in [CLAUDE_CODE, CODEX] {
        state
            .last_counts
            .insert(id.to_string(), SessionImportCounts::default());
    }
    state.last_counts.get_mut(CODEX).unwrap().deferred = 4;
    // Codex turned off (or gone): its left work is no reason to run.
    assert!(!due(
        &state,
        ImportTrigger::Background,
        2_000,
        &[CLAUDE_CODE]
    ));
    assert!(due(
        &state,
        ImportTrigger::Background,
        2_000,
        &[CLAUDE_CODE, CODEX]
    ));
}

#[test]
fn the_apps_own_homes_are_never_imported_from() {
    let home = Path::new("/Users/me");
    let roots = own_roots(
        home,
        [
            PathBuf::from("/Volumes/data/zlo"),
            PathBuf::from("/Users/me/.zlo"),
            PathBuf::from("relative"),
        ],
    );
    assert_eq!(
        roots,
        vec![
            home.join(".ironclaw"),
            home.join(".zlo"),
            PathBuf::from("/Volumes/data/zlo"),
        ]
    );
}

#[tokio::test]
async fn sessions_run_in_the_zlo_home_are_not_imported() {
    let mut h = harness().await;
    let zlo = h.root.join(".zlo/workspace");
    std::fs::create_dir_all(&zlo).unwrap();
    let dir = h
        .root
        .join(".claude/projects")
        .join(claude_codes::transcript::encode_project_dir(&zlo));
    let row = serde_json::json!({
        "type": "user", "sessionId": "z1", "cwd": zlo, "timestamp": "2026-10-01T10:00:00Z",
        "message": {"role": "user", "content": "inside the app's own home"},
    })
    .to_string();
    write(&dir.join("z1.jsonl"), &[row]);
    h.importer.paths.own_roots = own_roots(&h.root, []);
    let counts = h.run().await;
    assert_eq!(
        counts[CLAUDE_CODE].imported, 1,
        "only the app folder's session: {counts:?}"
    );
}
