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

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use chrono::Utc;
use rune::{TextEdit, Tx};
use zlo_client::ClientError;
use zlo_client::inbox::{
    InboxAction, InboxEngine, InboxListResponse, InboxProfile, InboxRow, InboxState, InboxStatus,
};
use zlo_client::profiles::{DEFAULT_PROFILE_ID, ProfileInfo};

use super::{active_id, draft};
use crate::jobs;
use crate::state::{Page, State};

mod labels;
pub(super) mod picker;
mod row_menu;
mod shelves;
pub(super) mod subagents;
pub(super) mod view;

pub(super) use labels::{branch_tail, short_age, wake_label};
pub(super) use row_menu::{open_row_menu, settle_blocker, snooze_presets};
pub(super) use shelves::{jump_target, shelves};

/// Poll period while a row is working.
const POLL: Duration = Duration::from_secs(5);
/// Poll period while a prompt sent here waits for the backend to list its
/// run as working.
const SENT_POLL: Duration = Duration::from_secs(1);
/// How long a sent prompt shows its card as working without the backend
/// confirming it (a send that never started a run stops claiming it).
const SENT_GRACE: Duration = Duration::from_secs(30);
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
    /// The open profile menu's search and selection (`picker.rs`).
    pub picker: picker::ProfilePicker,
    pub profile: Option<String>,
    pub model: Option<String>,
    pub state: Option<InboxState>,
    /// Only threads imported from terminal sessions.
    pub imported_only: bool,
    /// Folded state of the Working shelf; folded by default, as in t3code.
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
    /// Bumped by each profile change made here ([`profiles_changed`]): a
    /// read that left before one answers with the profiles it replaced.
    profiles_epoch: u64,
    /// A read the person caused is waiting or in flight ([`refresh_shown`]):
    /// its answer moves the rows instead of landing them.
    shown: bool,
    /// Parents whose subagent list is unfolded (closed by default).
    pub expanded: BTreeSet<String>,
    /// The thread whose parents were last unfolded to show it.
    revealed: Option<String>,
    /// ⌘ alone is held: cards show their `⌘N` jump badge.
    pub cmd_held: bool,
    /// The spacer that seats the shelves at the sidebar's bottom.
    pub(super) shelf_gap: view::list::ShelfGap,
    /// Prompts sent from this window whose run the inbox has not listed as
    /// working yet, with when they were sent. The read after a send can
    /// land before the backend marks the run; without this the card read
    /// "now" and nothing polled until the run ended.
    pub(super) sent: BTreeMap<String, Sent>,
}

/// A prompt sent from this window, waiting for the backend to list its run.
#[derive(Clone, Copy, Debug)]
pub(super) struct Sent {
    pub(super) at: Instant,
    /// The row's status when the prompt went out: a read still carrying it
    /// (an answered "Needs you") is stale, not the new run's.
    pub(super) before: Option<InboxStatus>,
}

impl Sent {
    fn live(&self) -> bool {
        self.at.elapsed() < SENT_GRACE
    }

    /// The backend lists the new run: `status` is live and not the one the
    /// row had before the send.
    fn confirmed_by(&self, status: Option<InboxStatus>) -> bool {
        match status {
            Some(InboxStatus::Working | InboxStatus::Background) => true,
            Some(InboxStatus::NeedsInput) => self.before != status,
            _ => false,
        }
    }
}

impl Inbox {
    pub(super) fn new() -> Self {
        Self {
            list: InboxListResponse::default(),
            search: TextEdit::new(""),
            filters_open: false,
            new_menu_open: false,
            picker: picker::ProfilePicker::new(),
            profile: None,
            model: None,
            state: None,
            imported_only: false,
            show_working: false,
            show_snoozed: false,
            show_settled: false,
            settled_limit: SETTLED_PAGE,
            snooze_menu: None,
            unread: BTreeSet::new(),
            settle_poll_until: 0,
            loading: false,
            stale: false,
            profiles_epoch: 0,
            shown: false,
            expanded: BTreeSet::new(),
            revealed: None,
            cmd_held: false,
            shelf_gap: view::list::ShelfGap::default(),
            sent: BTreeMap::new(),
        }
    }

