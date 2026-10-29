//! The sidebar inbox (t3code's): one list by recency with search and
//! filters (profile, model, state), Pinned on top, the Working, Snoozed (N)
//! and Settled (N) shelves at the bottom (a working thread that waits on an
//! approval or sign-in comes back up to the inbox, marked "Needs you"), and the profile menu of the
//! new-thread screen.
//!
//! The backend joins and classifies every row (`GET /inbox`), so this file
//! only renders and sends marks. Refresh is event-driven: on connect, when
//! the window comes to the front, after a run or a mark. While a row is
//! working, a slow poll follows it, only with the window in front. A snooze
//! or usage limit that ends sets one timer for that moment; a working row
//! repaints its timer once a second; an idle inbox schedules nothing.

use std::collections::BTreeSet;
use std::time::Duration;

use chrono::{Datelike, Duration as Span, Local, TimeZone, Utc};
use ironclaw_gpui_client::inbox::{
    InboxAction, InboxEngine, InboxListResponse, InboxProfile, InboxRow, InboxState, InboxStatus,
};
use ironclaw_gpui_client::profiles::DEFAULT_PROFILE_ID;

use super::{Page, Workspace, sidebar_row};
use crate::assets::ExtraIcon;
use crate::design::{self, Palette, Style, kit};
use crate::jobs;
use crate::ui::*;

/// One context-menu entry's effect on a row, given its thread id.
type RowAction = Box<dyn Fn(&mut Workspace, String, &mut Window, &mut Context<Workspace>)>;

/// Poll period while a row is working.
const POLL: Duration = Duration::from_secs(5);
/// How long the inbox keeps polling after a run ends.
const TITLE_SETTLE_MS: i64 = 20_000;
/// Settled rows shown before "Show more".
const SETTLED_PAGE: usize = 30;

#[derive(Default)]
pub(super) struct Inbox {
    pub(super) list: InboxListResponse,
    pub(super) search: Option<(Entity<InputState>, Subscription)>,
    filters_open: bool,
    /// The new-thread button's profile menu.
    pub(super) new_menu_open: bool,
    profile: Option<String>,
    model: Option<String>,
    state: Option<InboxState>,
    /// Folded state of the Working shelf; open by default, as in t3code.
    show_working: bool,
    show_snoozed: bool,
    show_settled: bool,
    settled_limit: usize,
    snooze_menu: Option<String>,
    /// Rows marked unread from the context menu. Local to this app run:
    /// the backend has no read flag; opening the thread clears it.
    unread: BTreeSet<String>,
    /// Keep polling until then (Unix ms) after a run ends: the backend
    /// writes the thread's generated title a few seconds later.
    settle_poll_until: i64,
    loading: bool,
    stale: bool,
    polling: bool,
    /// Rows on screen at first load draw settled; later ones grow in.
    seeded: bool,
    /// ⌘ alone is held: cards show their `⌘N` jump badge.
    pub(super) cmd_held: bool,
    /// Thread ids ⌘1..⌘9 open, in the order the cards show.
    pub(super) jump: Vec<String>,
    _wake: Option<Task<()>>,
    /// Repaints the Working timers once a second while a row works.
    _tick: Option<Task<()>>,
}

