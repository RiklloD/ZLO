//! Drawing the sidebar inbox (the render half of GPUI `workspace/inbox.rs`):
//! the header (search, filters, new thread), the filter panel, the profile
//! menu, the new-thread draft card, the cards, the shelves and their slim
//! rows.

use chrono::Utc;
use ironclaw_gpui_client::inbox::{InboxAction, InboxProfile, InboxRow, InboxState, InboxStatus};
use ironclaw_gpui_client::profiles::DEFAULT_PROFILE_ID;
use rune::{Color, Cx, El, ScrollSpec, col, el, row};
use rune_kit::{ControlSize, Kit, Placement, TextInput};

use super::super::ui::{self, MEDIUM, SEMIBOLD, SM, XS, t, tw};
use super::{
    SETTLED_PAGE, Shelves, branch_tail, mark, settle, settle_blocker, shelves, short_age,
    snooze_presets, subagents, title_of, wake_label,
};
use crate::design::smooth_scroll::WheelGlide;
use crate::design::{self, Palette, motion};
use crate::icons;
use crate::state::{Page, State};
use crate::workspace::{active_id, draft, message_search};

/// The sidebar's thread list with its header. `layer` takes the profile
/// menu, which floats over the list so no row moves.
pub fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let inbox = &s.workspace.inbox;
    let query = inbox.query();
    let active = (s.page == Page::Chat)
        .then(|| active_id(s))
        .flatten();
    let sh = shelves(s);
    let now = Utc::now().timestamp_millis();
    let mut list: Vec<El> = Vec::new();
    // Message text is searched too; say so while older chats are indexed.
    if !query.is_empty() && s.workspace.message_search.indexing(&query) {
        list.push(
            el().px(12.)
                .py(4.)
                .child(t("Searching older chats…", design::SIDEBAR_META_SIZE, p.text_faint)),
        );
    }
    let badge = |id: &str| {
        inbox
            .cmd_held
            .then(|| sh.jump.iter().position(|j| j == id))
            .flatten()
    };
    // A new-thread draft sits on top until it is sent, then a rule.
    if let Some(card) = new_draft_card(s, k) {
        list.push(card);
        list.push(el().key("draft-rule").mx(12.).my(6.).h(1.).fill(p.border));
    }
    for row in sh.pinned.iter().chain(sh.main.iter()) {
        let is_active = active == Some(row.thread_id.as_str());
        list.push(card(s, &sh, row, is_active, badge(&row.thread_id), now, k));
    }
    if sh.main.is_empty() && sh.pinned.is_empty() && sh.working.is_empty() && inbox.list.rows.is_empty()
    {
        list.push(
            el().key("inbox-empty")
                .px(8.)
                .py(12.)
                .child(t("No conversations yet.", XS, p.text_faint)),
        );
    }
    // Like t3code: Working, Snoozed and Settled sit at the bottom of the
    // sidebar and unfold downward, scrolling with the list. Working keeps
    // full cards; the parked shelves use slim rows.
    let mut pushed = false;
    if !sh.working.is_empty() {
        pushed = true;
        list.push(el().key("shelf-gap").grow(1.0).min_h(8.));
        list.push(fold(p, "working", "Working", sh.working.len(), sh.working_open));
        if sh.working_open {
            for row in &sh.working {
                let is_active = active == Some(row.thread_id.as_str());
                list.push(card(s, &sh, row, is_active, badge(&row.thread_id), now, k));
            }
        }
    }
    let sections = [
        (
            "snoozed",
            "Snoozed",
            &sh.snoozed,
            inbox.show_snoozed || sh.unfold,
            usize::MAX,
        ),
        (
            "settled",
            "Settled",
            &sh.settled,
            inbox.show_settled || sh.unfold,
            inbox.settled_limit,
        ),
    ];
    for (key, label, items, open, limit) in sections {
        if items.is_empty() {
            continue;
        }
        if !std::mem::replace(&mut pushed, true) {
            list.push(el().key("shelf-gap").grow(1.0).min_h(8.));
        }
        list.push(fold(p, key, label, items.len(), open));
        if open {
            for row in items.iter().take(limit) {
                let is_active = active == Some(row.thread_id.as_str());
                list.push(slim(s, &sh, row, is_active, now, k));
            }
            if items.len() > limit {
                list.push(ui::ghost_button(k, "Show more", Some(&icons::CHEVRON_DOWN), |s| {
                    s.workspace.inbox.settled_limit += SETTLED_PAGE;
                }));
            }
        }
    }
    if inbox.new_menu_open {
        layer.push(rune_kit::popover::anchored(
            k,
            col()
                .w(design::SIDEBAR_WIDTH - 16.)
                .modal(true, |s: &mut State| s.workspace.inbox.new_menu_open = false)
                .child(profile_picker(s, k)),
            "new-thread",
            Placement::BottomEnd,
            4.,
        ));
    }
    let mut column = col()
        .key("inbox")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(header(s, k));
    if inbox.filters_open {
        column = column.child(filters(s, k));
    }
    column.child(
        col()
            .key("inbox-scroll")
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .scroll(ScrollSpec { stick_bottom: false }).wheel_glide("inbox-scroll")
            .child(
                col()
                    .key("inbox-list")
                    .w_full()
                    .min_h(0.0)
                    .px(8.)
                    .pb(8.)
                    .gap(4.)
                    .children(list),
            ),
    )
}

