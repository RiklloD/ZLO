//! The sidebar inbox (t3code's): one list by recency with search and
//! filters (profile, model, state), Pinned on top, the Working, Snoozed (N)
//! and Settled (N) shelves at the bottom (a working thread that waits on an
//! approval or sign-in comes back up to the inbox, marked "Needs you"), and
//! the profile menu of the new-thread screen. (GPUI `workspace/inbox.rs`;
//! the drawing is in `inbox/view.rs`.)
//!
//! The backend joins and classifies every row (`GET /inbox`), so this file
//! only renders and sends marks. Refresh is event-driven: on connect, when
//! the window comes to the front, after a run or a mark. While a row is
//! working, a slow poll follows it, only with the window in front. A snooze
//! or usage limit that ends sets one timer for that moment; a working row
//! repaints its timer once a second; an idle inbox schedules nothing.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use chrono::Utc;
use zlo_client::inbox::{
    InboxAction, InboxEngine, InboxListResponse, InboxProfile, InboxRow, InboxState, InboxStatus,
};
use zlo_client::profiles::DEFAULT_PROFILE_ID;
use rune::TextEdit;

use super::{active_id, draft};
use crate::jobs;
use crate::state::{Page, State};

mod labels;
mod row_menu;
mod shelves;
pub(super) mod subagents;
pub(super) mod view;

pub(super) use labels::{branch_tail, short_age, wake_label};
pub(super) use row_menu::{open_row_menu, settle_blocker, snooze_presets};
pub(super) use shelves::{jump_target, shelves};

/// Poll period while a row is working.
const POLL: Duration = Duration::from_secs(5);
/// How long the inbox keeps polling after a run ends.
const TITLE_SETTLE_MS: i64 = 20_000;
/// Settled rows shown before "Show more".
pub(super) const SETTLED_PAGE: usize = 30;

pub struct Inbox {
    pub list: InboxListResponse,
    /// The search field.
    pub search: TextEdit,
    pub filters_open: bool,
    /// The new-thread button's profile menu.
    pub new_menu_open: bool,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub state: Option<InboxState>,
    /// Only threads imported from terminal sessions.
    pub imported_only: bool,
    /// Folded state of the Working shelf; open by default, as in t3code.
    pub show_working: bool,
    pub show_snoozed: bool,
    pub show_settled: bool,
    pub settled_limit: usize,
    pub snooze_menu: Option<String>,
    /// Rows marked unread from the context menu. Local to this app run:
    /// the backend has no read flag; opening the thread clears it.
    pub unread: BTreeSet<String>,
    /// Keep polling until then (Unix ms) after a run ends: the backend
    /// writes the thread's generated title a few seconds later.
    settle_poll_until: i64,
    loading: bool,
    stale: bool,
    /// Parents whose subagent list is unfolded (closed by default).
    pub expanded: BTreeSet<String>,
    /// The thread whose parents were last unfolded to show it.
    revealed: Option<String>,
    /// ⌘ alone is held: cards show their `⌘N` jump badge.
    pub cmd_held: bool,
    /// The spacer that seats the shelves at the sidebar's bottom.
    pub(super) shelf_gap: view::list::ShelfGap,
}

impl Inbox {
    pub(super) fn new() -> Self {
        Self {
            list: InboxListResponse::default(),
            search: TextEdit::new(""),
            filters_open: false,
            new_menu_open: false,
            profile: None,
            model: None,
            state: None,
            imported_only: false,
            show_working: true,
            show_snoozed: false,
            show_settled: false,
            settled_limit: SETTLED_PAGE,
            snooze_menu: None,
            unread: BTreeSet::new(),
            settle_poll_until: 0,
            loading: false,
            stale: false,
            expanded: BTreeSet::new(),
            revealed: None,
            cmd_held: false,
            shelf_gap: view::list::ShelfGap::default(),
        }
    }

    pub fn row(&self, thread_id: &str) -> Option<&InboxRow> {
        self.list.rows.iter().find(|r| r.thread_id == thread_id)
    }

    /// The thread to open first: the top open row. Never a parked one; with
    /// none open the caller shows the new-thread screen.
    pub fn first_open(&self) -> Option<String> {
        self.list
            .rows
            .iter()
            .filter(|r| !has_listed_parent(&self.list.rows, r))
            .find(|r| !matches!(r.state, InboxState::Snoozed | InboxState::Settled))
            .map(|r| r.thread_id.clone())
    }