impl Inbox {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        let sub = cx.subscribe_in(&input, window, |_, _, event, _, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        });
        Self {
            search: Some((input, sub)),
            show_working: true,
            settled_limit: SETTLED_PAGE,
            ..Self::default()
        }
    }

    pub(super) fn row(&self, thread_id: &str) -> Option<&InboxRow> {
        self.list.rows.iter().find(|r| r.thread_id == thread_id)
    }

    /// The thread to open first: the top open row. Never a parked one; with
    /// none open the caller shows the new-thread screen.
    pub(super) fn first_open(&self) -> Option<String> {
        self.list
            .rows
            .iter()
            .find(|r| !matches!(r.state, InboxState::Snoozed | InboxState::Settled))
            .map(|r| r.thread_id.clone())
    }

    fn profile(&self, id: &str) -> Option<&InboxProfile> {
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

    fn query(&self, cx: &App) -> String {
        self.search
            .as_ref()
            .map(|(input, _)| input.read(cx).value().trim().to_lowercase())
            .unwrap_or_default()
    }

    fn matches(&self, row: &InboxRow, query: &str) -> bool {
        if self.profile.as_ref().is_some_and(|p| *p != row.profile_id)
            || self.state.is_some_and(|s| s != row.state)
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

    fn models(&self) -> Vec<String> {
        let set: BTreeSet<String> = self
            .list
            .rows
            .iter()
            .flat_map(|r| r.engines.iter().filter_map(|e| e.model.clone()))
            .collect();
        set.into_iter().collect()
    }
}

/// Choose only a confirmed open row; an empty result always means a fresh draft.
fn next_after_settle(rows: &[InboxRow], settled: &str) -> Option<String> {
    rows.iter()
        .filter(|r| r.state == InboxState::Pinned)
        .chain(
            rows.iter()
                .filter(|r| matches!(r.state, InboxState::Inbox | InboxState::Working)),
        )
        .find(|r| r.thread_id != settled)
        .map(|r| r.thread_id.clone())
}

pub(crate) fn title_of(row: &InboxRow) -> &str {
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

impl Workspace {
    /// Take a fresh list: titles cache, poll, wake timer.
    pub(super) fn apply_inbox(&mut self, list: InboxListResponse, cx: &mut Context<Self>) {
        crate::titles::set_rows(&list.rows, cx);
        crate::run_clock::set_rows(&list.rows, cx);
        // A thread that enters or leaves the Working shelf grows into its
        // new place instead of jumping there.
        let was_working = |id: &str| self.inbox.row(id).map(|r| r.state == InboxState::Working);
        // Any live status counts: a run can go Needs input → Done between polls.
        let was_running = |id: &str| {
            self.inbox.row(id).is_some_and(|r| {
                matches!(
                    r.status,
                    Some(InboxStatus::Working | InboxStatus::Background | InboxStatus::NeedsInput)
                )
            })
        };
        let mut finished = false;
        for row in &list.rows {
            if was_working(&row.thread_id)
                .is_some_and(|was| was != (row.state == InboxState::Working))
            {
                crate::design::motion::restart(&format!("card-grow-{}", row.thread_id));
            }
            finished |= was_running(&row.thread_id) && row.status != Some(InboxStatus::Working);
        }
        self.inbox.list = list;
        if finished {
            self.poll_for_title();
        }
        self.ensure_inbox_poll(cx);
        self.schedule_wake(cx);
        self.ensure_tick(cx);
        cx.notify();
    }

    fn ensure_tick(&mut self, cx: &mut Context<Self>) {
        if !self.inbox.any_working() {
            self.inbox._tick = None;
            return;
        }
        if self.inbox._tick.is_some() {
            return;
        }
        self.inbox._tick = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                // Hidden, or the last cached run ended: stop. Activation
                // refreshes the inbox, which starts the tick again.
                let keep = this.update(cx, |this, cx| {
                    if cx.active_window().is_none() || !this.inbox.any_working() {
                        this.inbox._tick = None;
                        return false;
                    }
                    cx.notify();
                    true
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
            }
        }));
    }

    /// A run ended: poll a little longer to pick up its generated title.
    pub(super) fn poll_for_title(&mut self) {
        self.inbox.settle_poll_until = Utc::now().timestamp_millis() + TITLE_SETTLE_MS;
    }

    /// Re-read the inbox. Coalesces: a call during a read marks it stale.
    pub(super) fn refresh_inbox(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        if self.inbox.loading {
            self.inbox.stale = true;
            return;
        }
        self.inbox.loading = true;
        jobs::run(
            &backend,
            cx,
            |c| async move { c.inbox().await },
            |this, result, cx| {
                this.inbox.loading = false;
                match result {
                    Ok(list) => this.apply_inbox(list, cx),
                    // silent-ok: the list keeps its last state; the next event retries.
                    Err(e) => tracing::debug!(error = %e, "inbox refresh failed"),
                }
                if std::mem::take(&mut this.inbox.stale) {
                    this.refresh_inbox(cx);
                }
            },
        );
    }

    /// Write one mark; the answer is the new list.
    pub(super) fn inbox_mark(
        &mut self,
        thread_id: String,
        action: InboxAction,
        until: Option<i64>,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        self.inbox.snooze_menu = None;
        jobs::run(
            &backend,
            cx,
            move |c| async move { c.inbox_mutate(&thread_id, action, until).await },
            |this, result, cx| match result {
                Ok(list) => this.apply_inbox(list, cx),
                // silent-ok: an older gateway has no marks; the row stays as it was.
                Err(e) => tracing::debug!(error = %e, "inbox mark failed"),
            },
        );
    }

    /// Navigate only after the gateway confirms the mark. Never use an old
    /// sidebar snapshot to choose a thread that may already be settled.
    fn settle(&mut self, thread_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let id = thread_id.clone();
        jobs::run_in(
            &backend,
            window,
            cx,
            move |c| async move { c.inbox_mutate(&id, InboxAction::Settle, None).await },
            move |this, result, window, cx| {
                let list = match result {
                    Ok(list) => list,
                    Err(e) => {
                        tracing::warn!(error = %e, "could not settle thread");
                        return;
                    }
                };
                let next = next_after_settle(&list.rows, &thread_id);
                let open = this.page == Page::Chat
                    && this.active.as_ref().is_some_and(|(id, _)| *id == thread_id);
                this.apply_inbox(list, cx);
                if next.is_none() {
                    this.active = None;
                    this._chat_events.clear();
                    this.draft = None;
                    this.start_in_profile(DEFAULT_PROFILE_ID.into(), window, cx);
                } else if open && let Some(next) = next {
                    this.open_thread(next, window, cx);
                }
            },
        );
    }

    /// Opening a row with an unseen result clears its Done / Woke badge.
    pub(super) fn visit(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        self.inbox.unread.remove(thread_id);
        let unseen = self
            .inbox
            .row(thread_id)
            .is_some_and(|r| matches!(r.status, Some(InboxStatus::Done | InboxStatus::Woke)));
        if unseen {
            self.inbox_mark(thread_id.to_string(), InboxAction::Visit, None, cx);
        }
    }

    fn ensure_inbox_poll(&mut self, cx: &mut Context<Self>) {
        if self.inbox.polling || !self.inbox.needs_poll() {
            return;
        }
        self.inbox.polling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let keep = this.update(cx, |this, cx| {
                    if !this.inbox.needs_poll() {
                        this.inbox.polling = false;
                        return false;
                    }
                    // In the background nothing is watching; activation refreshes.
                    if cx.active_window().is_some() {
                        this.refresh_inbox(cx);
                    }
                    true
                });
                if !matches!(keep, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    fn schedule_wake(&mut self, cx: &mut Context<Self>) {
        let now = Utc::now().timestamp_millis();
        self.inbox._wake = self.inbox.next_wake(now).map(|at| {
            let wait = Duration::from_millis((at - now).max(0) as u64);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                let _ = this.update(cx, |this, cx| this.refresh_inbox(cx));
            })
        });
    }

    fn new_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.inbox.new_menu_open = false;
        if let Some(draft) = self.draft.as_mut() {
            draft.menu_open = false;
        }
        self.show(Page::Profiles, window, cx);
        if let Some(view) = self.pages.profiles.clone() {
            view.update(cx, |v, cx| v.new_profile(window, cx));
        }
    }

    /// Deleting from the menu asks first: a right-click misfires more than
    /// the settled row's trash button.
    fn confirm_delete(&mut self, thread_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let title = self
            .inbox
            .row(&thread_id)
            .map_or("this conversation", title_of)
            .to_string();
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete \"{title}\"?"),
            Some("The conversation leaves the inbox and can't be opened again."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if matches!(answer.await, Ok(0)) {
                let _ = this.update_in(cx, |this, window, cx| {
                    this.delete_thread(thread_id, window, cx)
                });
            }
        })
        .detach();
    }

    /// The right-click menu of an inbox row. Every mark goes through the
    /// same calls as the hover buttons; "unread" is the local set.
    fn row_menu(
        &self,
        row: &InboxRow,
        cx: &mut Context<Self>,
    ) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
        let weak = cx.entity().downgrade();
        let id = row.thread_id.clone();
        let title = title_of(row).to_string();
        let state = row.state;
        let blocker = settle_blocker(row);
        let unread = self.inbox.unread.contains(&id);
        move |menu, window, cx| {
            let act = |label: &str, icon: Icon, f: RowAction| {
                let (weak, id) = (weak.clone(), id.clone());
                PopupMenuItem::new(label.to_string())
                    .icon(icon)
                    .on_click(move |_, window, cx| {
                        let _ = weak.update(cx, |this, cx| f(this, id.clone(), window, cx));
                    })
            };
            let mark = |label: &str, icon: Icon, action: InboxAction| {
                act(
                    label,
                    icon,
                    Box::new(move |this, id, _, cx| this.inbox_mark(id, action, None, cx)),
                )
            };
            let copy = |label: &str, text: String| {
                PopupMenuItem::new(label.to_string())
                    .icon(Icon::new(IconName::Copy))
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
                    })
            };
            let mut menu = menu.item(act(
                "Open",
                Icon::new(IconName::ExternalLink),
                Box::new(|this, id, window, cx| this.open_thread(id, window, cx)),
            ));
            menu = match state {
                InboxState::Snoozed => menu.item(mark(
                    "Wake now",
                    Icon::new(ExtraIcon::Bell),
                    InboxAction::Wake,
                )),
                InboxState::Settled => menu.item(mark(
                    "Un-settle",
                    Icon::new(IconName::Undo2),
                    InboxAction::Unsettle,
                )),
                _ => {
                    let pin = if state == InboxState::Pinned {
                        mark("Unpin", Icon::new(ExtraIcon::PinOff), InboxAction::Unpin)
                    } else {
                        mark("Pin", Icon::new(ExtraIcon::Pin), InboxAction::Pin)
                    };
                    let (sweak, sid) = (weak.clone(), id.clone());
                    menu.item(pin)
                        .submenu_with_icon(
                            Some(Icon::new(ExtraIcon::AlarmClock)),
                            "Snooze",
                            window,
                            cx,
                            move |sub, _, _| {
                                snooze_presets()
                                    .into_iter()
                                    .fold(sub, |sub, (label, until)| {
                                        let (weak, id) = (sweak.clone(), sid.clone());
                                        sub.item(PopupMenuItem::new(label).on_click(
                                            move |_, _, cx| {
                                                let _ = weak.update(cx, |this, cx| {
                                                    this.inbox_mark(
                                                        id.clone(),
                                                        InboxAction::Snooze,
                                                        Some(until),
                                                        cx,
                                                    )
                                                });
                                            },
                                        ))
                                    })
                            },
                        )
                        .item(
                            act(
                                "Settle",
                                Icon::new(IconName::Check),
                                Box::new(|this, id, window, cx| this.settle(id, window, cx)),
                            )
                            .disabled(blocker.is_some()),
                        )
                }
            };
            menu.item(act(
                if unread {
                    "Mark as read"
                } else {
                    "Mark as unread"
                },
                Icon::new(IconName::Eye),
                Box::new(move |this, id, _, cx| {
                    if !this.inbox.unread.remove(&id) {
                        this.inbox.unread.insert(id);
                    }
                    cx.notify();
                }),
            ))
            .separator()
            .item(copy("Copy title", title.clone()))
            .item(copy("Copy thread ID", id.clone()))
            .separator()
            .item(
                act(
                    "Delete",
                    Icon::new(IconName::Delete),
                    Box::new(|this, id, window, cx| this.confirm_delete(id, window, cx)),
                )
                .disabled(state == InboxState::Working),
            )
        }
    }

    // ── render ──────────────────────────────────────────────────────────

    pub(super) fn render_inbox(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        let query = self.inbox.query(cx);
        let active = (self.page == Page::Chat)
            .then(|| self.active.as_ref().map(|(id, _)| id.clone()))
            .flatten();
        let rows: Vec<InboxRow> = self
            .inbox
            .list
            .rows
            .iter()
            .filter(|r| self.inbox.matches(r, &query))
            .cloned()
            .collect();
        let in_state = |s: &[InboxState]| -> Vec<&InboxRow> {
            rows.iter().filter(|r| s.contains(&r.state)).collect()
        };
        // t3code's Working shelf: a thread with a live run (or live
        // subagents) leaves the inbox while it works and comes back when it
        // needs you. Pinned outranks it, as in t3code.
        let working = in_state(&[InboxState::Working]);
        let pinned = in_state(&[InboxState::Pinned]);
        let main = in_state(&[InboxState::Inbox]);
        let snoozed = in_state(&[InboxState::Snoozed]);
        let settled = in_state(&[InboxState::Settled]);
        // Searching or filtering on a parked state unfolds it.
        let unfold = !query.is_empty() || self.inbox.state.is_some();
        let now = Utc::now().timestamp_millis();

        let mut list: Vec<AnyElement> = Vec::new();
        let enter = Style::of(cx).enter;
        let reduced = cx.reduce_motion();
        let seeding = !self.inbox.seeded && !self.inbox.list.rows.is_empty();
        if seeding {
            self.inbox.seeded = true;
            for row in &self.inbox.list.rows {
                crate::design::motion::settle(&format!("card-grow-{}", row.thread_id));
            }
        }
        // ⌘1..⌘9 follow the full cards top to bottom, Working last when
        // its shelf is open, as t3code numbers its visible threads.
        let working_open = self.inbox.show_working || unfold;
        self.inbox.jump = pinned
            .iter()
            .chain(main.iter())
            .chain(working.iter().filter(|_| working_open))
            .take(9)
            .map(|r| r.thread_id.clone())
            .collect();
        let badge = |id: &str, inbox: &Inbox| {
            inbox
                .cmd_held
                .then(|| inbox.jump.iter().position(|j| j == id))
                .flatten()
        };
        // A new-thread draft sits on top until it is sent, then a rule.
        if let Some(card) = self.render_new_draft(cx) {
            list.push(card);
            list.push(
                div()
                    .mx(px(12.))
                    .my(px(6.))
                    .h_px()
                    .bg(p.border)
                    .into_any_element(),
            );
        }
        let cards: Vec<&InboxRow> = pinned.iter().chain(main.iter()).copied().collect();
        for (ix, row) in cards.into_iter().enumerate() {
            let is_active = active.as_deref() == Some(row.thread_id.as_str());
            let jump = badge(&row.thread_id, &self.inbox);
            let card = self.render_card(row, is_active, jump, now, cx);
            // A thread that appears later (a new one, a T3 child) grows
            // into place, Zeron's 180 ms collapse run in reverse.
            let card = crate::design::motion::grow(
                &format!("card-grow-{}", row.thread_id),
                crate::design::motion::COLLAPSE,
                reduced,
                card,
            );
            // Cards cascade in when they first mount (app start, a filter
            // change); a mounted card never replays.
            // A resort (new activity lifts a thread) glides the cards to
            // their new places instead of swapping them in one frame.
            list.push(
                slide(
                    &row.thread_id,
                    crate::design::motion::fade_in_after(
                        div().w_full().child(card),
                        SharedString::from(format!("card-in-{}", row.thread_id)),
                        enter,
                        4.,
                        (ix.min(12) * 28) as u64,
                    ),
                )
                .into_any_element(),
            );
        }
        if main.is_empty()
            && pinned.is_empty()
            && working.is_empty()
            && self.inbox.list.rows.is_empty()
        {
            list.push(
                div()
                    .px_2()
                    .py_3()
                    .text_xs()
                    .text_color(p.text_faint)
                    .child("No conversations yet.")
                    .into_any_element(),
            );
        }
        // Like t3code: Working, Snoozed and Settled sit at the bottom of
        // the sidebar and unfold downward, scrolling with the list. Working
        // keeps full cards; the parked shelves use slim rows.
        let mut pushed = false;
        if !working.is_empty() {
            pushed = true;
            list.push(div().flex_1().min_h_2().into_any_element());
            list.push(self.render_fold("working", "Working", working.len(), working_open, cx));
            if working_open {
                for row in &working {
                    let is_active = active.as_deref() == Some(row.thread_id.as_str());
                    let jump = badge(&row.thread_id, &self.inbox);
                    let card = self.render_card(row, is_active, jump, now, cx);
                    list.push(
                        slide(
                            &row.thread_id,
                            crate::design::motion::grow(
                                &format!("card-grow-{}", row.thread_id),
                                crate::design::motion::COLLAPSE,
                                reduced,
                                card,
                            ),
                        )
                        .into_any_element(),
                    );
                }
            }
        }
        let sections = [
            (
                "snoozed",
                "Snoozed",
                snoozed,
                self.inbox.show_snoozed || unfold,
                usize::MAX,
            ),
            (
                "settled",
                "Settled",
                settled,
                self.inbox.show_settled || unfold,
                self.inbox.settled_limit,
            ),
        ];
        for (key, label, items, open, limit) in sections {
            if items.is_empty() {
                continue;
            }
            if !std::mem::replace(&mut pushed, true) {
                list.push(div().flex_1().min_h_2().into_any_element());
            }
            list.push(self.render_fold(key, label, items.len(), open, cx));
            if open {
                for row in items.iter().take(limit) {
                    let is_active = active.as_deref() == Some(row.thread_id.as_str());
                    list.push(
                        slide(&row.thread_id, self.render_slim(row, is_active, now, cx))
                            .into_any_element(),
                    );
                }
                if items.len() > limit {
                    list.push(
                        Button::new("settled-more")
                            .icon(IconName::ChevronDown)
                            .label("Show more")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.inbox.settled_limit += SETTLED_PAGE;
                                cx.notify();
                            }))
                            .into_any_element(),
                    );
                }
            }
        }

        // The profile picker floats over the list so no thread row moves.
        v_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .child(self.render_inbox_header(cx))
            .when(self.inbox.new_menu_open, |d| {
                d.child(gpui_kit::deferred(
                    div()
                        .absolute()
                        .top(px(design::SIDEBAR_SEARCH_HEIGHT))
                        .left_0()
                        .right_0()
                        .occlude()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.inbox.new_menu_open = false;
                            cx.notify();
                        }))
                        .child(self.render_picker(cx)),
                ))
            })
            .when(self.inbox.filters_open, |d| {
                d.child(self.render_filters(cx))
            })
            .child(
                v_flex()
                    .id("inbox-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .px_2()
                    .pb_2()
                    .gap_1()
                    .child(crate::design::flip::anchor(SIDEBAR_GROUP))
                    .children(list),
            )
    }

    fn render_inbox_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filtered = self.inbox.profile.is_some()
            || self.inbox.model.is_some()
            || self.inbox.state.is_some();
        h_flex()
            .h(px(design::SIDEBAR_SEARCH_HEIGHT))
            .flex_none()
            .px_3()
            .gap_1()
            .items_center()
            .children(self.inbox.search.as_ref().map(|(input, _)| {
                // T3 Code's search: a magnifier and the text, no box.
                div()
                    .flex_1()
                    .text_size(px(design::SIDEBAR_TITLE_SIZE))
                    .child(
                        Input::new(input)
                            .appearance(false)
                            .prefix(Icon::new(IconName::Search).size(px(16.))),
                    )
            }))
            .child(
                Button::new("inbox-filters")
                    .icon(ExtraIcon::ListFilter)
                    .ghost()
                    .large()
                    .icon_size(19.)
                    .selected(self.inbox.filters_open || filtered)
                    .tooltip("Filter by profile, model or state")
                    .accessibility_label("Filters")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.inbox.filters_open = !this.inbox.filters_open;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("new-thread")
                    .icon(ExtraIcon::SquarePen)
                    .ghost()
                    .large()
                    // The pen fills its box; at the filter's size it outweighs it.
                    .icon_size(16.)
                    .selected(self.inbox.new_menu_open)
                    .tooltip("New thread")
                    .accessibility_label("New thread")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.inbox.new_menu_open = !this.inbox.new_menu_open;
                        cx.notify();
                    })),
            )
    }

    fn render_filters(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        let i = &self.inbox;
        let group = |label: &'static str| {
            div()
                .pt_1()
                .text_xs()
                .font_medium()
                .text_color(p.text_faint)
                .child(label)
        };
        let profiles = i.list.profiles.iter().enumerate().map(|(ix, profile)| {
            let id = profile.profile_id.clone();
            let on = i.profile.as_deref() == Some(id.as_str());
            chip(("f-profile", ix), profile.display_name.clone(), on, cx)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let pick = Some(id.clone());
                    this.inbox.profile = if this.inbox.profile == pick {
                        None
                    } else {
                        pick
                    };
                    cx.notify();
                }))
                .into_any_element()
        });
        let models = i.models().into_iter().enumerate().map(|(ix, model)| {
            let on = i.model.as_deref() == Some(model.as_str());
            let pick = model.clone();
            chip(("f-model", ix), model, on, cx)
                .on_click(cx.listener(move |this, _, _, cx| {
                    let pick = Some(pick.clone());
                    this.inbox.model = if this.inbox.model == pick { None } else { pick };
                    cx.notify();
                }))
                .into_any_element()
        });
        let states = [
            (InboxState::Pinned, "Pinned"),
            (InboxState::Inbox, "Inbox"),
            (InboxState::Working, "Working"),
            (InboxState::Snoozed, "Snoozed"),
            (InboxState::Settled, "Settled"),
        ]
        .into_iter()
        .enumerate()
        .map(|(ix, (state, label))| {
            let on = i.state == Some(state);
            chip(("f-state", ix), label.to_string(), on, cx)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.inbox.state = if this.inbox.state == Some(state) {
                        None
                    } else {
                        Some(state)
                    };
                    cx.notify();
                }))
                .into_any_element()
        });
        let pop = kit::popover("inbox-filters", cx);
        pop(v_flex()
            .mx_2()
            .mb_1()
            .p_2()
            .gap_1()
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .child(group("Profile"))
                    .child(
                        Button::new("filters-new-profile")
                            .icon(IconName::Plus)
                            .label("New profile")
                            .ghost()
                            .xsmall()
                            .on_click(
                                cx.listener(|this, _, window, cx| this.new_profile(window, cx)),
                            ),
                    ),
            )
            .child(h_flex().flex_wrap().gap_1().children(profiles))
            .when(!i.models().is_empty(), |d| {
                d.child(group("Model"))
                    .child(h_flex().flex_wrap().gap_1().children(models))
            })
            .child(group("State"))
            .child(h_flex().flex_wrap().gap_1().children(states)))
    }

    /// The new-thread screen's profile menu; the current pick is highlighted.
    pub(super) fn render_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        let last = self.inbox.list.last_used_profile_id.clone();
        let chosen = self.draft.as_ref().map(|d| d.profile.clone());
        let mut profiles = self.inbox.list.profiles.clone();
        if profiles.is_empty() {
            profiles.push(InboxProfile {
                profile_id: DEFAULT_PROFILE_ID.into(),
                display_name: "Default".into(),
                git_branch: None,
                last_used_at: None,
            });
        }
        let rows = profiles.into_iter().enumerate().map(|(ix, profile)| {
            let highlighted = chosen.as_deref() == Some(profile.profile_id.as_str());
            let is_last = last.as_deref() == Some(profile.profile_id.as_str());
            let id = profile.profile_id.clone();
            h_flex()
                .id(("pick-profile", ix))
                .role(Role::Button)
                .aria_label(format!("Start in {}", profile.display_name))
                .gap_2()
                .px_2()
                .py_1()
                .rounded(px(design::CONTROL_RADIUS))
                .text_sm()
                .cursor_pointer()
                .when(highlighted, |d| d.bg(p.accent_wash).text_color(p.text))
                .when(!highlighted, |d| {
                    kit::smooth_bg(d, format!("pick-profile-{ix}"), cx)
                })
                .child(profile_mark(
                    &profile.profile_id,
                    &profile.display_name,
                    1.,
                    cx,
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(profile.display_name.clone()),
                )
                .when_some(profile.git_branch.clone(), |d, branch| {
                    d.child(branch_label(branch, cx))
                })
                .when(is_last, |d| {
                    d.child(div().text_xs().text_color(p.text_faint).child("Last used"))
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.start_in_profile(id.clone(), window, cx)
                }))
        });
        let pop = kit::popover("profile-picker", cx);
        pop(v_flex()
            .mx_2()
            .mb_1()
            .p_1()
            .gap_px()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .font_medium()
                    .text_color(p.text_faint)
                    .child("Start in profile"),
            )
            .children(rows)
            .child(
                Button::new("picker-new-profile")
                    .icon(IconName::Plus)
                    .label("New profile")
                    .ghost()
                    .xsmall()
                    .on_click(cx.listener(|this, _, window, cx| this.new_profile(window, cx))),
            ))
    }

    fn render_fold(
        &self,
        key: &'static str,
        label: &'static str,
        count: usize,
        open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        // Neutral headings: color belongs to the rows' real states.
        let (color, hover) = (p.text_muted, p.text);
        h_flex()
            .id(SharedString::from(format!("fold-{key}")))
            .role(Role::Button)
            .aria_label(format!("{label} {count}"))
            .flex_none()
            .h_9()
            .px_2()
            .gap_2()
            .items_center()
            .text_size(px(design::SIDEBAR_META_SIZE + 1.))
            .font_medium()
            .text_color(color)
            .cursor_pointer()
            .hover(|s| s.text_color(hover))
            .child(label)
            .child(div().text_color(p.text_faint).child(count.to_string()))
            .child(div().flex_1())
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall()
                .text_color(p.text_faint),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                match key {
                    "working" => this.inbox.show_working = !this.inbox.show_working,
                    "snoozed" => this.inbox.show_snoozed = !this.inbox.show_snoozed,
                    _ => this.inbox.show_settled = !this.inbox.show_settled,
                }
                cx.notify();
            }))
            .into_any_element()
    }

    /// The unsent new-thread draft as a card: pen, the profile it was
    /// started in, its first line. A click reopens the new-thread screen.
    fn render_new_draft(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (line, saved_profile) = crate::drafts::preview(cx, crate::drafts::NEW_THREAD)?;
        let p = *Palette::of(cx);
        let style = *Style::of(cx);
        let active = self.page == Page::Draft;
        // The open screen's profile is live; a closed draft keeps its own.
        let profile_id = self
            .draft
            .as_ref()
            .map(|d| d.profile.clone())
            .or(saved_profile)
            .unwrap_or_else(|| DEFAULT_PROFILE_ID.into());
        let profile = self.inbox.profile(&profile_id);
        let name = profile.map_or_else(|| profile_id.clone(), |p| p.display_name.clone());
        let branch = profile.and_then(|p| p.git_branch.clone());
        let h = crate::design::motion::hover_t("card:new-draft");
        let wash =
            crate::design::motion::mix(kit::draft_wash(&p), kit::draft_wash(&p).opacity(0.14), h);
        let wash = if active {
            crate::design::motion::mix(wash, kit::selected_wash(&style, &p), 0.5)
        } else {
            wash
        };
        let card = v_flex()
            .id("row-new-draft")
            .group("new-draft")
            .role(Role::Button)
            .aria_label(format!("Draft: {line}"))
            .w_full()
            .min_h(px(design::SIDEBAR_CARD_MIN_HEIGHT))
            .justify_center()
            .px(px(12.))
            .py(px(14.))
            .gap(px(6.))
            .rounded(px(8.))
            .cursor_pointer()
            .bg(wash)
            .on_hover(crate::design::motion::hover_listener(
                "card:new-draft".into(),
                style.hover,
            ))
            .on_click(cx.listener(|this, _, window, cx| this.open_draft(window, cx)))
            .child(
                h_flex()
                    .gap(px(7.))
                    .items_center()
                    .text_size(px(design::SIDEBAR_META_SIZE))
                    .text_color(p.text_muted)
                    .child(draft_mark(cx))
                    .child(profile_mark(
                        &profile_id,
                        &name,
                        h.max(active as u8 as f32),
                        cx,
                    ))
                    .child(div().flex_1().min_w_0().truncate().child(name))
                    .when_some(branch, |d, b| d.child(branch_label(b, cx)))
                    .child(
                        div()
                            .invisible()
                            .group_hover("new-draft", |s| s.visible())
                            .child(
                                Button::new("discard-new-draft")
                                    .icon(IconName::Close)
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Discard draft")
                                    .accessibility_label("Discard draft")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        cx.stop_propagation();
                                        this.discard_new_draft(window, cx)
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .truncate()
                    .text_size(px(design::SIDEBAR_TITLE_SIZE))
                    .text_color(p.text)
                    .child(line),
            );
        Some(card.into_any_element())
    }

    /// Card row: profile · branch with the status or age, then the title;
    /// hover actions pin, snooze, settle. No model readout: the profile names it.
    fn render_card(
        &self,
        row: &InboxRow,
        active: bool,
        jump: Option<usize>,
        now: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        let id = row.thread_id.clone();
        let profile = self.inbox.profile(&row.profile_id);
        let name = profile.map_or_else(|| row.profile_id.clone(), |p| p.display_name.clone());
        let branch = profile.and_then(|p| p.git_branch.clone());
        let group: SharedString = format!("card-{id}").into();
        let pinned = row.state == InboxState::Pinned;
        let menu_open = self.inbox.snooze_menu.as_deref() == Some(id.as_str());
        let unread = self.inbox.unread.contains(&id);
        let recede = !active && row.status.is_none() && !unread;
        let style = *Style::of(cx);
        let hover_key: SharedString = format!("card:{id}").into();
        let h = crate::design::motion::hover_t(&hover_key);
        let sel = crate::design::motion::state_t(
            &format!("card:{id}:on"),
            active,
            style.slide,
            cx.reduce_motion(),
        );
        // Zeron's session row: the wash alone marks the selection and the
        // selected row never drifts toward the hover wash.
        // An unsent draft waits here: t3code's pen mark and warm tint.
        let drafted = !active && crate::drafts::has(cx, &id);
        let (rest, hover) = if drafted {
            (kit::draft_wash(&p), kit::draft_wash(&p).opacity(0.14))
        } else {
            (kit::hover_wash(&p).opacity(0.), kit::hover_wash(&p))
        };
        let wash = crate::design::motion::mix(
            crate::design::motion::mix(rest, hover, h),
            kit::selected_wash(&style, &p),
            sel,
        );
        // Read titles sit at 75 % and brighten on hover; active and
        // unread ones are full ink.
        let ink = if recede {
            crate::design::motion::mix(p.text.opacity(0.75), p.text, h)
        } else {
            p.text
        };

        // Like t3code: on hover the actions take the status slot's place.
        let blocker = settle_blocker(row);
        let actions = {
            let (pin_id, snooze_id, settle_id) = (id.clone(), id.clone(), id.clone());
            // Hover flips visibility only: GPUI reads hover per phase, and a
            // display toggle would skip prepaint yet paint ("must call
            // prepaint before paint").
            // In flow on the meta line, so the profile name truncates first.
            h_flex()
                .gap_px()
                .items_center()
                .when(!menu_open, |d| {
                    d.invisible().group_hover(group.clone(), |s| s.visible())
                })
                .child(
                    Button::new(SharedString::from(format!("pin-{id}")))
                        .icon(if pinned {
                            ExtraIcon::PinOff
                        } else {
                            ExtraIcon::Pin
                        })
                        .ghost()
                        .xsmall()
                        .icon_size(14.)
                        .tooltip(if pinned { "Unpin" } else { "Pin" })
                        .accessibility_label(if pinned { "Unpin" } else { "Pin" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let action = if pinned {
                                InboxAction::Unpin
                            } else {
                                InboxAction::Pin
                            };
                            this.inbox_mark(pin_id.clone(), action, None, cx)
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("snooze-{id}")))
                        .icon(ExtraIcon::AlarmClock)
                        .ghost()
                        .xsmall()
                        .icon_size(14.)
                        .selected(menu_open)
                        .tooltip("Snooze")
                        .accessibility_label("Snooze")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.inbox.snooze_menu = match &this.inbox.snooze_menu {
                                Some(open) if *open == snooze_id => None,
                                _ => Some(snooze_id.clone()),
                            };
                            cx.notify();
                        })),
                )
                .child(
                    Button::new(SharedString::from(format!("settle-{id}")))
                        .icon(IconName::Check)
                        .label("Settle")
                        .ghost()
                        .xsmall()
                        .icon_size(14.)
                        .disabled(blocker.is_some())
                        .tooltip(match blocker {
                            Some(why) => format!("Can't settle: it {why}, snooze it instead"),
                            None => "Settle session".into(),
                        })
                        .accessibility_label("Settle session")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.settle(settle_id.clone(), window, cx)
                        })),
                )
        };

        let open_id = id.clone();
        let quiet = p.text_muted;
        let card =
            v_flex()
                .id(SharedString::from(format!("row-{id}")))
                .group(group.clone())
                .role(Role::Button)
                .aria_label(title_of(row).to_string())
                .relative()
                .w_full()
                .min_h(px(design::SIDEBAR_CARD_MIN_HEIGHT))
                .justify_center()
                .px(px(12.))
                .py(px(14.))
                .gap(px(6.))
                .rounded(px(8.))
                .cursor_pointer()
                .bg(wash)
                .on_hover(crate::design::motion::hover_listener(
                    hover_key,
                    style.hover,
                ))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_thread(open_id.clone(), window, cx)
                }))
                // Drop it on a chat to hand that chat this thread's transcript.
                .on_drag(
                    crate::chat::DraggedThread {
                        id: id.clone(),
                        title: title_of(row).to_string(),
                    },
                    |dragged, _, _, cx| cx.new(|_| dragged.clone()),
                )
                // Profile first, with the status or age (or, on hover, the
                // actions) at its end; the title gets the full width below.
                .child(
                    h_flex()
                        .gap(px(7.))
                        .items_center()
                        .text_size(px(design::SIDEBAR_META_SIZE))
                        .text_color(quiet)
                        .when(drafted, |d| d.child(draft_mark(cx)))
                        .child(profile_mark(&row.profile_id, &name, h.max(sel), cx))
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .when_some(branch, |d, b| d.child(branch_label(b, cx)))
                        .child(
                            div().relative().flex_none().child(actions).child(
                                h_flex()
                                    .absolute()
                                    .top_0()
                                    .bottom_0()
                                    .right_0()
                                    .items_center()
                                    .when(menu_open, |d| d.invisible())
                                    .group_hover(group.clone(), |s| s.invisible())
                                    .child(match jump {
                                        Some(ix) => kit::shortcut_badge(ix, cx).into_any_element(),
                                        None => status_label(row, active, now, cx),
                                    }),
                            ),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .truncate()
                        .text_size(px(design::SIDEBAR_TITLE_SIZE))
                        .text_color(ink)
                        .when(unread, |d| d.font_semibold())
                        .when(row.parent_thread_id.is_some(), |d| d.child("↳ "))
                        .child(title_of(row).to_string()),
                );
        v_flex()
            .child(card.context_menu(self.row_menu(row, cx)))
            .when(menu_open, |d| d.child(self.render_snooze_menu(&id, cx)))
            .into_any_element()
    }

    fn render_snooze_menu(&self, thread_id: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let items = snooze_presets()
            .into_iter()
            .enumerate()
            .map(|(ix, (label, until))| {
                let id = thread_id.to_string();
                h_flex()
                    .id(("snooze-preset", ix))
                    .role(Role::Button)
                    .aria_label(label.clone())
                    .px_2()
                    .py_1()
                    .rounded(px(design::CONTROL_RADIUS))
                    .text_xs()
                    .cursor_pointer()
                    .map(|d| kit::smooth_bg(d, format!("snooze-preset-{ix}"), cx))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.inbox_mark(id.clone(), InboxAction::Snooze, Some(until), cx)
                    }))
            });
        let pop = kit::popover(SharedString::from(format!("snooze-{thread_id}")), cx);
        pop(v_flex().mx_2().mb_1().p_1().children(items))
    }

    /// Slim row for Snoozed and Settled: dimmed mark, title, wake or age.
    fn render_slim(
        &self,
        row: &InboxRow,
        active: bool,
        now: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        let id = row.thread_id.clone();
        let name = self
            .inbox
            .profile(&row.profile_id)
            .map_or_else(|| row.profile_id.clone(), |p| p.display_name.clone());
        let group: SharedString = format!("slim-{id}").into();
        let snoozed = row.state == InboxState::Snoozed;
        let (undo_id, delete_id, open_id) = (id.clone(), id.clone(), id.clone());
        let unread = self.inbox.unread.contains(&id);
        let menu = self.row_menu(row, cx);
        let label = match (snoozed, row.snooze_until) {
            (true, Some(at)) => wake_label(at),
            _ => short_age(row.settled_at.unwrap_or(row.last_activity_at), now),
        };
        sidebar_row(SharedString::from(format!("slim-row-{id}")), active, cx)
            .min_h(px(design::SIDEBAR_SLIM_HEIGHT))
            .text_size(px(design::SIDEBAR_TITLE_SIZE))
            .group(group.clone())
            .aria_label(title_of(row).to_string())
            .on_click(
                cx.listener(move |this, _, window, cx| {
                    this.open_thread(open_id.clone(), window, cx)
                }),
            )
            .child(
                div()
                    .opacity(0.4)
                    .child(profile_mark(&row.profile_id, &name, 0., cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(if unread { p.text } else { p.text_muted })
                    .when(unread, |d| d.font_semibold())
                    .child(title_of(row).to_string()),
            )
            .child(
                h_flex()
                    .gap_px()
                    .invisible()
                    .group_hover(group.clone(), |s| s.visible())
                    .child(
                        Button::new(SharedString::from(format!("unpark-{id}")))
                            .when(snoozed, |b| b.icon(ExtraIcon::Bell))
                            .when(!snoozed, |b| b.icon(IconName::Undo2))
                            .ghost()
                            .xsmall()
                            .tooltip(if snoozed {
                                "Wake now"
                            } else {
                                "Un-settle session"
                            })
                            .accessibility_label(if snoozed {
                                "Wake now"
                            } else {
                                "Un-settle session"
                            })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let action = if snoozed {
                                    InboxAction::Wake
                                } else {
                                    InboxAction::Unsettle
                                };
                                this.inbox_mark(undo_id.clone(), action, None, cx)
                            })),
                    )
                    .when(!snoozed, |d| {
                        d.child(
                            Button::new(SharedString::from(format!("delete-{id}")))
                                .icon(IconName::Delete)
                                .ghost()
                                .xsmall()
                                .tooltip("Delete conversation")
                                .accessibility_label("Delete conversation")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.delete_thread(delete_id.clone(), window, cx)
                                })),
                        )
                    }),
            )
            .child(
                h_flex()
                    .flex_none()
                    .gap_1()
                    .items_center()
                    .text_xs()
                    .text_color(if snoozed { p.accent } else { p.text_faint })
                    .group_hover(group, |s| s.invisible())
                    .when(snoozed, |d| {
                        d.child(Icon::new(ExtraIcon::AlarmClock).xsmall())
                    })
                    .child(label),
            )
            .context_menu(menu)
            .into_any_element()
    }
}