fn header(s: &State, k: &Kit) -> El {
    let inbox = &s.workspace.inbox;
    // T3 Code's search: a magnifier and the text, no box.
    let search = TextInput::new(&inbox.search)
        .placeholder("Search")
        .plain()
        .leading_icon(&icons::SEARCH)
        .leading_icon_size(16.)
        .text_style(rune::TextStyle::new(design::SIDEBAR_TITLE_SIZE, ui::REGULAR))
        .on_edit(|s: &mut State| &mut s.workspace.inbox.search)
        .on_change(|s: &mut State, _| super::search_changed(s))
        .build(k);
    let filter_on = inbox.filters_open || inbox.filtered();
    row()
        .key("inbox-header")
        .w_full()
        .h(design::SIDEBAR_SEARCH_HEIGHT)
        .px(12.)
        .gap(4.)
        .center_y()
        .child(el().grow(1.0).min_w(0.0).child(search))
        .child(
            selected_bg(
                &s.palette,
                filter_on,
                ui::ghost_icon(
                    k,
                    &icons::LIST_FILTER,
                    "Filter by profile, model or state",
                    ControlSize::Large,
                    |s| s.workspace.inbox.filters_open = !s.workspace.inbox.filters_open,
                ),
            )
            .key("inbox-filters"),
        )
        .child(
            selected_bg(
                &s.palette,
                inbox.new_menu_open,
                ui::ghost_icon(k, &icons::SQUARE_PEN, "New thread", ControlSize::Large, |s| {
                    s.workspace.inbox.new_menu_open = !s.workspace.inbox.new_menu_open
                }),
            )
            .key("new-thread"),
        )
}

/// A ghost button shown pressed (GPUI `.selected(true)`).
fn selected_bg(p: &Palette, on: bool, button: El) -> El {
    el().radius(design::CONTROL_RADIUS)
        .fill(if on { p.active } else { Color::TRANSPARENT })
        .child(button)
}

