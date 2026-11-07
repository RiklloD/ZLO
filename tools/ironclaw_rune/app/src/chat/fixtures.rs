//! Headless fixture scenes for the chat (PLAN.md rule 6): `chat`,
//! `chat-running`, `chat-gate`, `chat-question`, `chat-menu`. Each fills the
//! open chat with rows the way a timeline load would, without a gateway.

use std::rc::Rc;
use std::time::{Duration, Instant};

use ironclaw_gpui_client::GateDecision;
use ironclaw_gpui_client::commands::MenuSources;
use ironclaw_gpui_client::contract::ProductGateKind;
use ironclaw_gpui_client::product_wire::RebornProductCommandInfo;
use ironclaw_gpui_client::transcript::{AttachmentChip, GateState, QuestionState, Row, ToolStatus};
use ironclaw_host_api::agent_plan::{QuestionField, QuestionFieldKind, QuestionForm, QuestionOption};

use super::{ChatState, flow, question_form};
use crate::state::{Conn, Page, State};

const THREAD: &str = "t1";

fn user(key: &str, text: &str) -> Row {
    Row::User {
        key: key.into(),
        text: text.into(),
        pending: false,
        attachments: Vec::new(),
    }
}

fn reply(key: &str, text: &str, streaming: bool) -> Row {
    Row::Assistant {
        key: key.into(),
        text: text.into(),
        streaming,
    }
}

fn tool(key: &str, title: &str, detail: &str, status: ToolStatus) -> Row {
    Row::Tool {
        key: key.into(),
        title: title.into(),
        detail: Some(detail.into()),
        status,
        reason: None,
    }
}

/// The window with the thread list and `rows` open in a chat.
fn shell_with(s: &mut State, rows: Vec<Row>, live: bool) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Chat;
    s.workspace.threads = vec![
        (THREAD.into(), "Port the frontend to Rune".into()),
        ("t2".into(), "Fix the flaky integration test".into()),
    ];
    let mut chat = ChatState::new(THREAD.into());
    chat.channel = Some("gpui".into());
    if live {
        chat.fixture_run = Some("run-2".into());
        chat.run_started = Some(Instant::now() - Duration::from_secs(14));
    }
    chat.items = Rc::new(flow::fold(&rows, live, false, false, false));
    chat.outline.set(super::outline::prompts(&chat.items));
    chat.rows = Rc::new(rows);
    question_form::sync_drafts(&mut chat);
    s.chat = Some(chat);
}

fn history() -> Vec<Row> {
    vec![
        user("msg:1", "Port the chat transcript to Rune and keep the GPUI look."),
        Row::Thinking {
            key: "think:1".into(),
            text: "The transcript folds activity into one summary line with a tree under it.".into(),
        },
        tool("tool:1", "ironclaw.shell", "rg -n \"fn render_row\" tools/ironclaw_gpui", ToolStatus::Completed),
        tool("tool:2", "ironclaw.read_file", "tools/ironclaw_gpui/app/src/chat.rs", ToolStatus::Completed),
        tool("tool:3", "mcp.yab-browser.open", "https://github.com/anomalyco/rune", ToolStatus::Failed),
        reply(
            "msg:2",
            "Done. The transcript is a **virtualized growth stack** in a scroll viewport that follows its end:\n\n\
             - activity folds into one quiet line\n\
             - replies stream as Markdown\n\
             - gates and question forms are cards\n\n\
             ```rust\nfn view(s: &State, cx: &mut Cx<State>) -> El {\n    chat::view(s, cx, &k, &mut layer)\n}\n```",
            false,
        ),
        Row::System {
            key: "sys:1".into(),
            text: "Switched to claude-opus-5-5 · Claude CLI".into(),
        },
        Row::User {
            key: "msg:3".into(),
            text: "Compare these two {{attachment:0}} and summarize.".into(),
            pending: false,
            attachments: vec![
                AttachmentChip {
                    message_id: Some("m3".into()),
                    id: "a0".into(),
                    filename: "report.pdf".into(),
                    size: Some(48_200),
                    local_path: None,
                },
                AttachmentChip {
                    message_id: Some("m3".into()),
                    id: "a1".into(),
                    filename: "numbers.csv".into(),
                    size: Some(2_100),
                    local_path: None,
                },
            ],
        },
        reply(
            "msg:4",
            "Here is the summary.\n\n```visual\n{\"type\":\"stats\",\"title\":\"Q3\",\"items\":[{\"label\":\"Revenue\",\"value\":\"$1.2M\",\"delta\":\"+12%\"},{\"label\":\"Churn\",\"value\":\"2.1%\",\"delta\":\"-0.4%\",\"good\":true},{\"label\":\"Tickets\",\"value\":\"318\"}]}\n```\n\nThe report reads well.",
            false,
        ),
        Row::RunNotice {
            key: "notice:1".into(),
            run_id: "run-1".into(),
            text: "The model provider timed out after 120 s.".into(),
            failed: true,
            retryable: true,
        },
    ]
}

