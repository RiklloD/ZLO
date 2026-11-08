//! Headless fixture states for the workspace's scenes (`scenes.rs`): an
//! inbox with every shelf, subagents, a context-heavy thread, the ⌘K menu,
//! the new-thread screen and the settings nav. No gateway is involved.

use chrono::Utc;
use zlo_client::history::{
    HistoryHighlight, HistoryHit, HistorySearchRole, HistoryThreadOrigin, HistoryThreadResult,
};
use zlo_client::inbox::{
    InboxEngine, InboxListResponse, InboxProfile, InboxRow, InboxState, InboxStatus,
};
use zlo_client::usage::{ThreadContextReport, ThreadTelemetry};
use rune::TextEdit;

use crate::state::{Conn, Page, State};

pub(crate) fn row(
    id: &str,
    title: &str,
    profile: &str,
    state: InboxState,
    minutes_ago: i64,
) -> InboxRow {
    InboxRow {
        thread_id: id.into(),
        title: Some(title.into()),
        profile_id: profile.into(),
        state,
        status: None,
        engines: vec![InboxEngine {
            runtime_id: "ironclaw".into(),
            provider: Some("anthropic".into()),
            model: Some("claude-opus-5-5".into()),
        }],
        last_activity_at: Utc::now().timestamp_millis() - minutes_ago * 60_000,
        snooze_until: None,
        settled_at: None,
        limit_reset_at: None,
        working_since: None,
        parent_thread_id: None,
        automation: false,
        worktree_branch: None,
        title_pending: false,
        imported: None,
    }
}

fn base(s: &mut State) {
    s.fixture = true;
    s.conn = Conn::Ready;
    s.page = Page::Chat;
    s.workspace.timers = Default::default();
    // Scenes start from the person's prefs file; show the shell's parts.
    s.prefs.status_bar_hidden = false;
    super::set_sidebar_open(s, true);
}

fn inbox() -> InboxListResponse {
    let now = Utc::now().timestamp_millis();
    let mut working = row(
        "t-work",
        "Run the integration suite",
        "work",
        InboxState::Working,
        2,
    );
    working.status = Some(InboxStatus::Working);
    working.working_since = Some(now - 153_000);
    let mut needs = row(
        "t-needs",
        "Approve the deploy plan",
        "default",
        InboxState::Inbox,
        4,
    );
    needs.status = Some(InboxStatus::NeedsInput);
    let mut done = row(
        "t-done",
        "Summarise yesterday's PRs",
        "default",
        InboxState::Inbox,
        35,
    );
    done.status = Some(InboxStatus::Done);
    let mut pinned = row(
        "t-pin",
        "Port the frontend to Rune",
        "work",
        InboxState::Pinned,
        90,
    );
    pinned.worktree_branch = Some("feature/rune/workspace-port".into());
    let mut snoozed = row(
        "t-snooze",
        "Reply to the design review",
        "default",
        InboxState::Snoozed,
        300,
    );
    snoozed.snooze_until = Some(now + 3 * 3_600_000);
    let mut settled = row(
        "t-settled",
        "Fix the flaky integration test",
        "work",
        InboxState::Settled,
        2000,
    );
    settled.settled_at = Some(now - 86_400_000);
    InboxListResponse {
        rows: vec![
            pinned,
            needs,
            done,
            row(
                "t-plain",
                "Draft the release notes",
                "default",
                InboxState::Inbox,
                180,
            ),
            working,
            snoozed,
            settled,
        ],
        profiles: vec![
            InboxProfile {
                profile_id: "default".into(),
                display_name: "Default".into(),
                git_branch: None,
                last_used_at: None,
            },
            InboxProfile {
                profile_id: "work".into(),
                display_name: "IronClaw".into(),
                git_branch: Some("main".into()),
                last_used_at: None,
            },
        ],
        last_used_profile_id: Some("work".into()),
    }
}

fn open(s: &mut State, id: &str) {
    s.chat = Some(crate::chat::ChatState::new(id.into()));
}

