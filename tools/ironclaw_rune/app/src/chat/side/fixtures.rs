//! Fixture states for the side pane's headless scenes (`scenes.rs`), and a
//! scene-only chat column that lays the docks out beside a stand-in
//! conversation until area A's `chat::view` does.

use std::path::PathBuf;
use std::process::Command;

use ironclaw_gpui_client::transcript::{GateState, Row, ToolStatus};
use rune::{Cx, El, col, row};
use rune_kit::Kit;

use super::ui::{self, SM};
use super::{SideChat, SideState, Tab, changes, console, git, history, outputs, project, routines};
use crate::state::{Conn, Page, State};

/// Fixture data the bridge's stand-ins read until areas A and H wire the
/// real chat (rows) and orchestration (tasks).
#[derive(Default)]
pub struct FixtureData {
    pub rows: Vec<Row>,
    pub tasks: Vec<ironclaw_gpui_client::orchestration::TaskView>,
}

thread_local! {
    static DATA: std::cell::RefCell<Option<&'static FixtureData>> = const { std::cell::RefCell::new(None) };
}

/// The scene's fixture rows and tasks, if a scene set them.
pub(super) fn data() -> Option<&'static FixtureData> {
    DATA.with(|d| *d.borrow())
}

fn set_data(data: FixtureData) {
    DATA.with(|d| *d.borrow_mut() = Some(Box::leak(Box::new(data))));
}

/// The scene's chat column: titlebar strip, stand-in conversation, docks.
// PORT-STUB(area A): scenes only; `chat::view` lays the docks out for real.
pub fn chat_view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let titlebar = row()
        .w_full()
        .h(crate::design::TITLEBAR_HEIGHT)
        .items_center()
        .child(ui::line("Port the frontend to Rune", SM, p.text_muted).pl(16.).grow(1.))
        .child(super::titlebar_actions(s, cx, k, layer));
    let conversation = col()
        .grow(1.)
        .min_w(0.)
        .h_full()
        .center()
        .child(ui::line("Conversation (area A)", SM, p.text_faint));
    col()
        .w_full()
        .h_full()
        .child(titlebar)
        .child(
            row()
                .w_full()
                .grow(1.)
                .min_h(0.)
                .child(conversation)
                .children(super::docks(s, cx, k, layer)),
        )
}

/// A scene shows the side pane fixture (and so the scene-only chat column).
pub fn active(s: &State) -> bool {
    s.fixture && s.chat.is_some()
}

/// The window with a thread open; `edit` fills its side state. Preferences
/// change in memory only (a scene must not save the prefs file).
fn base(s: &mut State, side: Option<Tab>, work: Option<Tab>, edit: impl FnOnce(&mut SideState)) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Chat;
    s.workspace.titles.remember("t1", "Port the frontend to Rune");
    s.workspace.titles.remember("t2", "Fix the flaky integration test");
    let mut chat = crate::chat::ChatState::new("t1".into());
    edit(&mut chat.side);
    s.chat = Some(chat);
    s.prefs.side_open = side.is_some();
    s.prefs.side_tab = side.map(|t| t.id().to_string()).unwrap_or_default();
    s.prefs.side_width = 0.;
    s.prefs.work_open = work.is_some();
    s.prefs.work_tab = work.map(|t| t.id().to_string()).unwrap_or_default();
    s.prefs.work_width = 0.;
    s.prefs.last_project = Some(PathBuf::from("/Users/you/code/zlo"));
}

