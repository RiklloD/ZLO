//! Headless fixture scenes for the chat: `chat`,
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
use zlo_client::transcript::{
    AttachmentChip, GateState, QuestionState, Row, SteerMark, ToolStatus,
};

use super::{ChatState, flow, question_form};
use crate::state::{Conn, Page, State};

const THREAD: &str = "t1";

fn user(key: &str, text: &str) -> Row {
    Row::User {
        key: key.into(),
        text: text.into(),
        pending: false,
        attachments: Vec::new(),
        steer: None,
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
    let ctx = flow::Folding {
        spans: &c.spans,
        toggles: &c.toggles,
        prompt_shown: true,
    };
    c.items = Rc::new(flow::fold(&c.rows, live, false, has_tail, false, &ctx));
}

/// Put `rows` into `chat` the way a timeline load folds them; `live` adds a
/// running turn (14 s in). Other areas' scenes use this for their chats.
pub fn fill(chat: &mut ChatState, rows: Vec<Row>, live: bool) {
    if live {
        chat.fixture_run = Some("run-2".into());
        chat.run_started = Some(Instant::now() - Duration::from_secs(14));
    }
    // Every settled turn of a scene worked 2m 14s.
    chat.spans = Rc::new(
        rows.iter()
            .filter_map(|r| match r {
                Row::User {
                    key, steer: None, ..
                } => Some((key.clone(), 134_000)),
                _ => None,
            })
            .collect(),
    );
    let ctx = flow::Folding {
        spans: &chat.spans,
        toggles: &chat.toggles,
        prompt_shown: true,
    };
    chat.items = Rc::new(flow::fold(&rows, live, false, false, false, &ctx));
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
            steer: None,
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

/// A long CLI turn, t3code's timeline: the agent's words between its tool
/// groups. `live` keeps the run going (a command still running at the
/// end); settled, the turn folds under "Worked for" over the answer, and
/// `open` opens that fold.
fn cli_turn(live: bool) -> Vec<Row> {
    let say = |key: &str, text: &str| Row::Narration {
        key: key.into(),
        text: text.into(),
    };
    let edit = |key: &str, path: &str| tool(key, "cli.write", path, ToolStatus::Completed);
    let mut rows = vec![
        user("msg:c1", "Build the web2 auto-battler from the roadmap."),
        Row::Thinking {
            key: "think:c1".into(),
            text: "Start from the decision record, then the engine.".into(),
        },
        tool("t:c1", "cli.bash", "ls -la", ToolStatus::Completed),
        tool(
            "t:c2",
            "cli.read",
            "chroniques-du-rift-roadmap.md",
            ToolStatus::Completed,
        ),
        tool("t:c3", "cli.bash", "bun --version", ToolStatus::Failed),
        say(
            "say:c1",
            "First, the decision record. It's the contract that everything else follows.",
        ),
        edit("t:c4", "docs/ARCHITECTURE.md"),
        say("say:c2", "Now the engine contract: types, RNG and balance."),
        edit("t:c5", "engine/src/types.ts"),
        edit("t:c6", "engine/src/rng.ts"),
        edit("t:c7", "engine/src/balance.ts"),
        say("say:c3", "Engine tests:"),
        tool("t:c8", "cli.bash", "bun test engine", ToolStatus::Completed),
        say(
            "say:c4",
            "All 10 tests pass on the first run. Next I'll write the soak test, which doubles as a balance probe.",
        ),
        edit("t:c9", "engine/test/soak.ts"),
    ];
    if live {
        rows.push(tool(
            "t:c10",
            "cli.bash",
            "bun run soak --matches 500",
            ToolStatus::Running,
        ));
    } else {
        rows.push(tool(
            "t:c10",
            "cli.bash",
            "bun run soak --matches 500",
            ToolStatus::Completed,
        ));
        rows.push(reply(
            "msg:c2",
            "Chroniques du Rift is playable in the browser now: guest accounts with just a name, solo practice against bots, private lobbies and a public 2v2 queue.\n\n\
             **What I tested**\n\n\
             - Engine tests: 10/10 pass, and replays come out identical.\n\
             - Headless bot matches: 0 crashes across 500.",
            false,
        ));
    }
    rows
}

/// [`cli_turn`] settled: one "Worked for" line over the answer.
pub fn cli_turn_settled(s: &mut State) {
    shell_with(s, cli_turn(false), false);
}

/// [`cli_turn`] settled, its fold opened.
pub fn cli_turn_open(s: &mut State) {
    shell_with(s, cli_turn(false), false);
    if let Some(c) = s.chat.as_mut() {
        c.toggles.insert("fold-msg:c1".into(), true);
    }
    refold(s);
}

/// [`cli_turn`] still running.
pub fn cli_turn_live(s: &mut State) {
    shell_with(s, cli_turn(true), true);
}

/// Mid-turn steering (t3code's): a finished run with a steer, the work
/// after it folded under "Continued for", an unconfirmed steer notice, and
/// a running turn with a steer still waiting to be read; four queued
/// messages above the composer (two shown and "Show all").
pub fn steering(s: &mut State) {
    let steer =
        |key: &str, text: &str, waiting: bool, continues: bool, ms: Option<i64>| Row::User {
            key: key.into(),
            text: text.into(),
            pending: false,
            attachments: Vec::new(),
            steer: Some(SteerMark {
                waiting,
                continues,
                continued_ms: ms,
            }),
        };
    let rows = vec![
        user("msg:1", "Port the queue panel to Rune."),
        tool("tool:1", "cli.read", "app/src/chat/orchestration/view.rs", ToolStatus::Completed),
        steer("msg:2", "Match t3code's dashed bubbles.", false, false, Some(185_000)),
        steer("msg:3", "And keep the flat look.", false, true, Some(170_000)),
        tool("tool:2", "cli.edit", "app/src/chat/orchestration/view.rs", ToolStatus::Completed),
        tool("tool:3", "cli.bash", "cargo test -p ironclaw_rune", ToolStatus::Completed),
        reply("msg:4", "Done: the queue sits above the composer now.", false),
        Row::SteerNotice {
            key: "steer-notice:s9".into(),
            title: "Steer delivery not confirmed".into(),
            text: "The agent did not confirm reading your steer before Stop. It may not have been delivered. Resend it if needed: \u{201c}Also update the docs.\u{201d}".into(),
            words: "Also update the docs.".into(),
        },
        user("msg:5", "Now run clippy and fix the warnings."),
        tool("tool:9", "cli.bash", "cargo clippy --all-targets", ToolStatus::Running),
        steer("msg:6", "Skip the bench targets.", true, false, None),
    ];
    shell_with(s, rows, true);
    if let Some(c) = s.chat.as_mut() {
        c.composer.replace_text("then commit");
        let items: Vec<_> = [
            ("q1", "Then run the headless scenes.", "recovered_steer"),
            ("q2", "Compare the PNGs with main.", "message"),
            ("q3", "Commit on the branch.", "message"),
            ("q4", "Write the report.", "message"),
        ]
        .iter()
        .map(|(id, text, kind)| serde_json::json!({"id": id, "text": text, "kind": kind}))
        .collect();
        if let Ok(queue) = serde_json::from_value(serde_json::json!({ "items": items })) {
            c.orchestration.queue = queue;
        }
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

/// The wallpaper over the whole window, as most people set it, with both
/// docks open (Work on History, Side on Activity): the docks get the
/// lighter share of the picture.
pub fn wallpaper_window(s: &mut State) {
    super::side::fixtures::history(s);
    s.prefs.side_open = true;
    s.prefs.side_tab = "activity".into();
    s.prefs.wallpaper = crate::design::prefs::Wallpaper::Scene {
        name: "dusk".into(),
    };
    s.prefs.wallpaper_effect = "none".into();
    s.prefs.wallpaper_area = "window".into();
    s.prefs.wallpaper_opacity = 0.35;
    crate::design::look::fixture_wallpaper(s, (1280, 820));
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