    pub fn profile(&self, id: &str) -> Option<&InboxProfile> {
        self.list.profiles.iter().find(|p| p.profile_id == id)
    }

    fn any_working(&self) -> bool {
        self.list
            .rows
            .iter()
            .any(|r| r.status == Some(InboxStatus::Working))
    }

    fn needs_poll(&self) -> bool {
        Utc::now().timestamp_millis() < self.settle_poll_until
            || self.list.rows.iter().any(|r| {
                r.title_pending
                    || matches!(
                        r.status,
                        Some(
                            InboxStatus::Working
                                | InboxStatus::Background
                                | InboxStatus::NeedsInput
                        )
                    )
            })
    }

    /// The next moment a row changes on its own (a snooze ends, a limit lifts).
    fn next_wake(&self, now: i64) -> Option<i64> {
        self.list
            .rows
            .iter()
            .flat_map(|r| [r.snooze_until, r.limit_reset_at.map(|at| at + 60_000)])
            .flatten()
            .filter(|at| *at > now)
            .min()
    }

    /// The search field's text, trimmed and lowercased.
    pub fn query(&self) -> String {
        self.search.text.trim().to_lowercase()
    }

    pub fn matches(&self, row: &InboxRow, query: &str) -> bool {
        if self.profile.as_ref().is_some_and(|p| *p != row.profile_id)
            || self.state.is_some_and(|s| s != row.state)
            || (self.imported_only && row.imported.is_none())
            || self
                .model
                .as_ref()
                .is_some_and(|m| !row.engines.iter().any(|e| e.model.as_ref() == Some(m)))
        {
            return false;
        }
        if query.is_empty() {
            return true;
        }
        let profile = self
            .profile(&row.profile_id)
            .map(|p| p.display_name.to_lowercase());
        title_of(row).to_lowercase().contains(query)
            || profile.is_some_and(|p| p.contains(query))
            || row.engines.iter().any(|e| {
                e.model
                    .as_deref()
                    .is_some_and(|m| m.to_lowercase().contains(query))
            })
    }

    pub fn models(&self) -> Vec<String> {
        let set: BTreeSet<String> = self
            .list
            .rows
            .iter()
            .flat_map(|r| r.engines.iter().filter_map(|e| e.model.clone()))
            .collect();
        set.into_iter().collect()
    }

    pub fn filtered(&self) -> bool {
        self.profile.is_some() || self.model.is_some() || self.state.is_some() || self.imported_only
    }
}

/// A subagent drawn under its parent rather than as a row of its own.
fn has_listed_parent(rows: &[InboxRow], row: &InboxRow) -> bool {
    row.parent_thread_id
        .as_deref()
        .is_some_and(|p| p != row.thread_id && rows.iter().any(|r| r.thread_id == p))
}

/// Choose only a confirmed open row; an empty result always means a fresh
/// draft. A settled subagent hands the screen back to its parent while the
/// parent is open; otherwise the next top-level row takes it.
fn next_after_settle(rows: &[InboxRow], settled: &str) -> Option<String> {
    let open = |r: &InboxRow| {
        matches!(
            r.state,
            InboxState::Pinned | InboxState::Inbox | InboxState::Working
        )
    };
    let parent = rows
        .iter()
        .find(|r| r.thread_id == settled)
        .and_then(|r| r.parent_thread_id.as_deref())
        .and_then(|p| rows.iter().find(|r| r.thread_id == p))
        .filter(|r| r.thread_id != settled && open(r));
    if let Some(parent) = parent {
        return Some(parent.thread_id.clone());
    }
    rows.iter()
        .filter(|r| r.state == InboxState::Pinned)
        .chain(
            rows.iter()
                .filter(|r| matches!(r.state, InboxState::Inbox | InboxState::Working)),
        )
        .filter(|r| !has_listed_parent(rows, r))
        .find(|r| r.thread_id != settled)
        .map(|r| r.thread_id.clone())
}

pub fn title_of(row: &InboxRow) -> &str {
    let title = row
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("New conversation");
    strip_model_suffix(title, &row.engines)
}