fn filters(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let i = &s.workspace.inbox;
    let group = |label: &str| el().pt(4.).child(tw(label, XS, MEDIUM, p.text_faint));
    let wrap = |chips: Vec<El>| row().w_full().min_w(0.0).flex_wrap().gap(4.).children(chips);
    let profiles: Vec<El> = i
        .list
        .profiles
        .iter()
        .enumerate()
        .map(|(ix, profile)| {
            let id = profile.profile_id.clone();
            let on = i.profile.as_deref() == Some(id.as_str());
            ui::chip(p, format!("f-profile-{ix}"), &profile.display_name, on, move |s| {
                let pick = Some(id.clone());
                let f = &mut s.workspace.inbox.profile;
                *f = if *f == pick { None } else { pick };
            })
        })
        .collect();
    let models: Vec<El> = i
        .models()
        .into_iter()
        .enumerate()
        .map(|(ix, model)| {
            let on = i.model.as_deref() == Some(model.as_str());
            let pick = model.clone();
            ui::chip(p, format!("f-model-{ix}"), &model, on, move |s| {
                let pick = Some(pick.clone());
                let f = &mut s.workspace.inbox.model;
                *f = if *f == pick { None } else { pick };
            })
        })
        .collect();
    let states: Vec<El> = [
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
        ui::chip(p, format!("f-state-{ix}"), label, on, move |s| {
            let f = &mut s.workspace.inbox.state;
            *f = if *f == Some(state) { None } else { Some(state) };
        })
    })
    .collect();
    let any_imported = i.list.rows.iter().any(|r| r.imported.is_some());
    let mut body = col()
        .w_full()
        .min_w(0.0)
        .padding(8.)
        .gap(4.)
        .child(
            row()
                .w_full()
                .justify_between()
                .center_y()
                .child(group("Profile"))
                .child(ui::ghost_button(k, "New profile", Some(&icons::PLUS), |s| {
                    super::new_profile(s)
                })),
        )
        .child(wrap(profiles));
    if !models.is_empty() {
        body = body.child(group("Model")).child(wrap(models));
    }
    body = body.child(group("State")).child(wrap(states));
    if any_imported || i.imported_only {
        body = body.child(group("Origin")).child(wrap(vec![ui::chip(
            p,
            "f-origin-0".into(),
            "Imported",
            i.imported_only,
            |s| s.workspace.inbox.imported_only = !s.workspace.inbox.imported_only,
        )]));
    }
    el().key("inbox-filters-panel")
        .min_w(0.0)
        .mx(8.)
        .mb(4.)
        .child(ui::floating(p, body, 12.))
}

/// The new-thread screen's profile menu; the current pick is highlighted.
/// Also opened from the new-thread screen's profile switch.
pub fn profile_picker(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let last = s.workspace.inbox.list.last_used_profile_id.clone();
    let chosen = s.workspace.draft.as_ref().map(|d| d.profile.clone());
    let mut profiles = s.workspace.inbox.list.profiles.clone();
    if profiles.is_empty() {
        profiles.push(InboxProfile {
            profile_id: DEFAULT_PROFILE_ID.into(),
            display_name: "Default".into(),
            git_branch: None,
            last_used_at: None,
        });
    }
    let rows: Vec<El> = profiles
        .into_iter()
        .enumerate()
        .map(|(ix, profile)| {
            let highlighted = chosen.as_deref() == Some(profile.profile_id.as_str());
            let is_last = last.as_deref() == Some(profile.profile_id.as_str());
            let id = profile.profile_id.clone();
            let mut r = row()
                .key(format!("pick-profile-{ix}"))
                .role("button")
                .label(format!("Start in {}", profile.display_name))
                .gap(8.)
                .px(8.)
                .py(4.)
                .center_y()
                .radius(design::CONTROL_RADIUS)
                .cursor(rune::Cursor::Pointer)
                .fill(if highlighted { p.accent_wash } else { Color::TRANSPARENT })
                .hover(p.hover)
                .child(profile_mark(p, &profile.profile_id, &profile.display_name, 1.))
                .child(
                    el().grow(1.0)
                        .min_w(0.0)
                        .child(t(profile.display_name.clone(), SM, if highlighted { p.text } else { p.text_muted }).truncate()),
                );
            if let Some(branch) = profile.git_branch.clone() {
                r = r.child(branch_label(p, &branch));
            }
            if is_last {
                r = r.child(t("Last used", XS, p.text_faint));
            }
            r.on_press(move |s: &mut State| draft::start_in_profile(s, id.clone()))
        })
        .collect();
    ui::floating(
        p,
        col()
            .key("profile-picker")
            .w_full()
            .padding(4.)
            .gap(1.)
            .child(el().px(8.).py(4.).child(tw("Start in profile", XS, MEDIUM, p.text_faint)))
            .children(rows)
            .child(ui::ghost_button(k, "New profile", Some(&icons::PLUS), |s| {
                super::new_profile(s)
            })),
        12.,
    )
}