/// A filter chip.
fn chip(
    id: impl Into<ElementId>,
    label: String,
    on: bool,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let id: ElementId = id.into();
    kit::chip(SharedString::from(format!("{id:?}")), label, on, cx)
}

/// The profile badge: a colored tile with the first letter.
/// `lit` 0 draws the mark at rest, 1 at full strength on hover or open.
/// The pen before a drafted row's profile, in the theme's accent.
fn draft_mark(cx: &App) -> impl IntoElement {
    Icon::new(ExtraIcon::SquarePen)
        .size(px(14.))
        .flex_none()
        .text_color(Palette::of(cx).accent)
}

fn profile_mark(profile_id: &str, name: &str, lit: f32, cx: &App) -> impl IntoElement {
    let p = Palette::of(cx);
    let colors = [p.accent, p.success, p.warning, p.danger, p.text_muted];
    let hash = profile_id
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    // Identity keeps its color at rest (T3 reversed its grayscale pass);
    // hover and selection only bring it to full strength.
    let color = crate::design::motion::mix(
        p.text_muted,
        colors[hash as usize % colors.len()],
        0.7 + 0.3 * lit,
    );
    let letter = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or('·', |c| c.to_ascii_uppercase());
    div()
        .flex_none()
        .size(px(16.))
        .rounded(px(4.))
        .bg(color.opacity(0.22))
        .text_color(color)
        .text_size(px(10.))
        .font_semibold()
        .flex()
        .items_center()
        .justify_center()
        .child(letter.to_string())
}