/// A finished conversation: activity, Markdown, a visual, a notice.
pub fn chat(s: &mut State) {
    shell_with(s, history(), false);
}

/// A turn in flight: the live activity group with a command's output, a
/// streaming reply and the working line; Stop beside Send.
pub fn running(s: &mut State) {
    let mut rows = history();
    rows.truncate(6);
    rows.push(user("msg:5", "Now run the tests and fix what fails."));
    rows.push(tool(
        "tool:9",
        "cli.bash",
        "cargo test -p ironclaw_rune",
        ToolStatus::Running,
    ));
    rows.push(Row::Thinking {
        key: "think:9".into(),
        text: "Two tests fail in pace.rs.".into(),
    });
    rows.push(reply("text:live", "Running the suite now. So far **two failures** in", true));
    shell_with(s, rows, true);
    if let Some(c) = s.chat.as_mut() {
        c.composer.replace_text("also check clippy");
    }
}

/// An approval waiting on the person, with its reason, and the reject field
/// open under it.
pub fn gate(s: &mut State) {
    let mut rows = history();
    rows.truncate(6);
    rows.push(user("msg:5", "Clean the build folder."));
    rows.push(Row::Gate {
        key: "gate:1".into(),
        run_id: "run-2".into(),
        gate_ref: "g1".into(),
        kind: ProductGateKind::Approval,
        headline: "Run a shell command".into(),
        body: Some("rm -rf target/ && cargo build --release".into()),
        allow_always: true,
        state: GateState::Open,
        reason: Some("The stale artifacts break the incremental build.".into()),
    });
    rows.push(Row::Gate {
        key: "gate:0".into(),
        run_id: "run-2".into(),
        gate_ref: "g0".into(),
        kind: ProductGateKind::Approval,
        headline: "Read ~/.cargo/config.toml".into(),
        body: None,
        allow_always: false,
        state: GateState::Resolving(GateDecision::Approve { always: false }),
        reason: None,
    });
    shell_with(s, rows, true);
}

/// A question form: a choice with Other, yes/no and a number.
pub fn question(s: &mut State) {
    let mut rows = history();
    rows.truncate(2);
    rows.push(Row::Question {
        key: "q:1".into(),
        run_id: "run-2".into(),
        gate_ref: "q1".into(),
        form: QuestionForm {
            source: Some("Claude".into()),
            message: Some("Before I set up the store, two choices:".into()),
            fields: vec![
                QuestionField {
                    id: "db".into(),
                    label: "Which database?".into(),
                    header: Some("Storage".into()),
                    description: Some("The local profile uses libSQL by default.".into()),
                    required: true,
                    kind: QuestionFieldKind::Choice {
                        options: vec![
                            QuestionOption::new("PostgreSQL"),
                            QuestionOption::new("libSQL"),
                            QuestionOption::new("Both"),
                        ],
                        multi: false,
                        allow_other: true,
                        min_items: None,
                        max_items: None,
                    },
                },
                QuestionField {
                    id: "seed".into(),
                    label: "Seed demo data?".into(),
                    header: None,
                    description: None,
                    required: false,
                    kind: QuestionFieldKind::Boolean {
                        default: Some(true),
                    },
                },
                QuestionField {
                    id: "n".into(),
                    label: "Connection pool size".into(),
                    header: None,
                    description: None,
                    required: false,
                    kind: QuestionFieldKind::Number {
                        integer: true,
                        minimum: Some(1.into()),
                        maximum: None,
                    },
                },
            ],
        },
        state: QuestionState::Open,
    });
    shell_with(s, rows, true);
}

/// The `/` menu open over the composer.
pub fn menu(s: &mut State) {
    let mut rows = history();
    rows.truncate(6);
    shell_with(s, rows, false);
    let command = |name: &str, description: &str| RebornProductCommandInfo {
        name: name.into(),
        title: name.into(),
        description: description.into(),
        usage: format!("/{name}"),
    };
    if let Some(c) = s.chat.as_mut() {
        c.menu.sources = Rc::new(MenuSources {
            commands: vec![
                command("new", "Start a new conversation"),
                command("status", "Show the thread's run and model"),
                command("compact", "Summarize the history to save context"),
            ],
            ..Default::default()
        });
        c.composer.replace_text("/");
    }
}