fn fold(p: &Palette, key: &'static str, label: &'static str, count: usize, open: bool) -> El {
    // Neutral headings: color belongs to the rows' real states.
    row()
        .key(format!("fold-{key}"))
        .role("button")
        .label(format!("{label} {count}"))
        .w_full()
        .h(36.)
        .px(8.)
        .gap(8.)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .hover_group()
        .child(tw(label, design::SIDEBAR_META_SIZE + 1., MEDIUM, p.text_muted).group_color(p.text))
        .child(t(count.to_string(), design::SIDEBAR_META_SIZE + 1., p.text_faint))
        .child(el().grow(1.0))
        .child(ui::glyph(
            if open { &icons::CHEVRON_DOWN } else { &icons::CHEVRON_RIGHT },
            12.,
            p.text_faint,
        ))
        .on_press(move |s: &mut State| {
            let i = &mut s.workspace.inbox;
            match key {
                "working" => i.show_working = !i.show_working,
                "snoozed" => i.show_snoozed = !i.show_snoozed,
                _ => i.show_settled = !i.show_settled,
            }
        })
}

/// The unsent new-thread draft as a card: pen, the profile it was started
/// in, its first line. A click reopens the new-thread screen.
fn new_draft_card(s: &State, k: &Kit) -> Option<El> {
    let (line, saved_profile) = crate::drafts::preview(s, crate::drafts::NEW_THREAD)?;
    let p = &s.palette;
    let active = s.page == Page::Draft;
    // The open screen's profile is live; a closed draft keeps its own.
    let profile_id = s
        .workspace
        .draft
        .as_ref()
        .map(|d| d.profile.clone())
        .or(saved_profile)
        .unwrap_or_else(|| DEFAULT_PROFILE_ID.into());
    let profile = s.workspace.inbox.profile(&profile_id);
    let name = profile.map_or_else(|| profile_id.clone(), |p| p.display_name.clone());
    let branch = profile.and_then(|p| p.git_branch.clone());
    let wash = if active {
        ui::selected_wash(p)
    } else {
        ui::draft_wash(p)
    };
    let mut meta = row()
        .gap(7.)
        .center_y()
        .child(draft_mark(p))
        .child(profile_mark(p, &profile_id, &name, if active { 1. } else { 0. }))
        .child(el().grow(1.0).min_w(36.).child(t(name, design::SIDEBAR_META_SIZE, p.text_muted).truncate()));
    if let Some(b) = branch {
        meta = meta.child(branch_label(p, &b));
    }
    meta = meta.child(
        el().group_opacity(0., 1.).child(ui::ghost_icon(
            k,
            &icons::CLOSE,
            "Discard draft",
            ControlSize::Mini,
            draft::discard_new_draft,
        )),
    );
    Some(
        col()
            .key("row-new-draft")
            .role("button")
            .label(format!("Draft: {line}"))
            .w_full()
            .min_h(design::SIDEBAR_CARD_MIN_HEIGHT)
            .justify_center()
            .px(12.)
            .py(14.)
            .gap(6.)
            .radius(8.)
            .cursor(rune::Cursor::Pointer)
            .fill(wash)
            .hover(ui::draft_wash(p).alpha(0.14))
            .hover_group()
            .on_press(|s: &mut State| draft::open(s))
            .child(meta)
            .child(t(line, design::SIDEBAR_TITLE_SIZE, p.text).truncate()),
    )
}

