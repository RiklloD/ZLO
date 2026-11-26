//! Headless fixture states for the workspace's scenes (`scenes.rs`): an
//! inbox with every shelf, subagents, a context-heavy thread, the ⌘K menu,
//! the new-thread screen and the settings nav. No gateway is involved.

use chrono::Utc;
use rune::TextEdit;
use zlo_client::history::{
    HistoryHighlight, HistoryHit, HistorySearchRole, HistoryThreadOrigin, HistoryThreadResult,
};
use zlo_client::inbox::{
    InboxEngine, InboxListResponse, InboxProfile, InboxRow, InboxState, InboxStatus,
};
use zlo_client::usage::{ThreadContextReport, ThreadTelemetry};

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

/// The bench scenes' window (`crate::bench`): the shell with the bench's
/// inbox rows, nothing open yet.
pub(crate) fn bench_shell(s: &mut State) {
    base(s);
    s.workspace.inbox.list = InboxListResponse {
        rows: crate::bench::inbox_rows(),
        profiles: inbox().profiles,
        last_used_profile_id: Some("default".into()),
    };
}

/// A bench run starts or ends in `id`, as a send and the next inbox read
/// would show it: Working claims the card for the shelf at once, and the
/// new list lands through the same path a gateway read takes.
pub(crate) fn bench_set_status(s: &mut State, id: &str, status: Option<InboxStatus>) {
    if status == Some(InboxStatus::Working) {
        super::inbox::note_sent(s, id);
    }
    let mut list = s.workspace.inbox.list.clone();
    if let Some(row) = list.rows.iter_mut().find(|r| r.thread_id == id) {
        row.status = status;
        row.last_activity_at = Utc::now().timestamp_millis();
        // The gateway lists a running thread on the Working shelf.
        if status == Some(InboxStatus::Working) {
            row.state = InboxState::Working;
            row.working_since = Some(row.last_activity_at);
        } else {
            row.state = InboxState::Inbox;
            row.working_since = None;
        }
    }
    if status != Some(InboxStatus::Working) {
        super::run_ended(s, id.to_string(), false);
    }
    super::inbox::apply(s, list);
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

/// The sidebar's profile menu on a long profile list: recents with their
/// age, the rest by name, a profile working and one that needs an answer.
pub fn fixture_profile_menu(s: &mut State) {
    fixture_shell(s);
    let now = Utc::now().timestamp_millis();
    let profile = |id: &str, name: &str, branch: Option<&str>, mins: Option<i64>| InboxProfile {
        profile_id: id.into(),
        display_name: name.into(),
        git_branch: branch.map(str::to_string),
        last_used_at: mins.map(|m| now - m * 60_000),
    };
    s.workspace.inbox.list.profiles = vec![
        profile("default", "Default", None, Some(0)),
        profile("avallon", "Avallon Case", None, None),
        profile("flat", "Flat Finder", None, Some(60 * 30)),
        profile("futur", "Futurchoices", None, None),
        profile("work", "IronClaw Builder", Some("main"), Some(3)),
        profile("verpin", "Verpin", Some("main"), Some(190)),
        profile("thesis", "Thesis", None, None),
        profile("notion", "Notion Manager", None, None),
        profile("finance", "Financial Advisor", None, None),
        profile("cyber", "Cyberzen", Some("master"), None),
        profile("network", "Network Manager", None, None),
        profile("club", "Techclub", None, None),
    ];
    s.workspace.inbox.list.last_used_profile_id = Some("default".into());
    super::inbox::picker::toggle_new_menu(s);
}

/// The new-thread screen's profile switch, filtered by typing, the
/// second match selected.
pub fn fixture_profile_menu_search(s: &mut State) {
    fixture_profile_menu(s);
    s.chat = None;
    super::draft::open(s);
    super::inbox::picker::toggle_draft_menu(s);
    s.workspace.inbox.picker.search.replace_text("man");
    s.workspace.inbox.picker.fixture_select(1);
}

/// Every inbox shelf unfolded, ⌘ held (jump badges), one snooze menu open
/// and the filters panel.
pub fn fixture_inbox(s: &mut State) {
    fixture_shell(s);
    s.workspace.inbox.cmd_held = true;
    s.workspace.inbox.filters_open = true;
    s.workspace.inbox.snooze_menu = Some(super::inbox::RowId::focused(s, "t-plain"));
    s.workspace
        .inbox
        .unread
        .insert(super::inbox::RowId::focused(s, "t-plain"));
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
/// sidebar's bottom, not under the last card.
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
    let p_id = super::inbox::RowId::focused(s, "t-parent");
    let a_id = super::inbox::RowId::focused(s, "t-a");
    s.workspace.inbox.expanded.insert(p_id);
    s.workspace.inbox.expanded.insert(a_id);
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
            "tools/ironclaw_rune/app/src/chat/composer.rs",
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
        message: "error sending request: connection refused\n\nIronClaw starts its service when the app opens. If this persists, run `zlo service install` once, then try again.".into(),
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

/// The sidebar footer's update button (`shell::update_button`): `available`
/// (This Mac behind), `confirm` (its dialog open), `confirm-many` (two
/// computers behind), `running` (This Mac building), `failed`.
pub fn fixture_update(s: &mut State, which: &str) {
    use crate::pages::computers::fixtures::{STUDIO, base};
    use crate::remote::updates::{Pending, RowState};
    use zlo_client::environments::THIS_MAC;
    fixture_shell(s);
    let pending = |behind: u32| Pending {
        behind,
        from: Some("68d1df2e7c0ffee0000000000000000000000000".into()),
        to: "a4f19b3d2e0000000000000000000000000000000".into(),
        subjects: [
            "feat(rune): update button in the sidebar footer",
            "fix(webui): pin the storage key for setLang",
            "test(trace-commons): one mkdtemp dir per call",
            "chore(deps): pin TypeScript to 6.0.3 for the WebUI frontend",
            "fix(rune): the Working shelf keeps its fold across switches",
        ]
        .into_iter()
        .take(behind as usize)
        .map(String::from)
        .collect(),
    };
    let u = &mut s.remote.updates;
    match which {
        "running" => {
            u.updating = true;
            u.rows.insert(
                THIS_MAC.into(),
                RowState::Updating("Building ZLO.app".into()),
            );
        }
        "failed" => {
            u.failed_run = true;
            u.rows.insert(
                THIS_MAC.into(),
                RowState::Failed("The checkout has local changes".into()),
            );
        }
        _ => {
            u.rows.insert(THIS_MAC.into(), RowState::Behind(pending(7)));
        }
    }
    if which == "confirm-many" {
        base(s);
        s.remote
            .updates
            .rows
            .insert(STUDIO.into(), RowState::Behind(pending(3)));
    }
    s.workspace.confirm_update = which
        .starts_with("confirm")
        .then(|| crate::remote::updates::waiting_keys(&s.remote.updates));
}

/// New-thread screen with two connected computers ("Runs on" next to worktree).
pub fn fixture_draft_runs_on(s: &mut State) {
    use crate::onboarding::gateway::Computer;
    use crate::pages::computers::fixtures::{MINI, STUDIO};
    use crate::remote::peers::{Peer, PeerConn};
    fixture_draft(s);
    s.remote.peers.insert_fixture(Peer::fixture(
        Computer::remote(STUDIO, "Studio"),
        PeerConn::Ready,
    ));
    s.remote.peers.insert_fixture(Peer::fixture(
        Computer::remote(MINI, "Mac mini"),
        PeerConn::Asleep,
    ));
}

/// New-thread screen with the "Runs on" computer menu open.
pub fn fixture_draft_runs_on_menu(s: &mut State) {
    fixture_draft_runs_on(s);
    if let Some(d) = s.workspace.draft.as_mut() {
        d.runs_on_open = true;
    }
}

/// Settings → Computers with "New threads start on" set to a named computer.
pub fn fixture_computers_default_pref(s: &mut State) {
    use crate::pages::computers::fixtures::STUDIO;
    crate::pages::computers::fixtures::computers(s);
    s.prefs.default_computer = Some(STUDIO.into());
}

// ── Stage 2a: one sidebar across computers ───────────────────────────────

/// Two computers connected (This Mac focused, Studio as a ready peer): rows
/// merged by recency across Pinned, Inbox, Working, Snoozed and Settled, with
/// quiet computer tags on Studio's rows.
pub fn fixture_inbox_two_computers(s: &mut State) {
    use crate::onboarding::gateway::Computer;
    use crate::pages::computers::fixtures::STUDIO;
    use crate::remote::peers::{Peer, PeerConn};
    fixture_shell(s);
    let now = Utc::now().timestamp_millis();
    let mut studio_pin = row(
        "s-pin",
        "Benchmark the CUDA runner",
        "infra",
        InboxState::Pinned,
        15,
    );
    studio_pin.worktree_branch = Some("feat/cuda-runner".into());
    let mut studio_needs = row(
        "s-needs",
        "Review the Linux release build",
        "infra",
        InboxState::Inbox,
        3,
    );
    studio_needs.status = Some(InboxStatus::NeedsInput);
    let mut studio_work = row(
        "s-work",
        "Index the monorepo symbols",
        "infra",
        InboxState::Working,
        1,
    );
    studio_work.status = Some(InboxStatus::Working);
    studio_work.working_since = Some(now - 92_000);
    let mut studio_done = row(
        "s-done",
        "Run the nightly fuzz targets",
        "default",
        InboxState::Inbox,
        20,
    );
    studio_done.status = Some(InboxStatus::Done);
    let mut studio_settled = row(
        "s-settled",
        "Upgrade the staging Postgres image",
        "infra",
        InboxState::Settled,
        1400,
    );
    studio_settled.settled_at = Some(now - 43_200_000);
    let mut peer = Peer::fixture(Computer::remote(STUDIO, "Studio"), PeerConn::Ready);
    peer.inbox = Some(InboxListResponse {
        rows: vec![
            studio_pin,
            studio_needs,
            studio_done,
            studio_work,
            studio_settled,
        ],
        profiles: vec![
            InboxProfile {
                profile_id: "default".into(),
                display_name: "Default".into(),
                git_branch: None,
                last_used_at: None,
            },
            InboxProfile {
                profile_id: "infra".into(),
                display_name: "Infra".into(),
                git_branch: Some("main".into()),
                last_used_at: None,
            },
        ],
        last_used_profile_id: Some("infra".into()),
    });
    s.remote.peers.insert_fixture(peer);
}

/// One computer connected, one peer asleep: quiet status line at the sidebar's
/// foot ("Studio is asleep"), no computer tags on the focused rows.
pub fn fixture_inbox_peer_asleep(s: &mut State) {
    use crate::onboarding::gateway::Computer;
    use crate::pages::computers::fixtures::STUDIO;
    use crate::remote::peers::{Peer, PeerConn};
    fixture_shell(s);
    s.remote.peers.insert_fixture(Peer::fixture(
        Computer::remote(STUDIO, "Studio"),
        PeerConn::Asleep,
    ));
}