/// A first run: no threads, the new-thread screen (setup's backdrop).
pub fn fixture_empty(s: &mut State) {
    base(s);
    s.chat = None;
    s.page = Page::Draft;
    super::draft::open(s);
}

/// The window with the inbox and a thread open.
pub fn fixture_shell(s: &mut State) {
    base(s);
    s.workspace.inbox.list = inbox();
    s.workspace.inbox.show_snoozed = true;
    s.workspace.inbox.show_settled = true;
    open(s, "t-pin");
    s.workspace.telemetry = Some(ThreadTelemetry {
        context_used: Some(51_200),
        context_max: Some(200_000),
        tps: Some(84.),
        cache_hit_rate: Some(0.91),
        ..ThreadTelemetry::default()
    });
}

/// Every inbox shelf unfolded, ⌘ held (jump badges), one snooze menu open
/// and the filters panel.
pub fn fixture_inbox(s: &mut State) {
    fixture_shell(s);
    s.workspace.inbox.cmd_held = true;
    s.workspace.inbox.filters_open = true;
    s.workspace.inbox.snooze_menu = Some("t-plain".into());
    s.workspace.inbox.unread.insert("t-plain".into());
    s.workspace
        .drafts_fixture("Sketch the onboarding flow", Some("work"));
}

/// A long history: 20 inbox cards, 30 snoozed and 950 settled threads,
/// every settled one shown (Show more pressed until the end).
pub fn fixture_inbox_long(s: &mut State) {
    fixture_shell(s);
    let now = Utc::now().timestamp_millis();
    let profiles = ["default", "work"];
    let mut rows = Vec::new();
    for i in 0..1000usize {
        let (state, ago) = match i {
            0..20 => (InboxState::Inbox, 5 + i as i64 * 7),
            20..50 => (InboxState::Snoozed, 300 + i as i64),
            _ => (InboxState::Settled, 2000 + i as i64 * 30),
        };
        let mut r = row(
            &format!("t-long-{i}"),
            &format!(
                "Thread {i}: tidy the {} module",
                ["chat", "inbox", "shell", "picker"][i % 4]
            ),
            profiles[i % 2],
            state,
            ago,
        );
        match state {
            InboxState::Snoozed => r.snooze_until = Some(now + 3_600_000),
            InboxState::Settled => r.settled_at = Some(r.last_activity_at),
            _ => {}
        }
        rows.push(r);
    }
    s.workspace.inbox.list.rows = rows;
    s.workspace.inbox.settled_limit = 1000;
}

/// A short inbox over a folded Settled shelf: the shelf sits at the
/// sidebar's bottom, as in GPUI, not under the last card.
pub fn fixture_inbox_short(s: &mut State) {
    fixture_shell(s);
    let mut rows: Vec<_> = [
        "Port the sidebar",
        "Review the deploy plan",
        "Draft the notes",
    ]
    .into_iter()
    .enumerate()
    .map(|(i, title)| {
        row(
            &format!("t-short-{i}"),
            title,
            "default",
            InboxState::Inbox,
            5 + i as i64,
        )
    })
    .collect();
    for i in 0..12usize {
        let mut r = row(
            &format!("t-short-settled-{i}"),
            &format!("Settled thread {i}"),
            "default",
            InboxState::Settled,
            900 + i as i64,
        );
        r.settled_at = Some(r.last_activity_at);
        rows.push(r);
    }
    s.workspace.inbox.list.rows = rows;
    s.workspace.inbox.show_settled = false;
}