/// Card row: profile · branch with the status or age, then the title;
/// hover actions pin, snooze, settle. No model readout: the profile names it.
fn card(
    s: &State,
    sh: &Shelves<'_>,
    row_data: &InboxRow,
    active: bool,
    jump: Option<usize>,
    now: i64,
    k: &Kit,
) -> El {
    let p = &s.palette;
    let inbox = &s.workspace.inbox;
    let id = row_data.thread_id.clone();
    let profile = inbox.profile(&row_data.profile_id);
    let name = profile.map_or_else(|| row_data.profile_id.clone(), |p| p.display_name.clone());
    // A thread in its own worktree shows that branch, not the profile's.
    let branch = row_data
        .worktree_branch
        .clone()
        .or_else(|| profile.and_then(|p| p.git_branch.clone()));
    let pinned = row_data.state == InboxState::Pinned;
    let menu_open = inbox.snooze_menu.as_deref() == Some(id.as_str());
    let unread = inbox.unread.contains(&id);
    let recede = !active && row_data.status.is_none() && !unread;
    // An unsent draft waits here: t3code's pen mark and warm tint.
    let drafted = !active && crate::drafts::has(s, &id);
    let rest = if active {
        ui::selected_wash(p)
    } else if drafted {
        ui::draft_wash(p)
    } else {
        Color::TRANSPARENT
    };
    let hover = if drafted {
        ui::draft_wash(p).alpha(0.14)
    } else {
        ui::hover_wash(p)
    };
    // Read titles sit at 75 % and brighten on hover; active and unread ones
    // are full ink.
    let ink = if recede { p.text.alpha(0.75) } else { p.text };
    let blocker = settle_blocker(row_data);

    // Like t3code: on hover the actions take the status slot's place.
    let (pin_id, snooze_id, settle_id) = (id.clone(), id.clone(), id.clone());
    let actions = row()
        .gap(1.)
        .center_y()
        .group_opacity(if menu_open { 1. } else { 0. }, 1.)
        .child(ui::ghost_icon(
            k,
            if pinned { &icons::PIN_OFF } else { &icons::PIN },
            if pinned { "Unpin" } else { "Pin" },
            ControlSize::Mini,
            move |s| {
                let action = if pinned {
                    InboxAction::Unpin
                } else {
                    InboxAction::Pin
                };
                mark(s, pin_id.clone(), action, None)
            },
        ))
        .child(selected_bg(
            p,
            menu_open,
            ui::ghost_icon(k, &icons::ALARM_CLOCK, "Snooze", ControlSize::Mini, move |s| {
                let menu = &mut s.workspace.inbox.snooze_menu;
                *menu = match menu {
                    Some(open) if *open == snooze_id => None,
                    _ => Some(snooze_id.clone()),
                };
            }),
        ))
        .child(
            rune_kit::Button::new("Settle")
                .variant(rune_kit::ButtonVariant::Ghost)
                .control_size(ControlSize::Mini)
                .icon(&icons::CHECK)
                .disabled(blocker.is_some())
                .help(match blocker {
                    Some(why) => format!("Can't settle: it {why}, snooze it instead"),
                    None => "Settle session".into(),
                })
                .on_press(move |s: &mut State| settle(s, settle_id.clone()))
                .build(k),
        );
    let end = match jump {
        Some(ix) => ui::shortcut_badge(p, ix),
        None => status_label(s, row_data, active, now),
    };
    let mut meta = row().gap(7.).center_y();
    if drafted {
        meta = meta.child(draft_mark(p));
    }
    meta = meta
        .child(profile_mark(p, &row_data.profile_id, &name, if active { 1. } else { 0. }))
        .child(el().grow(1.0).min_w(36.).child(t(name, design::SIDEBAR_META_SIZE, p.text_muted).truncate()));
    if let Some(b) = branch {
        meta = meta.child(branch_label(p, &b));
    }
    meta = meta.child(
        el().child(actions).child(
            row()
                .pin(None, Some(0.), Some(0.), Some(0.))
                .center_y()
                .group_opacity(if menu_open { 0. } else { 1. }, 0.)
                .child(end),
        ),
    );
    let title = format!(
        "{}{}",
        if row_data.parent_thread_id.is_some() { "↳ " } else { "" },
        title_of(row_data)
    );
    let open_id = id.clone();
    let menu_id = id.clone();
    let mut card = col()
        .key(format!("row-{id}"))
        .role("button")
        .label(title_of(row_data).to_string())
        .w_full()
        .min_h(design::SIDEBAR_CARD_MIN_HEIGHT)
        .justify_center()
        .px(12.)
        .py(14.)
        .gap(6.)
        .radius(8.)
        .cursor(rune::Cursor::Pointer)
        .fill(rest)
        .hover(if active { Color::TRANSPARENT } else { hover })
        .hover_group()
        .on_press(move |s: &mut State| crate::workspace::open_thread(s, open_id.clone()))
        .on_secondary_press(move |s: &mut State, at, tx| super::open_row_menu(s, &menu_id, at, tx))
        .child(meta)
        .child(
            tw(
                title,
                design::SIDEBAR_TITLE_SIZE,
                if unread { SEMIBOLD } else { ui::REGULAR },
                ink,
            )
            .truncate(),
        );
    if let Some(snippet) = message_snippet(s, row_data) {
        card = card.child(snippet);
    }
    if let Some(line) = subagents::line(s, &sh.tree, row_data, false) {
        card = card.child(line);
    }
    // A resorted row glides to its new place (GPUI `flip::ROW_SECS`).
    let mut out = col()
        .key(format!("card-{id}"))
        .w_full()
        .motion(motion::ROW_RESORT)
        .child(card);
    if menu_open {
        out = out.child(snooze_menu(p, &id));
    }
    if let Some(list) = subagents::list(s, &sh.tree, row_data, now, k) {
        out = out.child(list);
    }
    out
}