/// A small repository with a merge, a modified and a new file, made once in
/// the temp folder so scenes read real `git` output.
fn repo() -> Option<PathBuf> {
    let tmp = std::env::temp_dir();
    let dir = std::fs::canonicalize(&tmp).unwrap_or(tmp).join("zlo-rune-side-fixture");
    if dir.join(".git").exists() {
        return Some(dir);
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).ok()?;
    let git = |args: &[&str]| {
        Command::new("git")
            .current_dir(&dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Ada")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(drop)
    };
    let write = |p: &str, text: &str| std::fs::write(dir.join(p), text).ok();
    git(&["init", "-q", "-b", "main"])?;
    write("README.md", "# Fixture\n\nA small project.\n")?;
    write("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n")?;
    write(".gitignore", "target/\n")?;
    git(&["add", "-A"])?;
    git(&["commit", "-q", "-m", "Start the project"])?;
    git(&["checkout", "-q", "-b", "feature"])?;
    write("src/lib.rs", "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n")?;
    git(&["add", "-A"])?;
    git(&["commit", "-q", "-m", "Add the library"])?;
    git(&["checkout", "-q", "main"])?;
    write("README.md", "# Fixture\n\nA small project, with docs.\n")?;
    git(&["commit", "-q", "-am", "Document the project"])?;
    git(&["merge", "-q", "--no-ff", "feature", "-m", "Merge the library"])?;
    write(
        "src/main.rs",
        "fn main() {\n    let sum = fixture::add(2, 3);\n    println!(\"hello, {sum}\");\n}\n",
    )?;
    write("notes.md", "Ideas for the next turn.\n")?;
    std::fs::create_dir_all(dir.join("target")).ok()?;
    write("target/build.log", "ignored\n")?;
    Some(dir)
}

fn with_repo(x: &mut SideState) {
    if let Some(dir) = repo() {
        x.repo = git::root(&dir).ok();
        x.folder = Some(dir);
    }
}

const PATCH: &str = "diff --git a/src/main.rs b/src/main.rs
index 3b18e51..a1b2c3d 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,4 @@
 fn main() {
-    println!(\"hello\");
+    let sum = fixture::add(2, 3);
+    println!(\"hello, {sum}\");
 }
diff --git a/notes.md b/notes.md
new file mode 100644
index 0000000..5e1c309
--- /dev/null
+++ b/notes.md
@@ -0,0 +1 @@
+Ideas for the next turn.
";

pub fn changes(s: &mut State) {
    base(s, None, Some(Tab::Changes), |x| {
        with_repo(x);
        let changes = vec![
            git::Change {
                path: "src/main.rs".into(),
                from: None,
                index: ' ',
                worktree: 'M',
            },
            git::Change {
                path: "notes.md".into(),
                from: None,
                index: '?',
                worktree: '?',
            },
        ];
        changes::fixture(x, "main", changes, PATCH, Some(PATCH));
        changes::fixture_comment(x, 2, "Name this `total`; `sum` shadows the iterator method.");
    });
}

pub fn files(s: &mut State) {
    base(s, None, Some(Tab::Files), |x| {
        with_repo(x);
        project::fixture_load(x, &["src"]);
    });
}

pub fn history(s: &mut State) {
    base(s, Some(Tab::History), None, |x| {
        with_repo(x);
        history::fixture_load(x);
    });
}

pub fn no_folder(s: &mut State) {
    base(s, Some(Tab::Console), Some(Tab::Files), |_| {});
}

pub fn console(s: &mut State) {
    base(s, Some(Tab::Console), None, |x| {
        x.folder = Some(PathBuf::from("/Users/you/code/zlo"));
        x.console.lines = vec![
            (console::Kind::Command, "git log --oneline -3".into()),
            (console::Kind::Out, "7ac5538 ZLO on Rune: foundation".into()),
            (console::Kind::Out, "89a6065 fix(claude): identify headless runs".into()),
            (console::Kind::Out, "18c3b01 Voice: push-to-talk on by default".into()),
            (console::Kind::Info, "Done in 0.1 s".into()),
            (console::Kind::Command, "cargo test -q".into()),
            (console::Kind::Err, "error[E0425]: cannot find value `sum` in this scope".into()),
            (console::Kind::Info, "Exited with 101 after 4.2 s".into()),
        ];
    });
}

fn gate(key: &str, headline: &str, state: GateState) -> Row {
    Row::Gate {
        key: key.into(),
        run_id: "run-1".into(),
        gate_ref: key.into(),
        kind: ironclaw_gpui_client::contract::ProductGateKind::Approval,
        headline: headline.into(),
        body: Some("rm -rf target/ && cargo build --release".into()),
        allow_always: true,
        state,
        reason: None,
    }
}

fn tool(key: &str, title: &str, status: ToolStatus, detail: Option<&str>) -> Row {
    Row::Tool {
        key: key.into(),
        title: title.into(),
        detail: detail.map(str::to_string),
        status,
        reason: None,
    }
}

fn user(key: &str, text: &str) -> Row {
    Row::User {
        key: key.into(),
        text: text.into(),
        pending: false,
        attachments: Vec::new(),
    }
}

pub fn activity(s: &mut State) {
    set_data(FixtureData {
        rows: vec![
            user("u1", "Find why the integration test is flaky"),
            tool("t1", "Read tests/integration/turns.rs", ToolStatus::Completed, None),
            tool("t2", "Run cargo test -p ironclaw_turns", ToolStatus::Failed, Some("exit 101")),
            Row::RunNotice {
                key: "n1".into(),
                run_id: "run-0".into(),
                text: "Retried after a provider timeout".into(),
                failed: false,
                retryable: false,
            },
            user("u2", "Clean the build and rerun it"),
            tool("t3", "Search for `wait_for_status`", ToolStatus::Completed, None),
            gate("g1", "Run a shell command", GateState::Open),
            tool("t4", "Write src/fix.rs", ToolStatus::Running, None),
        ],
        tasks: Vec::new(),
    });
    base(s, Some(Tab::Activity), None, |x| {
        x.activity_open("t2");
    });
}

pub fn agents(s: &mut State) {
    let task = |json: serde_json::Value| serde_json::from_value(json).ok();
    let now = chrono::Utc::now().timestamp_millis();
    set_data(FixtureData {
        rows: vec![user("u1", "Split the port into areas")],
        tasks: [
            task(serde_json::json!({
                "taskId": "a1", "title": "Port the side pane", "status": "running",
                "provider": "anthropic", "model": "claude-opus-5-5", "childThreadId": "c1",
                "createdAt": now - 420_000, "startedAt": now - 400_000
            })),
            task(serde_json::json!({
                "taskId": "a2", "title": "Port the settings pages", "status": "completed",
                "model": "claude-sonnet-5-5", "createdAt": now - 900_000, "startedAt": now - 880_000,
                "finishedAt": now - 300_000, "summary": "All twelve pages ported; scenes render."
            })),
            task(serde_json::json!({
                "taskId": "a3", "title": "Port voice", "status": "failed",
                "createdAt": now - 1_200_000, "error": "Usage limit reached before the first turn."
            })),
        ]
        .into_iter()
        .flatten()
        .collect(),
    });
    base(s, Some(Tab::Agents), None, |_| {});
}

pub fn routines(s: &mut State) {
    let now = chrono::Utc::now();
    let schedule: Option<ironclaw_gpui_client::schedules::ScheduleInfo> = serde_json::from_value(serde_json::json!({
        "id": "s1", "title": "Morning inbox digest", "prompt": "Summarize my inbox",
        "schedule": {"type": "cron", "cron": "0 8 * * *"}, "describe": "Every day at 08:00",
        "enabled": true, "threadId": "t1",
        "nextRunAt": (now + chrono::Duration::minutes(95)).timestamp_millis(),
        "lastRunAt": (now - chrono::Duration::hours(22)).timestamp_millis()
    }))
    .map_err(|e| tracing::warn!(error = %e, "fixture schedule"))
    .ok();
    let automation: Option<ironclaw_gpui_client::product_wire::RebornAutomationInfo> =
        serde_json::from_value(serde_json::json!({
            "automation_id": "r1", "name": "Weekly dependency audit",
            "source": {"type": "schedule", "cron": "0 9 * * 1", "timezone": "Europe/Berlin"},
            "state": "paused",
            "recent_runs": [{"status": "error", "submitted_at": (now - chrono::Duration::days(3)).to_rfc3339(), "thread_id": "t2"}]
        }))
        .map_err(|e| tracing::warn!(error = %e, "fixture automation"))
        .ok();
    base(s, Some(Tab::Routines), None, |x| {
        routines::fixture(x, schedule.into_iter().collect(), automation.into_iter().collect());
    });
}

pub fn outputs(s: &mut State) {
    use ironclaw_gpui_client::workspace_views::{ProjectFsEntry, ProjectFsEntryKind};
    let entry = |name: &str, kind| ProjectFsEntry {
        name: name.into(),
        path: format!("/workspace/{name}"),
        kind,
    };
    base(s, Some(Tab::Browser), Some(Tab::Outputs), |x| {
        outputs::fixture(
            x,
            vec![
                entry("reports", ProjectFsEntryKind::Directory),
                entry("chart.png", ProjectFsEntryKind::File),
                entry("summary.md", ProjectFsEntryKind::File),
                entry("ten_words.pdf", ProjectFsEntryKind::File),
            ],
        );
    });
}

pub fn session(s: &mut State) {
    set_data(FixtureData {
        rows: vec![user("u1", "Port the side pane")],
        tasks: Vec::new(),
    });
    s.prefs.side_chats.insert("t1".into(), vec!["sc1".into(), "sc2".into()]);
    base(s, Some(Tab::SideChat), None, |x| {
        x.folder = Some(PathBuf::from("/Users/you/code/zlo"));
        x.chat = Some(SideChat {
            thread_id: "sc2".into(),
        });
        x.session_open = true;
    });
}

impl SideState {
    fn activity_open(&mut self, key: &str) {
        self.activity.open_step(key);
    }
}