fn branch_label(branch: String, cx: &App) -> impl IntoElement {
    let p = Palette::of(cx);
    h_flex()
        .flex_none()
        .gap_0p5()
        .items_center()
        .max_w(px(90.))
        .text_xs()
        .text_color(p.text_muted)
        .child(Icon::new(ExtraIcon::GitBranch).xsmall())
        .child(div().truncate().child(branch_tail(&branch)))
}

/// A long branch keeps its last path segment, the part that names it:
/// `feature/team/sidebar-pass` reads `…/sidebar-pass`.
fn branch_tail(branch: &str) -> String {
    match branch.rsplit_once('/') {
        Some((_, tail)) if branch.chars().count() > 14 && !tail.is_empty() => format!("…/{tail}"),
        _ => branch.to_string(),
    }
}

/// The open thread shows no Done or Woke: the person is looking at it.
/// Why a row can't be settled (t3code refuses live work), or None.
fn settle_blocker(row: &InboxRow) -> Option<&'static str> {
    match row.status {
        Some(InboxStatus::Working) => Some("is working"),
        Some(InboxStatus::NeedsInput) => Some("is waiting on you"),
        Some(InboxStatus::Background) => Some("has subagents running"),
        _ => None,
    }
}

/// The status label, crossfading in whenever the status changes (Working
/// to Done, Done to Failed) instead of snapping.
fn status_label(row: &InboxRow, active: bool, now: i64, cx: &App) -> AnyElement {
    let shown = row
        .status
        .filter(|s| !(active && matches!(s, InboxStatus::Done | InboxStatus::Woke)));
    let key = SharedString::from(format!("status-{}-{:?}", row.thread_id, shown));
    crate::design::motion::fade_in(
        div().flex_none().child(status_body(row, active, now, cx)),
        key,
        crate::design::motion::FADE_QUICK,
        2.,
    )
    .into_any_element()
}