/// Under a row that matched by message text (not by its title): the
/// matching snippet; a click opens the thread at that message.
fn message_snippet(s: &State, row_data: &InboxRow) -> Option<El> {
    let query = s.workspace.inbox.query();
    if query.is_empty() || title_of(row_data).to_lowercase().contains(&query) {
        return None;
    }
    let hit = s.workspace.message_search.hit(&query, &row_data.thread_id)?;
    let p = &s.palette;
    let (thread_id, message_id) = (row_data.thread_id.clone(), hit.message_id.clone());
    Some(
        el().key(format!("row-hit-{}", row_data.thread_id))
            .w_full()
            .min_w(0.0)
            .child(message_search::snippet_text(p, hit, design::SIDEBAR_META_SIZE).truncate())
            .on_press(move |s: &mut State| {
                crate::workspace::open_thread_at(s, thread_id.clone(), message_id.clone())
            }),
    )
}

fn snooze_menu(p: &Palette, thread_id: &str) -> El {
    let items: Vec<El> = snooze_presets()
        .into_iter()
        .enumerate()
        .map(|(ix, (label, until))| {
            let id = thread_id.to_string();
            row()
                .key(format!("snooze-preset-{ix}"))
                .role("button")
                .label(label.clone())
                .px(8.)
                .py(4.)
                .radius(design::CONTROL_RADIUS)
                .cursor(rune::Cursor::Pointer)
                .hover(p.hover)
                .child(t(label, XS, p.text))
                .on_press(move |s: &mut State| mark(s, id.clone(), InboxAction::Snooze, Some(until)))
        })
        .collect();
    el().key(format!("snooze-{thread_id}"))
        .mx(8.)
        .mb(4.)
        .child(ui::floating(p, col().w_full().padding(4.).children(items), 12.))
}

