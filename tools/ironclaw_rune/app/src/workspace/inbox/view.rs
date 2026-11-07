//! Drawing the sidebar inbox (the render half of GPUI `workspace/inbox.rs`):
//! the header (search, filters, new thread), the filter panel, the profile
//! menu, the new-thread draft card, the cards, the shelves and their slim
//! rows.

use chrono::Utc;
use ironclaw_gpui_client::inbox::{InboxProfile, InboxState};
use ironclaw_gpui_client::profiles::DEFAULT_PROFILE_ID;
use rune::{Color, Cx, El, ScrollSpec, col, el, row};
use rune_kit::{ControlSize, Kit, Placement, TextInput};

use super::super::ui::{self, MEDIUM, SM, XS, t, tw};
use super::{
    SETTLED_PAGE, shelves,
};
use crate::design::smooth_scroll::WheelGlide;
use crate::design::{self, Palette};
use crate::icons;
use crate::state::{Page, State};
use crate::workspace::{active_id, draft};

mod rows;

pub(in crate::workspace) use rows::{branch_label, draft_mark, profile_mark, status_label};
use rows::{card, slim};

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