/// A parent with nested subagents, its list unfolded.
pub fn fixture_inbox_subagents(s: &mut State) {
    fixture_shell(s);
    let mut parent = row(
        "t-parent",
        "Ship the workspace port",
        "work",
        InboxState::Inbox,
        1,
    );
    parent.status = Some(InboxStatus::Background);
    let kid = |id: &str, title: &str, status: Option<InboxStatus>, parent: &str| {
        let mut r = row(id, title, "work", InboxState::Inbox, 1);
        r.status = status;
        r.parent_thread_id = Some(parent.into());
        r
    };
    let mut rows = vec![
        parent,
        kid(
            "t-a",
            "Port inbox.rs",
            Some(InboxStatus::Working),
            "t-parent",
        ),
        kid(
            "t-b",
            "Port command.rs",
            Some(InboxStatus::NeedsInput),
            "t-parent",
        ),
        kid(
            "t-a1",
            "Write the subagent tests",
            Some(InboxStatus::Failed),
            "t-a",
        ),
        kid(
            "t-c",
            "Port context.rs",
            Some(InboxStatus::Done),
            "t-parent",
        ),
    ];
    rows.append(&mut s.workspace.inbox.list.rows);
    s.workspace.inbox.list.rows = rows;
    s.workspace.inbox.expanded.insert("t-parent".into());
    s.workspace.inbox.expanded.insert("t-a".into());
    open(s, "t-b");
}

/// The ⌘K menu over the shell, with a query and message hits.
pub fn fixture_command_menu(s: &mut State) {
    fixture_shell(s);
    s.workspace.command = Some(super::command::CommandMenu {
        input: TextEdit::new("port"),
        selected: 1,
        opened: 1,
    });
    s.workspace.message_search.fixture(
        "port",
        vec![HistoryThreadResult {
            thread_id: "t-old".into(),
            title: Some("Rune evaluation".into()),
            updated_at: None,
            origin: HistoryThreadOrigin::Own,
            lineage_root_thread_id: None,
            related_thread_ids: Vec::new(),
            hits: vec![HistoryHit {
                thread_id: "t-old".into(),
                message_id: "m1".into(),
                sequence: 3,
                role: HistorySearchRole::User,
                created_at: None,
                snippet: "…we should port the sidebar first, then the status bar…".into(),
                highlights: vec![HistoryHighlight { start: 13, end: 17 }],
            }],
        }],
    );
}

/// The new-thread screen with a draft and a staged file.
pub fn fixture_draft(s: &mut State) {
    fixture_shell(s);
    s.chat = None;
    s.workspace
        .drafts_fixture("Plan the Rune port of the settings pages", Some("work"));
    super::draft::open(s);
    let caret = s.workspace.draft.as_ref().map_or(0, |d| d.input.text.len());
    if let Some(d) = s.workspace.draft.as_mut() {
        d.input.set_caret(caret);
        d.input.insert(" using ");
    }
    super::draft::place(s, vec![fixture_file(1, "PLAN.md")]);
}

fn fixture_file(key: u64, name: &str) -> zlo_client::attachments::StagedFile {
    zlo_client::attachments::StagedFile {
        key,
        filename: name.into(),
        mime_type: "text/markdown".into(),
        size: 4_210,
        body: zlo_client::attachments::Body::Bytes(Vec::new()),
    }
}

/// The new-thread screen with two inline file cards between the words.
pub fn fixture_draft_files(s: &mut State) {
    fixture_shell(s);
    s.chat = None;
    s.workspace.drafts_fixture("Compare ", Some("work"));
    super::draft::open(s);
    let end = |s: &mut State| {
        if let Some(d) = s.workspace.draft.as_mut() {
            let len = d.input.text.len();
            d.input.set_caret(len);
        }
    };
    end(s);
    super::draft::place(s, vec![fixture_file(1, "PLAN.md")]);
    if let Some(d) = s.workspace.draft.as_mut() {
        d.input.insert("with ");
    }
    super::draft::place(s, vec![fixture_file(2, "PARITY_REVIEW.md")]);
    if let Some(d) = s.workspace.draft.as_mut() {
        d.input.insert("and list what is left.");
    }
}