    /// Where `row` is drawn. Like t3code, a working thread leaves the list
    /// for the Working shelf, from the moment a prompt is sent here (before
    /// the backend lists the run), and comes back when the run ends. Pinned
    /// rows stay pinned.
    pub fn shown_state(&self, row: &InboxRow) -> InboxState {
        match row.state {
            InboxState::Inbox
                if self
                    .sent
                    .get(&row.thread_id)
                    .is_some_and(|sent| sent.live() && !sent.confirmed_by(row.status)) =>
            {
                InboxState::Working
            }
            state => state,
        }
    }

    /// The status `row` shows: the backend's, or Working while a prompt
    /// sent here waits for the backend to list its run.
    pub fn shown_status(&self, row: &InboxRow) -> Option<InboxStatus> {
        match self.sent.get(&row.thread_id) {
            Some(sent) if sent.live() && !sent.confirmed_by(row.status) => {
                Some(InboxStatus::Working)
            }
            _ => row.status,
        }
    }

    /// Threads with a run going now (as drawn: a prompt just sent counts).
    pub fn running(&self) -> usize {
        self.list
            .rows
            .iter()
            .filter(|row| {
                matches!(
                    self.shown_status(row),
                    Some(InboxStatus::Working | InboxStatus::Background)
                )
            })
            .count()
    }

    fn any_sent_live(&self) -> bool {
        self.sent.values().any(Sent::live)
    }

    pub fn row(&self, thread_id: &str) -> Option<&InboxRow> {
        self.list.rows.iter().find(|r| r.thread_id == thread_id)
    }

    pub fn profile(&self, id: &str) -> Option<&InboxProfile> {
        self.list.profiles.iter().find(|p| p.profile_id == id)
    }

    fn any_working(&self) -> bool {
        self.list
            .rows
            .iter()
            .any(|r| self.shown_status(r) == Some(InboxStatus::Working))
    }