/// A model switch forks the thread as "<title> (<model>)". The sidebar
/// names the profile, never the model, so that suffix is dropped.
fn strip_model_suffix<'a>(title: &'a str, engines: &[InboxEngine]) -> &'a str {
    let Some((head, inner)) = title
        .strip_suffix(')')
        .and_then(|t| t.rfind(" (").map(|at| (&t[..at], &t[at + 2..])))
    else {
        return title;
    };
    let named = engines.iter().any(|e| {
        [e.model.as_deref(), e.provider.as_deref()]
            .into_iter()
            .flatten()
            .any(|m| m.eq_ignore_ascii_case(inner))
    });
    let model_like =
        !inner.contains(' ') && inner.contains('-') && inner.chars().any(|c| c.is_ascii_digit());
    let head = head.trim_end();
    if (named || model_like) && !head.is_empty() {
        head
    } else {
        title
    }
}

/// Take a fresh list: titles cache, run clock, poll, wake timer.
pub(super) fn apply(s: &mut State, list: InboxListResponse) {
    crate::titles::set_rows(s, &list.rows);
    crate::run_clock::set_rows(s, &list.rows);
    // Any live status counts: a run can go Needs input → Done between polls.
    let inbox = &s.workspace.inbox;
    let finished = list.rows.iter().any(|row| {
        inbox.row(&row.thread_id).is_some_and(|r| {
            matches!(
                r.status,
                Some(InboxStatus::Working | InboxStatus::Background | InboxStatus::NeedsInput)
            )
        }) && row.status != Some(InboxStatus::Working)
    });
    s.workspace.inbox.list = list;
    super::draft::bind_catalog(s);
    if finished {
        poll_for_title(s);
    }
    reveal_active_subagent(s);
    ensure_poll(s);
    schedule_wake(s);
    ensure_tick(s);
}

fn ensure_tick(s: &mut State) {
    if !s.workspace.inbox.any_working() {
        s.workspace.timers.tick = None;
        return;
    }
    if s.workspace.timers.tick.is_none() {
        s.workspace.timers.tick = Some(Instant::now() + Duration::from_secs(1));
    }
}

/// The Working tick came due: repaint, and keep ticking while a row works
/// with the window in front. Activation refreshes the inbox, which starts
/// the tick again.
pub(super) fn tick_due(s: &mut State) -> bool {
    if !s.workspace.window_active || !s.workspace.inbox.any_working() {
        return false;
    }
    s.workspace.timers.tick = Some(Instant::now() + Duration::from_secs(1));
    true
}

/// A run ended: poll a little longer to pick up its generated title.
pub(super) fn poll_for_title(s: &mut State) {
    s.workspace.inbox.settle_poll_until = Utc::now().timestamp_millis() + TITLE_SETTLE_MS;
    ensure_poll(s);
}

/// Re-read the inbox. Coalesces: a call during a read marks it stale.
pub fn refresh(s: &mut State) {
    if s.rt.backend.is_none() {
        return;
    }
    if s.workspace.inbox.loading {
        s.workspace.inbox.stale = true;
        return;
    }
    s.workspace.inbox.loading = true;
    jobs::run(
        s,
        "inbox",
        |c| async move { c.inbox().await },
        |s, result, _| {
            s.workspace.inbox.loading = false;
            match result {
                Ok(list) => apply(s, list),
                // silent-ok: the list keeps its last state; the next event retries.
                Err(e) => tracing::debug!(error = %e, "inbox refresh failed"),
            }
            if std::mem::take(&mut s.workspace.inbox.stale) {
                refresh(s);
            }
        },
    );
}

/// Write one mark; the answer is the new list.
pub fn mark(s: &mut State, thread_id: String, action: InboxAction, until: Option<i64>) {
    s.workspace.inbox.snooze_menu = None;
    jobs::run(
        s,
        "inbox mark",
        move |c| async move { c.inbox_mutate(&thread_id, action, until).await },
        |s, result, _| match result {
            Ok(list) => apply(s, list),
            // silent-ok: an older gateway has no marks; the row stays as it was.
            Err(e) => tracing::debug!(error = %e, "inbox mark failed"),
        },
    );
}

/// Navigate only after the gateway confirms the mark. Never use an old
/// sidebar snapshot to choose a thread that may already be settled.
pub fn settle(s: &mut State, thread_id: String) {
    let id = thread_id.clone();
    jobs::run(
        s,
        "inbox settle",
        move |c| async move { c.inbox_mutate(&id, InboxAction::Settle, None).await },
        move |s, result, _| {
            let list = match result {
                Ok(list) => list,
                Err(e) => {
                    tracing::warn!(error = %e, "could not settle thread");
                    return;
                }
            };
            let next = next_after_settle(&list.rows, &thread_id);
            let open = s.page == Page::Chat && active_id(s) == Some(thread_id.as_str());
            apply(s, list);
            match next {
                None => {
                    s.chat = None;
                    s.workspace.draft = None;
                    draft::start_in_profile(s, DEFAULT_PROFILE_ID.into());
                }
                Some(next) if open => super::open_thread(s, next),
                Some(_) => {}
            }
        },
    );
}