/// The new-thread screen's `@` menu on the last project folder's files.
pub fn fixture_draft_mention(s: &mut State) {
    fixture_shell(s);
    s.chat = None;
    s.workspace.drafts_fixture("Look at @men", Some("work"));
    super::draft::open(s);
    let root = std::path::PathBuf::from("/Users/zlo/src/ironclaw");
    if let Some(d) = s.workspace.draft.as_mut() {
        let len = d.input.text.len();
        d.input.set_caret(len);
        let files = [
            "tools/ironclaw_rune/app/src/chat/menu.rs",
            "tools/ironclaw_gpui/app/src/chat/menu.rs",
            "crates/product/ironclaw_webui/frontend/src/menu.ts",
            "docs/internal/menus.md",
        ];
        d.menu
            .fixture_files(&root, files.iter().map(|f| f.to_string()).collect(), "men");
    }
}

/// A sidebar thread dragged over the new-thread composer.
pub fn fixture_thread_drag_draft(s: &mut State) {
    fixture_draft(s);
    super::thread_drag::fixture(
        s,
        "t-pin",
        "Rune port plan",
        [790., 440.],
        [424., 380., 720., 130.],
    );
}

/// A sidebar thread dragged over the open chat.
pub fn fixture_thread_drag(s: &mut State) {
    crate::chat::fixtures::chat(s);
    super::thread_drag::fixture(
        s,
        "t-pin",
        "Rune port plan",
        [760., 380.],
        [250., 0., 1030., 790.],
    );
}

/// The status bar with a filling context window and its banner.
pub fn fixture_status_bar(s: &mut State) {
    fixture_shell(s);
    s.workspace.telemetry = Some(ThreadTelemetry {
        context_used: Some(171_000),
        context_max: Some(200_000),
        tps: Some(62.),
        cache_hit_rate: Some(0.74),
        ..ThreadTelemetry::default()
    });
}

/// The context meter's popover open on a measured report.
pub fn fixture_context_meter(s: &mut State) {
    fixture_status_bar(s);
    let report: Result<ThreadContextReport, _> = serde_json::from_value(serde_json::json!({
        "threadId": "t-pin", "runtimeId": "ironclaw", "used": 171000, "max": 200000,
        "level": "warning", "source": "provider",
        "categories": [
            {"category": "system_prompt", "tokens": 9000},
            {"category": "tool_definitions", "tokens": 22000},
            {"category": "user_messages", "tokens": 18000},
            {"category": "assistant_messages", "tokens": 41000},
            {"category": "tool_results", "tokens": 80000},
            {"category": "attachments", "tokens": 1000}
        ],
        "toolOwners": [
            {"owner": {"kind": "builtin"}, "tools": 24, "tokens": 15000},
            {"owner": {"kind": "mcp", "name": "notion"}, "tools": 9, "tokens": 7000}
        ],
        "compactPrompt": "/compact", "autoCompacts": false, "updatedAt": null
    }));
    match report {
        Ok(report) => s.workspace.context.fixture(report),
        Err(e) => tracing::warn!(error = %e, "context fixture did not parse"),
    }
}

/// The settings pages' sidebar nav, Notifications with a badge.
pub fn fixture_settings_nav(s: &mut State) {
    fixture_shell(s);
    s.page = Page::Appearance;
    s.workspace.unread = 3;
}

/// The connection error screen.
pub fn fixture_disconnected(s: &mut State) {
    base(s);
    s.conn = Conn::Failed {
        message: "error sending request: connection refused\n\nIronClaw starts its service when the app opens. If this persists, run `ironclaw service install` once, then try again.".into(),
        retry_in: Some(10),
    };
}

/// The quick composer's card (it is its own window; headless tools draw the
/// primary window, so the scene shows the card over the shell), dictating:
/// the voice controls with the Listening pill, the mic on.
pub fn fixture_quick(s: &mut State) {
    fixture_shell(s);
    s.workspace.quick_fixture = true;
    s.workspace.quick.input = rune::TextEdit::new("Summarise yesterday's failed runs");
    let quick = crate::voice::Composer::Quick;
    crate::voice::fixtures::with_status(s, &["click", "words"], Some(quick));
}