    fn needs_poll(&self) -> bool {
        self.any_sent_live()
            || Utc::now().timestamp_millis() < self.settle_poll_until
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

/// Choose only a confirmed open row the sidebar shows; an empty result
/// always means a fresh draft (as ⇧⌘O). A settled subagent hands the screen
/// back to its parent while the parent is open; otherwise the next card on
/// the sidebar takes it. Side chats and nested subagents are never picked:
/// the sidebar hides them, so landing there reads as an old thread.
/// Call after `apply`, so the shelves already read the confirmed list.
fn next_after_settle(s: &State, settled: &str) -> Option<String> {
    let rows = &s.workspace.inbox.list.rows;
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
    first_shown(s, Some(settled))
}

/// The thread to open by itself: the top card the sidebar shows, Pinned,
/// then the list, then the folded Working shelf. Never a parked row, a side
/// chat or a nested subagent; with none, the caller shows a fresh draft.
pub(super) fn first_open(s: &State) -> Option<String> {
    first_shown(s, None)
}

fn first_shown(s: &State, except: Option<&str>) -> Option<String> {
    let sh = shelves(s);
    sh.pinned
        .iter()
        .chain(sh.main.iter())
        .chain(sh.working.iter())
        .find(|r| Some(r.thread_id.as_str()) != except)
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
    // A sent prompt stops claiming its card once the backend lists the run
    // or the grace runs out. A new thread may not be listed yet: it waits.
    s.workspace.inbox.sent.retain(|id, sent| {
        sent.live()
            && !list
                .rows
                .iter()
                .any(|r| r.thread_id == *id && sent.confirmed_by(r.status))
    });
    s.workspace.inbox.list = list;
    super::draft::bind_catalog(s);
    if finished {
        poll_for_title(s);
    }
    reveal_active_subagent(s);
    ensure_poll(s);
    // The open chat follows its tasks while the inbox says it (or a
    // subagent of it) works, even when its own stream went quiet.
    if let Some(open) = s.chat.as_ref().map(|c| c.thread_id.clone()) {
        crate::chat::orchestration::ensure_poll(s, &open);
    }
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

/// A prompt went out from this window: its card shows Working now, and the
/// inbox polls quickly until the backend lists the run.
pub fn note_sent(s: &mut State, thread_id: &str) {
    let before = s.workspace.inbox.row(thread_id).and_then(|r| r.status);
    s.workspace.inbox.sent.insert(
        thread_id.to_string(),
        Sent {
            at: Instant::now(),
            before,
        },
    );
    let soon = Instant::now() + SENT_POLL;
    let timer = &mut s.workspace.timers.inbox;
    *timer = Some(timer.map_or(soon, |at| at.min(soon)));
    ensure_tick(s);
}

/// What Settings › Profiles just did to a profile, as the server answered.
pub enum ProfileChange<'a> {
    /// Created or updated: the server's saved record.
    Saved(&'a ProfileInfo),
    /// Archived.
    Deleted(&'a str),
    /// Several changed at once (an import): only a re-read knows how.
    Reloaded,
}

/// A profile changed in Settings: the picker reads the inbox's profile
/// list, which otherwise only catches up on a thread event. The server's
/// own answer lands at once, then a re-read brings the order, branch and
/// last-used profile in line.
pub fn profiles_changed(s: &mut State, change: ProfileChange<'_>) {
    let list = &mut s.workspace.inbox.list;
    match change {
        // Before the first read the picker shows a stand-in Default: a
        // one-row list would hide it until the read lands.
        ProfileChange::Saved(_) if list.profiles.is_empty() => {}
        ProfileChange::Saved(saved) => {
            let profile = InboxProfile {
                profile_id: saved.profile_id.clone(),
                display_name: saved.settings.display_name.clone(),
                git_branch: saved.git_branch.clone(),
                last_used_at: saved.last_used_at,
            };
            match list
                .profiles
                .iter_mut()
                .find(|p| p.profile_id == profile.profile_id)
            {
                Some(row) => *row = profile,
                None => list.profiles.push(profile),
            }
        }
        ProfileChange::Deleted(id) => {
            list.profiles.retain(|p| p.profile_id != id);
            if list.last_used_profile_id.as_deref() == Some(id) {
                list.last_used_profile_id = None;
            }
            // Its chip goes, so its filter must too: nothing could turn it off.
            if s.workspace.inbox.profile.as_deref() == Some(id) {
                s.workspace.inbox.profile = None;
            }
            draft::profile_gone(s, id);
        }
        ProfileChange::Reloaded => {}
    }
    s.workspace.inbox.profiles_epoch += 1;
    refresh(s);
}

/// The run of `thread_id` ended (or its send failed): stop claiming it.
pub(super) fn forget_sent(s: &mut State, thread_id: &str) {
    s.workspace.inbox.sent.remove(thread_id);
}

/// A run ended: poll a little longer to pick up its generated title.
pub(super) fn poll_for_title(s: &mut State) {
    s.workspace.inbox.settle_poll_until = Utc::now().timestamp_millis() + TITLE_SETTLE_MS;
    ensure_poll(s);
}

/// Re-read the inbox. Coalesces: a call during a read marks it stale.
///
/// A background read (a poll, a wake, the window coming to the front, a
/// chat's update) lands its answer at once, as Rune lands any commit no
/// input caused: data changing under the person doesn't move. A read the
/// person caused goes through [`refresh_shown`].
pub fn refresh(s: &mut State) {
    if s.rt.backend.is_none() {
        return;
    }
    if s.workspace.inbox.loading {
        s.workspace.inbox.stale = true;
        return;
    }
    s.workspace.inbox.loading = true;
    let epoch = s.workspace.inbox.profiles_epoch;
    jobs::run(
        s,
        "inbox",
        |c| async move { c.inbox().await },
        move |s, result, tx| {
            s.workspace.inbox.loading = false;
            // A newer read is due: a shown read waits for it, the newest.
            let again = std::mem::take(&mut s.workspace.inbox.stale);
            let shown = !again && std::mem::take(&mut s.workspace.inbox.shown);
            match result {
                Ok(mut list) => {
                    hold_profiles(s, &mut list, epoch);
                    refreshed(s, list, shown, tx)
                }
                // silent-ok: the list keeps its last state; the next event retries.
                Err(e) => tracing::debug!(error = %e, "inbox refresh failed"),
            }
            if again {
                refresh(s);
            }
        },
    );
}

/// [`refresh`] for a read the person caused (a new thread they made, a
/// fork they asked for): its answer moves the rows, a new card enters and
/// the others step aside on `ROW_RESORT`.
pub fn refresh_shown(s: &mut State) {
    if s.rt.backend.is_none() {
        return;
    }
    s.workspace.inbox.shown = true;
    refresh(s);
}

/// A snapshot asked for before a profile change made here (`epoch` is the
/// count when it left) lists the profiles that change replaced: keep the
/// ones held until the read the change asked for answers.
fn hold_profiles(s: &State, list: &mut InboxListResponse, epoch: u64) {
    if epoch != s.workspace.inbox.profiles_epoch {
        let held = &s.workspace.inbox.list;
        list.profiles = held.profiles.clone();
        list.last_used_profile_id = held.last_used_profile_id.clone();
    }
}

/// An inbox read answered; `shown` when the person caused it.
fn refreshed(s: &mut State, list: InboxListResponse, shown: bool, tx: &mut Tx) {
    if shown {
        tx.animated();
    }
    apply(s, list);
}

/// Write one mark the person made (pin, unpin, snooze, wake, unsettle,
/// opening a row with a result); the answer is the new list, and its rows
/// move to their new places on `ROW_RESORT` (a reply is a queued update,
/// which Rune lands at once unless it asks for motion).
pub fn mark(s: &mut State, thread_id: String, action: InboxAction, until: Option<i64>) {
    mark_as(s, thread_id, action, until, true);
}

/// [`mark`] for one the app makes on its own (a run seen live as it ended):
/// the rows land where the answer puts them.
pub fn mark_quietly(s: &mut State, thread_id: String, action: InboxAction) {
    mark_as(s, thread_id, action, None, false);
}

fn mark_as(s: &mut State, thread_id: String, action: InboxAction, until: Option<i64>, shown: bool) {
    s.workspace.inbox.snooze_menu = None;
    let epoch = s.workspace.inbox.profiles_epoch;
    jobs::run(
        s,
        "inbox mark",
        move |c| async move { c.inbox_mutate(&thread_id, action, until).await },
        move |s, mut result, tx| {
            if let Ok(list) = result.as_mut() {
                hold_profiles(s, list, epoch);
            }
            marked(s, result, shown, tx)
        },
    );
}

/// A mark answered.
fn marked(s: &mut State, result: Result<InboxListResponse, ClientError>, shown: bool, tx: &mut Tx) {
    match result {
        Ok(list) => {
            if shown {
                tx.animated();
            }
            apply(s, list);
        }
        // silent-ok: an older gateway has no marks; the row stays as it was.
        Err(e) => tracing::debug!(error = %e, "inbox mark failed"),
    }
}

/// Navigate only after the gateway confirms the mark. Never use an old
/// sidebar snapshot to choose a thread that may already be settled.
pub fn settle(s: &mut State, thread_id: String) {
    let id = thread_id.clone();
    let epoch = s.workspace.inbox.profiles_epoch;
    jobs::run(
        s,
        "inbox settle",
        move |c| async move { c.inbox_mutate(&id, InboxAction::Settle, None).await },
        move |s, result, tx| {
            let mut list = match result {
                Ok(list) => list,
                Err(e) => {
                    tracing::warn!(error = %e, "could not settle thread");
                    return;
                }
            };
            hold_profiles(s, &mut list, epoch);
            settled(s, &thread_id, list, tx);
        },
    );
}

/// The gateway confirmed the settle. Only the thread on screen moves the
/// screen: settling a background card (found by search, say) leaves the
/// open chat where it is. The person settled it, so the card leaves and the
/// rows close up on `ROW_RESORT`, and the thread that takes the screen
/// arrives with the usual motion.
fn settled(s: &mut State, thread_id: &str, list: InboxListResponse, tx: &mut Tx) {
    tx.animated();
    let open = s.page == Page::Chat && active_id(s) == Some(thread_id);
    apply(s, list);
    if !open {
        return;
    }
    match next_after_settle(s, thread_id) {
        Some(next) => super::open_thread(s, next),
        None => {
            s.chat = None;
            s.workspace.draft = None;
            draft::start_in_profile(s, DEFAULT_PROFILE_ID.into());
        }
    }
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
    // Expire sent prompts here too: a read that keeps failing never applies.
    s.workspace.inbox.sent.retain(|_, sent| sent.live());
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
    let every = if s.workspace.inbox.sent.is_empty() {
        POLL
    } else {
        SENT_POLL
    };
    s.workspace.timers.inbox = Some(Instant::now() + every);
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
    picker::close(s);
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
        // A thread opened on the folded Working shelf unfolds it, so the
        // selection is never hidden.
        let inbox = &s.workspace.inbox;
        let on_shelf = above.iter().chain(std::iter::once(&id)).any(|t| {
            inbox
                .row(t)
                .is_some_and(|r| !tree.is_nested(t) && inbox.shown_state(r) == InboxState::Working)
        });
        s.workspace.inbox.expanded.extend(above);
        if on_shelf {
            s.workspace.inbox.show_working = true;
        }
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

    fn shelf_of(s: &State, id: &str) -> &'static str {
        let sh = shelves(s);
        if sh.pinned.iter().any(|r| r.thread_id == id) {
            "pinned"
        } else if sh.main.iter().any(|r| r.thread_id == id) {
            "main"
        } else if sh.working.iter().any(|r| r.thread_id == id) {
            "working"
        } else {
            "elsewhere"
        }
    }

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

    fn settled_with(rows: Vec<InboxRow>, side_chats: &[(&str, &str)]) -> State {
        let mut s = crate::chat::fixtures::test_state();
        for (parent, child) in side_chats {
            s.prefs
                .side_chats
                .entry((*parent).to_string())
                .or_default()
                .push((*child).to_string());
        }
        apply(
            &mut s,
            InboxListResponse {
                rows,
                ..Default::default()
            },
        );
        s
    }

    #[test]
    fn settling_the_last_open_row_never_selects_a_parked_thread() {
        let s = settled_with(
            vec![
                row("recent", InboxState::Settled, None),
                row("last", InboxState::Settled, None),
                row("later", InboxState::Snoozed, None),
            ],
            &[],
        );
        assert_eq!(next_after_settle(&s, "last"), None);
        let s = settled_with(
            vec![
                row("old", InboxState::Settled, None),
                row("next", InboxState::Inbox, None),
                row("last", InboxState::Settled, None),
            ],
            &[],
        );
        assert_eq!(next_after_settle(&s, "last"), Some("next".into()));
    }

    /// Settling a background card that a filter found leaves the open
    /// chat alone, even with no card left to show; settling the open one
    /// with nothing left shows a fresh draft.
    #[test]
    fn only_settling_the_open_thread_moves_the_screen() {
        let rows = || {
            vec![
                row("a", InboxState::Inbox, None),
                row("b", InboxState::Settled, None),
            ]
        };
        let mut s = crate::chat::fixtures::test_state();
        s.page = Page::Chat;
        s.chat = Some(crate::chat::ChatState::new("a".into()));
        s.workspace.inbox.profile = Some("elsewhere".into());
        let list = |rows| InboxListResponse {
            rows,
            ..Default::default()
        };
        let mut tx = Tx::default();
        settled(&mut s, "b", list(rows()), &mut tx);
        assert_eq!(active_id(&s), Some("a"));

        let mut last = rows();
        last[0].state = InboxState::Settled;
        settled(&mut s, "a", list(last), &mut tx);
        assert_eq!(active_id(&s), None);
    }

    /// The answers to what the person did move the rows on `ROW_RESORT`:
    /// a reply is a queued update, which a real window lands at once
    /// unless it asks for motion (a headless `Runtime` animates every
    /// commit, so the flag is what shows it). Reads and marks the app
    /// makes on its own land.
    #[test]
    fn only_answers_to_what_the_person_did_move_the_rows() {
        use crate::workspace::asked_motion;
        let list = || InboxListResponse {
            rows: vec![row("a", InboxState::Pinned, None)],
            ..Default::default()
        };
        let mut s = crate::chat::fixtures::test_state();

        let mut tx = Tx::default();
        marked(&mut s, Ok(list()), true, &mut tx);
        assert!(asked_motion(&tx), "a pin, snooze or wake glides");
        let mut tx = Tx::default();
        marked(&mut s, Ok(list()), false, &mut tx);
        assert!(!asked_motion(&tx), "a run seen ending lands");

        let mut tx = Tx::default();
        settled(&mut s, "a", list(), &mut tx);
        assert!(asked_motion(&tx), "a settle glides");

        let mut tx = Tx::default();
        refreshed(&mut s, list(), false, &mut tx);
        assert!(!asked_motion(&tx), "a background read lands");
        let mut tx = Tx::default();
        refreshed(&mut s, list(), true, &mut tx);
        assert!(asked_motion(&tx), "a read the person caused glides");
    }

    /// A side chat (a fork handoff) is an open row the sidebar hides under
    /// its parent: settling the last card shows a fresh draft, never it.
    #[test]
    fn settling_the_last_card_never_opens_a_hidden_side_chat() {
        let s = settled_with(
            vec![
                row("side", InboxState::Inbox, None),
                row("parent", InboxState::Settled, None),
                row("last", InboxState::Settled, None),
            ],
            &[("parent", "side")],
        );
        assert_eq!(next_after_settle(&s, "last"), None);
        assert_eq!(first_open(&s), None);
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

    /// Like t3code: a working thread leaves the list for the Working shelf,
    /// which is folded by default, and comes back when its run ends. A
    /// pinned thread stays pinned.
    #[test]
    fn working_threads_go_to_the_folded_working_shelf() {
        let list = |rows| InboxListResponse {
            rows,
            ..Default::default()
        };
        let mut s = crate::chat::fixtures::test_state();
        apply(
            &mut s,
            list(vec![
                row("a", InboxState::Inbox, None),
                row("p", InboxState::Pinned, None),
            ]),
        );
        apply(
            &mut s,
            list(vec![
                row("a", InboxState::Working, None),
                row("p", InboxState::Pinned, None),
                row("new", InboxState::Working, None),
            ]),
        );
        assert_eq!(shelf_of(&s, "a"), "working", "an inbox card moves down");
        assert_eq!(shelf_of(&s, "new"), "working");
        assert_eq!(shelf_of(&s, "p"), "pinned", "a pinned card stays pinned");
        let sh = shelves(&s);
        assert!(!sh.working_open, "the shelf starts folded");
        assert!(sh.jump.iter().all(|id| id != "a"), "folded rows take no ⌘N");
        // The run ends: back in the list.
        apply(&mut s, list(vec![row("a", InboxState::Inbox, None)]));
        assert_eq!(shelf_of(&s, "a"), "main");
    }

    /// Answering a "Needs you" thread: a read still carrying the old status
    /// is stale, so the card stays on the shelf until the new run shows.
    #[test]
    fn a_stale_needs_you_does_not_bounce_the_card() {
        let list = |rows| InboxListResponse {
            rows,
            ..Default::default()
        };
        let needs = || {
            let mut r = row("a", InboxState::Inbox, None);
            r.status = Some(InboxStatus::NeedsInput);
            r
        };
        let mut s = crate::chat::fixtures::test_state();
        apply(&mut s, list(vec![needs()]));
        note_sent(&mut s, "a");
        apply(&mut s, list(vec![needs()]));
        assert_eq!(shelf_of(&s, "a"), "working");
        let inbox = &s.workspace.inbox;
        assert_eq!(
            inbox.shown_status(inbox.row("a").expect("row")),
            Some(InboxStatus::Working)
        );
        assert!(!s.workspace.inbox.sent.is_empty(), "not confirmed yet");
    }

    /// Opening by itself never lands on a folded, hidden card: a card on
    /// screen comes first, and a thread opened on the shelf unfolds it.
    #[test]
    fn automatic_opening_prefers_cards_on_screen() {
        let mut working = row("w", InboxState::Working, None);
        working.status = Some(InboxStatus::Working);
        let rows = vec![working, row("b", InboxState::Inbox, None)];
        let mut s = crate::chat::fixtures::test_state();
        apply(
            &mut s,
            InboxListResponse {
                rows: rows.clone(),
                ..Default::default()
            },
        );
        assert_eq!(first_open(&s).as_deref(), Some("b"));
        assert_eq!(next_after_settle(&s, "x").as_deref(), Some("b"));
    }

    /// A prompt sent here shows its card as working at once, even when the
    /// inbox read after the send came back before the backend marked the
    /// run (the 2026-11-19 report: "now", no Working, until the run ended).
    /// The inbox polls until it catches up; the run's end clears it.
    #[test]
    fn a_sent_prompt_shows_working_before_the_inbox_catches_up() {
        let list = |rows| InboxListResponse {
            rows,
            ..Default::default()
        };
        let mut s = crate::chat::fixtures::test_state();
        apply(&mut s, list(vec![row("a", InboxState::Inbox, None)]));
        assert!(s.workspace.timers.inbox.is_none(), "nothing to follow yet");

        note_sent(&mut s, "a");
        // The read right after the send: the backend has not seen the run.
        apply(&mut s, list(vec![row("a", InboxState::Inbox, None)]));
        let inbox = &s.workspace.inbox;
        let shown = inbox.shown_status(inbox.row("a").expect("row"));
        assert_eq!(shown, Some(InboxStatus::Working));
        assert_eq!(
            shelf_of(&s, "a"),
            "working",
            "it moves to the shelf at once"
        );
        assert!(
            s.workspace.timers.inbox.is_some(),
            "the inbox keeps polling"
        );
        assert!(s.workspace.timers.tick.is_some(), "the Working timer ticks");

        // The backend catches up: its status rules from then on.
        let mut done = row("a", InboxState::Inbox, None);
        done.status = Some(InboxStatus::Working);
        apply(&mut s, list(vec![done]));
        assert!(s.workspace.inbox.sent.is_empty());
        apply(&mut s, list(vec![row("a", InboxState::Inbox, None)]));

        // The grace runs out on its own clock, even with no inbox read.
        note_sent(&mut s, "b");
        if let Some(sent) = s.workspace.inbox.sent.get_mut("b") {
            sent.at = Instant::now() - SENT_GRACE;
        }
        assert!(!s.workspace.inbox.any_sent_live());
        poll_due(&mut s);
        assert!(s.workspace.inbox.sent.is_empty(), "the poll drops it");

        // A run that ends before the backend ever showed it clears too.
        note_sent(&mut s, "a");
        crate::workspace::run_ended(&mut s, "a".into(), false);
        let inbox = &s.workspace.inbox;
        assert_eq!(inbox.shown_status(inbox.row("a").expect("row")), None);
    }

    /// A pin answered after a profile was archived here carries the
    /// profiles from before: it must not put the archived one back.
    #[test]
    fn a_mark_answered_after_an_archive_does_not_revive_it() {
        use crate::jobs::harness::Pump;
        use crate::test_gateway::{Answer, Gateway};

        let profile = |id: &str| InboxProfile {
            profile_id: id.into(),
            display_name: id.into(),
            git_branch: None,
            last_used_at: None,
        };
        let body = |ids: &[&str], last: &str| {
            serde_json::to_string(&InboxListResponse {
                rows: vec![row("a", InboxState::Pinned, None)],
                profiles: ids.iter().map(|id| profile(id)).collect(),
                last_used_profile_id: Some(last.into()),
            })
            .expect("json")
        };
        let gateway = Gateway::start(vec![
            Answer::ok(body(&["default", "work"], "work")).after(Duration::from_millis(1_500)),
            Answer::ok(body(&["default"], "default")),
        ]);
        let mut s = crate::chat::fixtures::test_state();
        s.rt.backend = Some(gateway.backend());
        let mut pump = Pump::install(&mut s);
        s.workspace.inbox.list.profiles = vec![profile("default"), profile("work")];
        s.workspace.inbox.list.last_used_profile_id = Some("work".into());

        mark(&mut s, "a".into(), InboxAction::Pin, None);
        let asked = Instant::now();
        while gateway.paths().is_empty() {
            assert!(asked.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(5));
        }
        profiles_changed(&mut s, ProfileChange::Deleted("work"));

        let ids = |s: &State| {
            s.workspace
                .inbox
                .list
                .profiles
                .iter()
                .map(|p| p.profile_id.clone())
                .collect::<Vec<_>>()
        };
        let mut revived = false;
        // Done once the pin's own answer (its pinned row) has landed.
        assert!(pump.until(&mut s, Duration::from_secs(10), |s| {
            revived |= ids(s).iter().any(|id| id == "work");
            s.workspace.inbox.row("a").is_some() && gateway.paths().len() >= 2
        }));
        pump.run_for(&mut s, Duration::from_millis(1_700));
        revived |= ids(&s).iter().any(|id| id == "work");
        assert!(!revived, "the pin's old snapshot put Work back");
        assert_eq!(
            s.workspace.inbox.list.last_used_profile_id.as_deref(),
            Some("default")
        );
    }
}