/// Opening a row with an unseen result clears its Done / Woke badge.
pub(super) fn visit(s: &mut State, thread_id: &str) {
    s.workspace.inbox.unread.remove(thread_id);
    let unseen = s
        .workspace
        .inbox
        .row(thread_id)
        .is_some_and(|r| matches!(r.status, Some(InboxStatus::Done | InboxStatus::Woke)));
    if unseen {
        mark(s, thread_id.to_string(), InboxAction::Visit, None);
    }
}

fn ensure_poll(s: &mut State) {
    if s.workspace.timers.inbox.is_some() || !s.workspace.inbox.needs_poll() {
        return;
    }
    s.workspace.timers.inbox = Some(Instant::now() + POLL);
}

/// The working poll came due.
pub(super) fn poll_due(s: &mut State) {
    if !s.workspace.inbox.needs_poll() {
        return;
    }
    // In the background nothing is watching: let the poll lapse instead of
    // waking every 5 s to do nothing. Activation refreshes the inbox, whose
    // reply arms the poll again (`apply` → `ensure_poll`).
    if !s.workspace.window_active {
        return;
    }
    refresh(s);
    s.workspace.timers.inbox = Some(Instant::now() + POLL);
}

fn schedule_wake(s: &mut State) {
    let now = Utc::now().timestamp_millis();
    s.workspace.timers.wake = s
        .workspace
        .inbox
        .next_wake(now)
        .map(|at| Instant::now() + Duration::from_millis((at - now).max(0) as u64));
}

/// "New profile" in the filters or the profile menu.
pub(super) fn new_profile(s: &mut State) {
    s.workspace.inbox.new_menu_open = false;
    if let Some(draft) = s.workspace.draft.as_mut() {
        draft.menu_open = false;
    }
    super::show(s, Page::Profiles);
    crate::pages::profiles::new_profile(s);
}

/// Open the subagent lists above the thread on screen, once per thread
/// opened, so the selection is visible and can still be folded.
pub(super) fn reveal_active_subagent(s: &mut State) {
    let active = (s.page == Page::Chat)
        .then(|| active_id(s).map(str::to_string))
        .flatten();
    if s.workspace.inbox.revealed == active {
        return;
    }
    s.workspace.inbox.revealed = active.clone();
    if let Some(id) = active {
        let tree = subagents::Tree::new(&s.workspace.inbox.list.rows);
        let above = tree.ancestors(&id);
        s.workspace.inbox.expanded.extend(above);
    }
}

pub(super) fn toggle_subagents(s: &mut State, root: &str) {
    let expanded = &mut s.workspace.inbox.expanded;
    if !expanded.remove(root) {
        expanded.insert(root.to_string());
    }
}