fn status_body(row: &InboxRow, active: bool, now: i64, cx: &App) -> AnyElement {
    let p = Palette::of(cx);
    let text = |t: String, color| {
        div()
            .flex_none()
            .text_xs()
            .text_color(color)
            .child(t)
            .into_any_element()
    };
    let status = row
        .status
        .filter(|s| !(active && matches!(s, InboxStatus::Done | InboxStatus::Woke)));
    match status {
        Some(InboxStatus::Working) => h_flex()
            .flex_none()
            .gap_1()
            .items_center()
            .text_xs()
            .text_color(p.accent)
            .child(kit::pulse_dots(
                SharedString::from(format!("working-{}", row.thread_id)),
                p.accent,
            ))
            .child(match crate::run_clock::elapsed(&row.thread_id, cx) {
                Some(secs) => format!("Working {}", crate::design::motion::format_elapsed(secs)),
                None => "Working".into(),
            })
            .into_any_element(),
        // t3code's "needs you": amber, with a dot, until it is answered.
        Some(InboxStatus::NeedsInput) => h_flex()
            .flex_none()
            .gap_1()
            .items_center()
            .text_xs()
            .text_color(p.warning)
            .child(div().size(px(6.)).rounded_full().bg(p.warning))
            .child("Needs you")
            .into_any_element(),
        Some(InboxStatus::Background) => text("Subagents".into(), p.accent),
        Some(InboxStatus::Limited) => {
            let when = row
                .limit_reset_at
                .map(|at| format!("Limit · {}", wake_label(at)))
                .unwrap_or_else(|| "Usage limit".into());
            text(when, p.warning)
        }
        Some(InboxStatus::Interrupted) => text("Interrupted".into(), p.warning),
        Some(InboxStatus::Failed) => text("Failed".into(), p.danger),
        Some(InboxStatus::Stopped) => text("Paused".into(), p.text_muted),
        Some(InboxStatus::Woke) => text("Woke".into(), p.accent),
        Some(InboxStatus::Done) => h_flex()
            .flex_none()
            .gap_1()
            .items_center()
            .text_xs()
            .text_color(p.accent)
            .child(div().size(px(6.)).rounded_full().bg(p.accent))
            .child(short_age(row.last_activity_at, now))
            .into_any_element(),
        None => text(short_age(row.last_activity_at, now), p.text_faint),
    }
}