/// Slim row for Snoozed and Settled: dimmed mark, title, wake or age.
fn slim(s: &State, sh: &Shelves<'_>, row_data: &InboxRow, active: bool, now: i64, k: &Kit) -> El {
    let p = &s.palette;
    let id = row_data.thread_id.clone();
    let name = s
        .workspace
        .inbox
        .profile(&row_data.profile_id)
        .map_or_else(|| row_data.profile_id.clone(), |p| p.display_name.clone());
    let snoozed = row_data.state == InboxState::Snoozed;
    let unread = s.workspace.inbox.unread.contains(&id);
    let label = match (snoozed, row_data.snooze_until) {
        (true, Some(at)) => wake_label(at),
        _ => short_age(row_data.settled_at.unwrap_or(row_data.last_activity_at), now),
    };
    let (undo_id, delete_id, open_id, menu_id) = (id.clone(), id.clone(), id.clone(), id.clone());
    let undo_label = if snoozed { "Wake now" } else { "Un-settle session" };
    let mut buttons = row()
        .gap(1.)
        .center_y()
        .pin(None, Some(0.), Some(0.), Some(0.))
        .group_opacity(0., 1.)
        .child(ui::ghost_icon(
        k,
        if snoozed { &icons::BELL } else { &icons::UNDO_2 },
        undo_label,
        ControlSize::Mini,
        move |s| {
            let action = if snoozed {
                InboxAction::Wake
            } else {
                InboxAction::Unsettle
            };
            mark(s, undo_id.clone(), action, None)
        },
    ));
    if !snoozed {
        buttons = buttons.child(ui::ghost_icon(
            k,
            &icons::DELETE,
            "Delete conversation",
            ControlSize::Mini,
            move |s| crate::workspace::delete_thread(s, delete_id.clone()),
        ));
    }
    let mut when = row().gap(4.).center_y().group_opacity(1., 0.);
    if snoozed {
        when = when.child(ui::glyph(&icons::ALARM_CLOCK, 12., p.accent));
    }
    when = when.child(t(label, XS, if snoozed { p.accent } else { p.text_faint }));
    let slim = row()
        .key(format!("slim-row-{id}"))
        .role("button")
        .label(title_of(row_data).to_string())
        .w_full()
        .min_h(design::SIDEBAR_SLIM_HEIGHT)
        .px(8.)
        .gap(8.)
        .center_y()
        .radius(design::ROW_RADIUS)
        .cursor(rune::Cursor::Pointer)
        .fill(if active { ui::selected_wash(p) } else { Color::TRANSPARENT })
        .hover(if active { Color::TRANSPARENT } else { ui::hover_wash(p) })
        .hover_group()
        .on_press(move |s: &mut State| crate::workspace::open_thread(s, open_id.clone()))
        .on_secondary_press(move |s: &mut State, at, tx| super::open_row_menu(s, &menu_id, at, tx))
        .child(el().opacity(0.4).child(profile_mark(p, &row_data.profile_id, &name, 0.)))
        .child(
            el().grow(1.0).min_w(0.0).child(
                tw(
                    title_of(row_data),
                    design::SIDEBAR_TITLE_SIZE,
                    if unread { SEMIBOLD } else { ui::REGULAR },
                    if unread { p.text } else { p.text_muted },
                )
                .truncate(),
            ),
        )
        .child(el().child(when).child(buttons));
    let line = subagents::line(s, &sh.tree, row_data, true);
    let list = subagents::list(s, &sh.tree, row_data, now, k);
    if line.is_none() {
        return slim;
    }
    let mut out = col()
        .key(format!("slim-{id}"))
        .w_full()
        .motion(motion::ROW_RESORT)
        .child(slim);
    if let Some(line) = line {
        out = out.child(el().pl(34.).pb(4.).child(line));
    }
    if let Some(list) = list {
        out = out.child(list);
    }
    out
}

/// The pen before a drafted row's profile, in the theme's accent.
pub(in crate::workspace) fn draft_mark(p: &Palette) -> El {
    ui::glyph(&icons::SQUARE_PEN, 14., p.accent)
}

/// The profile badge: a colored tile with the first letter. `lit` 0 draws
/// the mark at rest, 1 at full strength on hover or open.
pub(in crate::workspace) fn profile_mark(p: &Palette, profile_id: &str, name: &str, lit: f32) -> El {
    let colors = [p.accent, p.success, p.warning, p.danger, p.text_muted];
    let hash = profile_id
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    // Identity keeps its color at rest (T3 reversed its grayscale pass);
    // hover and selection only bring it to full strength.
    let color = mix(p.text_muted, colors[hash as usize % colors.len()], 0.7 + 0.3 * lit);
    let letter = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or('·', |c| c.to_ascii_uppercase());
    row()
        .size(16., 16.)
        .radius(4.)
        .fill(color.alpha(0.22))
        .center()
        .child(tw(letter.to_string(), 10., SEMIBOLD, color))
}