/// The search field changed: message text matches too, the backend
/// searches past chats.
pub(super) fn search_changed(s: &mut State) {
    let query = s.workspace.inbox.search.text.clone();
    super::message_search::search(s, &query);
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn row(id: &str, state: InboxState, model: Option<&str>) -> InboxRow {
        InboxRow {
            thread_id: id.into(),
            title: Some(format!("Title {id}")),
            profile_id: "default".into(),
            state,
            status: None,
            engines: vec![InboxEngine {
                runtime_id: "ironclaw".into(),
                provider: None,
                model: model.map(str::to_string),
            }],
            last_activity_at: 0,
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

    #[test]
    fn settling_the_last_open_row_never_selects_a_parked_thread() {
        let rows = vec![
            row("recent", InboxState::Settled, None),
            row("last", InboxState::Settled, None),
            row("later", InboxState::Snoozed, None),
        ];
        assert_eq!(next_after_settle(&rows, "last"), None);
        let rows = vec![
            row("old", InboxState::Settled, None),
            row("next", InboxState::Inbox, None),
            row("last", InboxState::Settled, None),
        ];
        assert_eq!(next_after_settle(&rows, "last"), Some("next".into()));
    }

    #[test]
    fn filters_combine_and_search_reads_titles_and_models() {
        let mut inbox = Inbox::new();
        let a = row("a", InboxState::Inbox, Some("opus"));
        let b = row("b", InboxState::Settled, Some("sonnet"));
        assert!(inbox.matches(&a, "title a"));
        assert!(inbox.matches(&b, "sonnet"));
        assert!(!inbox.matches(&a, "sonnet"));
        inbox.model = Some("opus".into());
        assert!(inbox.matches(&a, ""));
        assert!(!inbox.matches(&b, ""));
        inbox.model = None;
        inbox.state = Some(InboxState::Settled);
        assert!(!inbox.matches(&a, ""));
        assert!(inbox.matches(&b, ""));
        // Imported: only rows that came from a terminal session.
        inbox.state = None;
        inbox.imported_only = true;
        let mut c = row("c", InboxState::Settled, None);
        c.imported = Some("claude_code".into());
        assert!(!inbox.matches(&b, "") && inbox.matches(&c, ""));
    }

    #[test]
    fn titles_drop_a_model_suffix_only() {
        let mut r = row("a", InboxState::Inbox, Some("gpt-6.1-sol"));
        let shown = |r: &InboxRow| title_of(r).to_string();
        r.title = Some("thread (claude-opus-5-5)".into());
        assert_eq!(shown(&r), "thread");
        r.title = Some("Plan (gpt-6.1-sol)".into());
        assert_eq!(shown(&r), "Plan");
        r.title = Some("Notes (fork)".into());
        assert_eq!(shown(&r), "Notes (fork)");
        r.title = Some("Budget (Q3 2026)".into());
        assert_eq!(shown(&r), "Budget (Q3 2026)");
        r.title = Some("(claude-opus-5-5)".into());
        assert_eq!(shown(&r), "(claude-opus-5-5)");
    }

    #[test]
    fn wake_timer_targets_the_earliest_future_change() {
        let mut inbox = Inbox::new();
        let mut snoozed = row("s", InboxState::Snoozed, None);
        snoozed.snooze_until = Some(5_000);
        let mut limited = row("l", InboxState::Inbox, None);
        limited.limit_reset_at = Some(1_000);
        let mut past = row("p", InboxState::Snoozed, None);
        past.snooze_until = Some(10);
        inbox.list.rows = vec![snoozed, limited, past];
        // The limit lifts at 1 000 plus the engine's one-minute grace.
        assert_eq!(inbox.next_wake(100), Some(5_000));
        assert_eq!(inbox.next_wake(6_000), Some(61_000));
        assert_eq!(inbox.next_wake(70_000), None);
    }

    #[test]
    fn snooze_presets_are_all_in_the_future() {
        let now = Utc::now().timestamp_millis();
        let presets = snooze_presets();
        assert!(presets.len() >= 3);
        assert!(presets.iter().all(|(_, at)| *at > now));
    }

    #[test]
    fn long_branches_keep_their_last_segment() {
        assert_eq!(branch_tail("feature/team/sidebar-pass"), "…/sidebar-pass");
        assert_eq!(branch_tail("main"), "main");
    }

    /// A new thread lists its models: the draft's picker reads the catalog
    /// on open, with or without a thread (`GET /providers`), and binds a
    /// thread for saving its preferences once the inbox has one, keeping the
    /// user's pick.
    #[test]
    fn a_new_thread_loads_its_model_catalog() {
        // The inbox already lists a thread.
        let mut s = crate::chat::fixtures::test_state();
        s.workspace.inbox.list.rows = vec![row("t1", InboxState::Inbox, None)];
        draft::open(&mut s);
        let picker = &s.workspace.draft.as_ref().expect("draft").picker;
        assert_eq!(picker.catalog_thread.as_deref(), Some("t1"));
        assert!(picker.route_requested(), "the catalog is read on open");

        // A fresh account: no thread at all.
        let mut s = crate::chat::fixtures::test_state();
        draft::open(&mut s);
        let picker = &s.workspace.draft.as_ref().expect("draft").picker;
        assert_eq!(picker.catalog_thread, None);
        assert!(
            picker.route_requested(),
            "no thread is needed to list models"
        );
        let picked = Some(("codex".to_string(), "gpt-6.1-sol".to_string()));
        s.workspace.draft.as_mut().expect("draft").picker.current = picked.clone();
        apply(
            &mut s,
            InboxListResponse {
                rows: vec![row("t2", InboxState::Inbox, None)],
                ..Default::default()
            },
        );
        let picker = &s.workspace.draft.as_ref().expect("draft").picker;
        assert_eq!(picker.catalog_thread.as_deref(), Some("t2"));
        assert_eq!(picker.current, picked, "binding keeps the pick");
    }
}
