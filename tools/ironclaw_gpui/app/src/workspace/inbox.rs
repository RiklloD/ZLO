//! The sidebar inbox (t3code's): one list by recency with search and
//! filters (profile, model, state), Pinned on top, Snoozed (N) and
//! Settled (N) folded at the bottom, and the profile menu of the new-thread
//! screen.
//!
//! The backend joins and classifies every row (`GET /inbox`), so this file
//! only renders and sends marks. Refresh is event-driven: on connect, when
//! the window comes to the front, after a run or a mark. While a row is
//! working, a slow poll follows it, only with the window in front. A snooze
//! or usage limit that ends sets one timer for that moment; an idle inbox
//! schedules nothing.

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

/// Poll period while a row is working.
const POLL: Duration = Duration::from_secs(5);
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
    show_snoozed: bool,
    show_settled: bool,
    settled_limit: usize,
    snooze_menu: Option<String>,
    loading: bool,
    stale: bool,
    polling: bool,
    _wake: Option<Task<()>>,
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
            settled_limit: SETTLED_PAGE,
            ..Self::default()
        }
    }

    pub(super) fn row(&self, thread_id: &str) -> Option<&InboxRow> {
        self.list.rows.iter().find(|r| r.thread_id == thread_id)
    }

    /// The thread to open first: the top of the list, never a parked one.
    pub(super) fn first_open(&self) -> Option<String> {
        self.list
            .rows
            .iter()
            .find(|r| !matches!(r.state, InboxState::Snoozed | InboxState::Settled))
            .or(self.list.rows.first())
            .map(|r| r.thread_id.clone())
    }

    fn profile(&self, id: &str) -> Option<&InboxProfile> {
        self.list.profiles.iter().find(|p| p.profile_id == id)
    }

    fn needs_poll(&self) -> bool {
        self.list.rows.iter().any(|r| {
            matches!(
                r.status,
                Some(InboxStatus::Working | InboxStatus::Background)
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

pub(crate) fn title_of(row: &InboxRow) -> &str {
    row.title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("New conversation")
}

impl Workspace {
    /// Take a fresh list: titles cache, poll, wake timer.
    pub(super) fn apply_inbox(&mut self, list: InboxListResponse, cx: &mut Context<Self>) {
        crate::titles::set_rows(&list.rows, cx);
        self.inbox.list = list;
        self.ensure_inbox_poll(cx);
        self.schedule_wake(cx);
        cx.notify();
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

    /// Settle a row. Settling the open thread moves on to its neighbour in
    /// the list, like t3code's forward navigation.
    fn settle(&mut self, thread_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let open =
            self.page == Page::Chat && self.active.as_ref().is_some_and(|(id, _)| *id == thread_id);
        if open {
            let rows = &self.inbox.list.rows;
            let order: Vec<&str> = rows
                .iter()
                .filter(|r| r.state == InboxState::Pinned)
                .chain(
                    rows.iter()
                        .filter(|r| matches!(r.state, InboxState::Inbox | InboxState::Working)),
                )
                .map(|r| r.thread_id.as_str())
                .collect();
            let next = order.iter().position(|id| *id == thread_id).and_then(|ix| {
                order
                    .get(ix + 1)
                    .or_else(|| ix.checked_sub(1).and_then(|i| order.get(i)))
            });
            if let Some(next) = next.map(|id| id.to_string()) {
                self.open_thread(next, window, cx);
            }
        }
        self.inbox_mark(thread_id, InboxAction::Settle, None, cx);
    }

    /// Opening a row with an unseen result clears its Done / Woke badge.
    pub(super) fn visit(&mut self, thread_id: &str, cx: &mut Context<Self>) {
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
        let pinned = in_state(&[InboxState::Pinned]);
        let main = in_state(&[InboxState::Inbox, InboxState::Working]);
        let snoozed = in_state(&[InboxState::Snoozed]);
        let settled = in_state(&[InboxState::Settled]);
        // Searching or filtering on a parked state unfolds it.
        let unfold = !query.is_empty() || self.inbox.state.is_some();
        let now = Utc::now().timestamp_millis();

        let mut list: Vec<AnyElement> = Vec::new();
        let enter = Style::of(cx).enter;
        for (ix, row) in pinned.iter().chain(main.iter()).enumerate() {
            let is_active = active.as_deref() == Some(row.thread_id.as_str());
            let card = self.render_card(row, is_active, now, cx);
            // Cards cascade in when they first mount (app start, a filter
            // change); a mounted card never replays.
            list.push(
                crate::design::motion::fade_in_after(
                    div().w_full().child(card),
                    SharedString::from(format!("card-in-{}", row.thread_id)),
                    enter,
                    4.,
                    (ix.min(12) * 28) as u64,
                )
                .into_any_element(),
            );
        }
        if main.is_empty() && pinned.is_empty() && self.inbox.list.rows.is_empty() {
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
        // Like t3code: the shelves sit at the bottom of the sidebar and
        // unfold downward, scrolling with the list.
        let mut pushed = false;
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
                    list.push(self.render_slim(row, is_active, now, cx));
                }
                if items.len() > limit {
                    list.push(
                        Button::new("settled-more")
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

        v_flex()
            .flex_1()
            .min_h_0()
            .child(self.render_inbox_header(cx))
            .when(self.inbox.new_menu_open, |d| {
                d.child(
                    div()
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.inbox.new_menu_open = false;
                            cx.notify();
                        }))
                        .child(self.render_picker(cx)),
                )
            })
            .when(self.inbox.filters_open, |d| {
                d.child(self.render_filters(cx))
            })
            .child(
                v_flex()
                    .id("inbox-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .pb_2()
                    .gap_px()
                    .children(list),
            )
    }

    fn render_inbox_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let filtered = self.inbox.profile.is_some()
            || self.inbox.model.is_some()
            || self.inbox.state.is_some();
        h_flex()
            .px_2()
            .pt_2()
            .pb_1()
            .gap_1()
            .items_center()
            .children(
                self.inbox
                    .search
                    .as_ref()
                    .map(|(input, _)| div().flex_1().child(Input::new(input).small())),
            )
            .child(
                Button::new("inbox-filters")
                    .icon(ExtraIcon::ListFilter)
                    .ghost()
                    .small()
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
                    .small()
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
                .when(!highlighted, |d| kit::smooth_bg(d, format!("pick-profile-{ix}"), cx))
                .child(profile_mark(&profile.profile_id, &profile.display_name, cx))
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
        let (color, hover) = match key {
            "snoozed" => (p.accent, p.accent),
            _ => (p.text_faint, p.text_muted),
        };
        h_flex()
            .id(SharedString::from(format!("fold-{key}")))
            .role(Role::Button)
            .aria_label(format!("{label} ({count})"))
            .flex_none()
            .h_8()
            .px_2()
            .gap_2()
            .items_center()
            .text_xs()
            .font_medium()
            .text_color(color)
            .cursor_pointer()
            .hover(|s| s.text_color(hover))
            .child(if open {
                label.to_string()
            } else {
                format!("{label} ({count})")
            })
            .child(div().flex_1().min_w_2().h_px().bg(p.border))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronUp
                })
                .xsmall(),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                match key {
                    "snoozed" => this.inbox.show_snoozed = !this.inbox.show_snoozed,
                    _ => this.inbox.show_settled = !this.inbox.show_settled,
                }
                cx.notify();
            }))
            .into_any_element()
    }

    /// Card row: profile · branch · status or age, title, engines; hover
    /// actions pin, snooze, settle.
    fn render_card(
        &self,
        row: &InboxRow,
        active: bool,
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
        let recede = !active && row.status.is_none();
        let style = *Style::of(cx);
        let hover_key: SharedString = format!("card:{id}").into();
        let h = crate::design::motion::hover_t(&hover_key);
        let sel = crate::design::motion::state_t(
            &format!("card:{id}:on"),
            active,
            style.slide,
            cx.reduce_motion(),
        );
        let wash = crate::design::motion::mix(
            crate::design::motion::mix(p.hover.opacity(0.), p.hover, h),
            if p.light { p.surface_card } else { p.active },
            sel,
        );

        // Like t3code: on hover the actions take the status slot's place.
        let blocker = settle_blocker(row);
        let actions = {
            let (pin_id, snooze_id, settle_id) = (id.clone(), id.clone(), id.clone());
            // Hover flips visibility only: GPUI reads hover per phase, and a
            // display toggle would skip prepaint yet paint ("must call
            // prepaint before paint").
            h_flex()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
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
        let card =
            v_flex()
                .id(SharedString::from(format!("row-{id}")))
                .group(group.clone())
                .role(Role::Button)
                .aria_label(title_of(row).to_string())
                .relative()
                .w_full()
                .px_2()
                .py_1p5()
                .gap_0p5()
                .rounded(px(style.control_radius))
                .cursor_pointer()
                .bg(wash)
                .when(sel > 0.01, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left(px(-1.))
                            .top(px(10.))
                            .bottom(px(10.))
                            .w(px(3.))
                            .rounded_full()
                            .bg(p.accent.opacity(sel)),
                    )
                })
                .on_hover(crate::design::motion::hover_listener(hover_key, style.hover))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_thread(open_id.clone(), window, cx)
                }))
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_xs()
                        .text_color(p.text_muted)
                        .child(profile_mark(&row.profile_id, &name, cx))
                        .child(div().flex_none().max_w(px(96.)).truncate().child(name))
                        .when_some(branch, |d, b| d.child(branch_label(b, cx)))
                        .child(div().flex_1())
                        .child(
                            div()
                                .relative()
                                .flex_none()
                                .child(
                                    div()
                                        .when(menu_open, |d| d.invisible())
                                        .group_hover(group.clone(), |s| s.invisible())
                                        .child(status_label(row, active, now, cx)),
                                )
                                .child(actions),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .text_color(if recede { p.text_muted } else { p.text })
                                .when(row.parent_thread_id.is_some(), |d| d.child("↳ "))
                                .child(title_of(row).to_string()),
                        )
                        .child(engines(&row.engines, cx)),
                );
        v_flex()
            .child(card)
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
        let label = match (snoozed, row.snooze_until) {
            (true, Some(at)) => wake_label(at),
            _ => short_age(row.settled_at.unwrap_or(row.last_activity_at), now),
        };
        sidebar_row(SharedString::from(format!("slim-row-{id}")), active, cx)
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
                    .child(profile_mark(&row.profile_id, &name, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(p.text_muted)
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
                div()
                    .flex_none()
                    .text_xs()
                    .text_color(p.text_faint)
                    .group_hover(group, |s| s.invisible())
                    .child(label),
            )
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
fn profile_mark(profile_id: &str, name: &str, cx: &App) -> impl IntoElement {
    let p = Palette::of(cx);
    let colors = [p.accent, p.success, p.warning, p.danger, p.text_muted];
    let hash = profile_id
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    let color = colors[hash as usize % colors.len()];
    let letter = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or('·', |c| c.to_ascii_uppercase());
    div()
        .flex_none()
        .size(px(14.))
        .rounded(px(3.))
        .bg(color.opacity(0.22))
        .text_color(color)
        .text_size(px(9.))
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
        .text_color(p.text_faint)
        .child(Icon::new(ExtraIcon::GitBranch).xsmall())
        .child(div().truncate().child(branch))
}

/// One mark per engine that ran in the thread (no brand logos are bundled;
/// a lettered tile per engine family stands in).
fn engines(list: &[InboxEngine], cx: &App) -> impl IntoElement {
    let p = Palette::of(cx);
    let marks = list.iter().take(4).map(|e| {
        let (letter, color) = engine_mark(e, p);
        div()
            .flex_none()
            .size(px(14.))
            .rounded_full()
            .border_1()
            .border_color(color.opacity(0.5))
            .text_color(color)
            .text_size(px(8.))
            .font_semibold()
            .flex()
            .items_center()
            .justify_center()
            .child(letter)
    });
    h_flex().flex_none().gap_0p5().children(marks)
}

fn engine_mark(e: &InboxEngine, p: &Palette) -> (&'static str, gpui_kit::Hsla) {
    let key = format!(
        "{} {}",
        e.runtime_id,
        e.provider.as_deref().unwrap_or_default()
    )
    .to_lowercase();
    if key.contains("claude") || key.contains("anthropic") {
        ("C", p.warning)
    } else if key.contains("antigravity") || key.contains("gemini") || key.contains("google") {
        ("G", p.success)
    } else if key.contains("muse") || key.contains("meta") {
        ("M", p.danger)
    } else if key.contains("codex") || key.contains("openai") {
        ("O", p.text)
    } else {
        ("I", p.accent)
    }
}

/// The open thread shows no Done or Woke: the person is looking at it.
/// Why a row can't be settled (t3code refuses live work), or None.
fn settle_blocker(row: &InboxRow) -> Option<&'static str> {
    match row.status {
        Some(InboxStatus::Working) => Some("is working"),
        Some(InboxStatus::Background) => Some("has subagents running"),
        _ => None,
    }
}

fn status_label(row: &InboxRow, active: bool, now: i64, cx: &App) -> AnyElement {
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
            .child("Working")
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
            parent_thread_id: None,
            automation: false,
        }
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