/// Linear mix in OKLab, `t` 0 → `a`, 1 → `b`.
pub(in crate::workspace) fn mix(a: Color, b: Color, t: f32) -> Color {
    let (x, y) = (a.to_oklab(), b.to_oklab());
    let l = |i: usize| x[i] + (y[i] - x[i]) * t;
    Color::from_oklab([l(0), l(1), l(2), l(3)])
}

pub(in crate::workspace) fn branch_label(p: &Palette, branch: &str) -> El {
    row()
        .gap(2.)
        .center_y()
        .max_w(90.)
        .min_w(0.0)
        .shrink()
        .child(ui::glyph(&icons::GIT_BRANCH, 12., p.text_muted))
        .child(el().min_w(0.0).child(t(branch_tail(branch), XS, p.text_muted).truncate()))
}

/// The status label: what the row is doing, or its age. The open thread
/// shows no Done or Woke: the person is looking at it.
pub(super) fn status_label(s: &State, row_data: &InboxRow, active: bool, now: i64) -> El {
    let p = &s.palette;
    let status = row_data
        .status
        .filter(|st| !(active && matches!(st, InboxStatus::Done | InboxStatus::Woke)));
    let key = format!("status-{}-{:?}", row_data.thread_id, status);
    let plain = |text: String, color: Color| t(text, XS, color);
    let body = match status {
        Some(InboxStatus::Working) => row()
            .gap(4.)
            .center_y()
            .child(pulse_dots(p.accent))
            .child(t(
                match crate::run_clock::elapsed(s, &row_data.thread_id) {
                    Some(secs) => format!("Working {}", format_elapsed(secs)),
                    None => "Working".into(),
                },
                XS,
                p.accent,
            )),
        // t3code's "needs you": amber, with a dot, until it is answered.
        Some(InboxStatus::NeedsInput) => row()
            .gap(4.)
            .center_y()
            .child(ui::dot(p.warning, 6.))
            .child(t("Needs you", XS, p.warning)),
        Some(InboxStatus::Background) => plain("Subagents".into(), p.accent),
        Some(InboxStatus::Limited) => plain(
            row_data
                .limit_reset_at
                .map(|at| format!("Limit · {}", wake_label(at)))
                .unwrap_or_else(|| "Usage limit".into()),
            p.warning,
        ),
        Some(InboxStatus::Interrupted) => plain("Interrupted".into(), p.warning),
        Some(InboxStatus::Failed) => plain("Failed".into(), p.danger),
        Some(InboxStatus::Stopped) => plain("Paused".into(), p.text_muted),
        Some(InboxStatus::Woke) => plain("Woke".into(), p.accent),
        Some(InboxStatus::Done) => row()
            .gap(4.)
            .center_y()
            .child(ui::dot(p.accent, 6.))
            .child(t(short_age(row_data.last_activity_at, now), XS, p.accent)),
        None => plain(short_age(row_data.last_activity_at, now), p.text_faint),
    };
    // Keyed by status: a change crossfades instead of snapping.
    el().key(key).child(body)
}

/// Three dots pulsing in turn (GPUI `kit::pulse_dots`).
fn pulse_dots(color: Color) -> El {
    let dot = |phase: f32| {
        ui::dot(color, 4.).cycle(rune::Cycle {
            period: 1.2,
            phase,
            stops: &[(0.0, 0.35), (0.5, 1.0), (1.0, 0.35)],
            still: 1.0,
            fps: 30.0,
        })
    };
    row().gap(2.).center_y().child(dot(0.)).child(dot(0.2)).child(dot(0.4))
}

/// "12s", "2m 33s", "1h 04m" (GPUI `motion::format_elapsed`).
pub(in crate::workspace) fn format_elapsed(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::format_elapsed;

    #[test]
    fn elapsed_reads_like_the_gpui_timer() {
        assert_eq!(format_elapsed(5), "5s");
        assert_eq!(format_elapsed(153), "2m 33s");
        assert_eq!(format_elapsed(3840), "1h 4m");
    }
}