/// "now", "5m", "3h", "2d", then a date.
fn short_age(at: i64, now: i64) -> String {
    let secs = (now - at).max(0) / 1000;
    match secs {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 7 * 86_400 => format!("{}d", s / 86_400),
        _ => Local
            .timestamp_millis_opt(at)
            .single()
            .map_or_else(String::new, |t| t.format("%d.%m.").to_string()),
    }
}

/// "14:05" today, "Tue 09:00" later.
fn wake_label(at: i64) -> String {
    let Some(t) = Local.timestamp_millis_opt(at).single() else {
        return String::new();
    };
    if t.date_naive() == Local::now().date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%a %H:%M").to_string()
    }
}

/// T3's presets: in 1 hour, in 3 hours, this evening (if over an hour
/// away), tomorrow 09:00, next Monday 09:00.
fn snooze_presets() -> Vec<(String, i64)> {
    let now = Local::now();
    let at = |days: i64, hour: u32| {
        (now.date_naive() + Span::days(days))
            .and_hms_opt(hour, 0, 0)
            .and_then(|t| Local.from_local_datetime(&t).single())
    };
    let mut out = vec![
        ("In 1 hour".to_string(), now + Span::hours(1)),
        ("In 3 hours".to_string(), now + Span::hours(3)),
    ];
    if let Some(evening) = at(0, 18).filter(|t| *t - now > Span::hours(1)) {
        out.push(("This evening, 18:00".into(), evening));
    }
    if let Some(tomorrow) = at(1, 9) {
        out.push(("Tomorrow, 09:00".into(), tomorrow));
    }
    let to_monday = (7 - now.weekday().num_days_from_monday() as i64) % 7;
    let to_monday = if to_monday == 0 { 7 } else { to_monday };
    if to_monday != 1
        && let Some(monday) = at(to_monday, 9)
    {
        out.push(("Next week, Mon 09:00".into(), monday));
    }
    out.into_iter()
        .map(|(label, t)| (label, t.timestamp_millis()))
        .collect()
}

/// The FLIP group the sidebar's thread rows move in.
const SIDEBAR_GROUP: &str = "sidebar-threads";

/// A sidebar thread row that glides when the list resorts, or when it moves
/// between shelves (a thread starting work drops into Working).
fn slide(thread_id: &str, row: impl IntoElement) -> crate::design::flip::Flip {
    use crate::design::flip;
    flip::flip(
        SIDEBAR_GROUP,
        format!("thread:{thread_id}"),
        flip::ROW_SECS,
        row,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, state: InboxState, model: Option<&str>) -> InboxRow {
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
            title_pending: false,
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
        let mut inbox = Inbox::default();
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
        let mut inbox = Inbox::default();
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
}
