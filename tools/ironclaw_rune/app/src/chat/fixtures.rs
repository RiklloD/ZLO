//! Headless fixture scenes for the chat (PLAN.md rule 6): `chat`,
//! `chat-running`, `chat-gate`, `chat-question`, `chat-menu`,
//! `chat-long-draft`, `chat-context-banner`. Each fills the
//! open chat with rows the way a timeline load would, without a gateway.

use std::rc::Rc;
use std::time::{Duration, Instant};

use ironclaw_host_api::agent_plan::{
    QuestionField, QuestionFieldKind, QuestionForm, QuestionOption,
};
use zlo_client::GateDecision;
use zlo_client::commands::MenuSources;
use zlo_client::contract::ProductGateKind;
use zlo_client::product_wire::RebornProductCommandInfo;
use zlo_client::transcript::{AttachmentChip, GateState, QuestionState, Row, ToolStatus};

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
    s.workspace
        .titles
        .remember(THREAD, "Port the frontend to Rune");
    s.workspace
        .titles
        .remember("t2", "Fix the flaky integration test");
    let mut chat = ChatState::new(THREAD.into());
    chat.channel = Some("gpui".into());
    fill(&mut chat, rows, live);
    s.chat = Some(chat);
}

/// Fold the open chat's rows again after a scene filled the extras (area
/// H), so the transcript gains its tail row as a live chat would.
pub fn refold(s: &mut State) {
    let Some(id) = s.chat.as_ref().map(|c| c.thread_id.clone()) else {
        return;
    };
    refold_chat(s, &id);
}

/// [`refold`] for any chat, the open one or a side chat.
pub fn refold_chat(s: &mut State, id: &str) {
    let has_tail = super::orchestration::has_tail(s, id);
    let Some(c) = super::chat_for(s, id) else {
        return;
    };
    let live = c.fixture_run.is_some();
    c.has_tail = has_tail;
    c.items = Rc::new(flow::fold(&c.rows, live, false, has_tail, false));
}

/// Put `rows` into `chat` the way a timeline load folds them; `live` adds a
/// running turn (14 s in). Other areas' scenes use this for their chats.
pub fn fill(chat: &mut ChatState, rows: Vec<Row>, live: bool) {
    if live {
        chat.fixture_run = Some("run-2".into());
        chat.run_started = Some(Instant::now() - Duration::from_secs(14));
    }
    chat.items = Rc::new(flow::fold(&rows, live, false, false, false));
    chat.outline.set(super::outline::prompts(&chat.items));
    chat.rows = Rc::new(rows);
    question_form::sync_drafts(chat);
}

fn history() -> Vec<Row> {
    vec![
        user(
            "msg:1",
            "Port the chat transcript to Rune and keep the GPUI look.",
        ),
        Row::Thinking {
            key: "think:1".into(),
            text: "The transcript folds activity into one summary line with a tree under it."
                .into(),
        },
        tool(
            "tool:1",
            "ironclaw.shell",
            "rg -n \"fn render_row\" tools/ironclaw_gpui",
            ToolStatus::Completed,
        ),
        tool(
            "tool:2",
            "ironclaw.read_file",
            "tools/ironclaw_gpui/app/src/chat.rs",
            ToolStatus::Completed,
        ),
        tool(
            "tool:3",
            "mcp.yab-browser.open",
            "https://github.com/anomalyco/rune",
            ToolStatus::Failed,
        ),
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
    rows.push(reply(
        "text:live",
        "Running the suite now. So far **two failures** in",
        true,
    ));
    shell_with(s, rows, true);
    if let Some(c) = s.chat.as_mut() {
        c.composer.replace_text("also check clippy");
    }
}

/// A very long finished conversation (250 turns, 500 messages, Markdown
/// replies): the virtual transcript's cost check.
pub fn long(s: &mut State) {
    let mut rows = Vec::new();
    for i in 0..250 {
        rows.push(user(
            &format!("msg:u{i}"),
            &format!("Question {i}: how does the transcript keep its place while it grows?"),
        ));
        rows.push(reply(
            &format!("msg:a{i}"),
            &format!(
                "Answer {i}. The list is a **growth stack** in a scroll viewport:\n\n\
                 - only the rows near the viewport are built\n\
                 - finished rows are kept by `memo`\n\n\
                 It follows its end while a reply streams, and `scroll_into_view` \
                 still reaches row {i} from anywhere."
            ),
            false,
        ));
    }
    shell_with(s, rows, false);
}

/// The finished conversation with a long unsent message in the composer
/// (about 300 characters, several lines): sidebar and dock motion checks.
pub fn long_draft(s: &mut State) {
    chat(s);
    if let Some(c) = s.chat.as_mut() {
        c.composer.replace_text(
            "Before we merge, walk the whole sidebar animation again at the narrow \
             window widths: open and close it with an empty composer and with this \
             long message in it, and check that the transcript, the composer and the \
             sidebar all move on the same curve, with nothing drifting up or down \
             after the slide has ended.",
        );
    }
}

/// The finished conversation under the context banner (the window at 85 %
/// of its context): the banner's own checks.
pub fn context_banner(s: &mut State) {
    chat(s);
    s.workspace.telemetry = Some(zlo_client::usage::ThreadTelemetry {
        context_used: Some(171_000),
        context_max: Some(200_000),
        ..Default::default()
    });
}

/// The right pane open under a full-strength wallpaper laid over `area`
/// ("conversation" or "pane"): each covers only its own region.
pub fn wallpaper_area(s: &mut State, area: &str) {
    super::side::fixtures::history(s);
    s.prefs.wallpaper = crate::design::prefs::Wallpaper::Scene {
        name: "night".into(),
    };
    s.prefs.wallpaper_effect = "none".into();
    s.prefs.wallpaper_area = area.into();
    s.prefs.wallpaper_opacity = 0.6;
    crate::design::look::fixture_wallpaper(s, (800, 600));
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

/// A bare state for unit tests: default preferences, no gateway, and a
/// handle to an app that never runs (async replies queue and go nowhere;
/// tests call the reply handlers themselves).
#[cfg(test)]
pub(crate) fn test_state() -> State {
    let spec =
        rune::AppSpec::<State>::new("test", (100., 100.), crate::placeholder_init, crate::view);
    let handle = rune::App::new(spec).handle();
    let mut s = State::new(crate::jobs::Rt::new(handle));
    s.prefs = Default::default();
    s
}

/// The open chat `t1` with side chat `side_id` in its right pane.
#[cfg(test)]
pub(crate) fn with_side_chat(s: &mut State, side_id: &str) {
    s.page = Page::Chat;
    let mut main = ChatState::new(THREAD.into());
    let mut side = ChatState::new(side_id.into());
    side.embedded = true;
    side.composer.focused = false;
    main.side.chat = Some(super::side::SideChat {
        thread_id: side_id.into(),
        state: Box::new(side),
    });
    s.chat = Some(main);
}

/// Close the open chat's side chat, as its close button does.
#[cfg(test)]
pub(crate) fn close_side_chat(s: &mut State) {
    if let Some(c) = s.chat.as_mut() {
        c.side.chat = None;
    }
}
