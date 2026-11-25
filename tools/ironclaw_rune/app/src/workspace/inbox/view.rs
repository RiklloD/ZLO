//! Drawing the sidebar inbox (the state is `workspace/inbox.rs`):
//! the header (search, filters, new thread), the filter panel, the
//! new-thread draft card, the cards, the shelves and their slim
//! rows.

use rune::{Color, Cx, El, col, el, row};
use rune_kit::{ControlSize, Kit, TextInput};
use zlo_client::inbox::InboxState;
use zlo_client::profiles::DEFAULT_PROFILE_ID;

use super::super::ui::{self, MEDIUM, XS, tw};

use crate::design::{self, Palette};
use crate::icons;
use crate::state::{Page, State};
use crate::workspace::draft;
use crate::workspace::inbox::picker;

pub(in crate::workspace) mod list;
mod rows;

pub(in crate::workspace) use rows::{
    branch_label, draft_mark, profile_color, profile_mark, pulse_dots, status_label,
};

/// The sidebar's thread list with its header. `layer` takes the profile
/// menu, which floats over the list so no row moves.
pub fn view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let inbox = &s.workspace.inbox;
    if inbox.new_menu_open {
        // Under the header, as wide as the list: it covers rows, never the
        // chat beside them.
        let width = cx
            .window_layout_with("inbox-header", |r| r[2])
            .map_or(picker::WIDTH, |w| (w - 16.).max(picker::MIN_WIDTH));
        layer.push(picker::anchored(
            k,
            col()
                .w(width)
                .min_h(0.0)
                .modal(true, |s: &mut State| {
                    picker::close(s);
                })
                .child(picker::profile_picker(s, k)),
            "inbox-header",
            0.,
        ));
    }
    rows::snooze_menu(s, k, layer);
    let mut column = col()
        .key("inbox")
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .child(header(s, k));
    if inbox.filters_open {
        column = column.child(filters(s, k));
    }
    column.child(list::list(s, k))
}

fn header(s: &State, k: &Kit) -> El {
    let inbox = &s.workspace.inbox;
    // T3 Code's search: a magnifier and the text, no box.
    let search = TextInput::new(&inbox.search)
        .width(None)
        .placeholder("Search")
        .plain()
        .leading_icon(&icons::SEARCH)
        .leading_icon_size(16.)
        .text_style(ui::sidebar_style(design::SIDEBAR_TITLE_SIZE, ui::REGULAR))
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
                ui::ghost_icon_named(
                    k,
                    &icons::LIST_FILTER,
                    "Filters",
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
                ui::ghost_icon(
                    k,
                    &icons::SQUARE_PEN,
                    "New thread",
                    ControlSize::Large,
                    picker::toggle_new_menu,
                ),
            )
            .key("new-thread"),
        )
}

/// A ghost button shown pressed.
fn selected_bg(p: &Palette, on: bool, button: El) -> El {
    el().radius(design::CONTROL_RADIUS)
        .fill(if on { p.active } else { Color::TRANSPARENT })
        .child(button)
}

fn filters(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let i = &s.workspace.inbox;
    let group = |label: &str| el().pt(4.).child(tw(label, XS, MEDIUM, p.text_faint));
    let wrap = |chips: Vec<El>| {
        row()
            .w_full()
            .min_w(0.0)
            .flex_wrap()
            .gap(4.)
            .children(chips)
    };
    let profiles: Vec<El> = i
        .list
        .profiles
        .iter()
        .enumerate()
        .map(|(ix, profile)| {
            let id = profile.profile_id.clone();
            let on = i.profile.as_deref() == Some(id.as_str());
            ui::chip(
                p,
                format!("f-profile-{ix}"),
                &profile.display_name,
                on,
                move |s| {
                    let pick = Some(id.clone());
                    let f = &mut s.workspace.inbox.profile;
                    *f = if *f == pick { None } else { pick };
                },
            )
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
                .child(ui::ghost_button(
                    k,
                    "New profile",
                    Some(&icons::PLUS),
                    super::new_profile,
                )),
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
        .transition(rune::Transition::reveal(rune::Edge::Top))
        .min_w(0.0)
        .mx(8.)
        .mb(4.)
        .child(ui::floating(p, body, 12.))
}

pub(super) fn fold(
    p: &Palette,
    k: &Kit,
    key: &'static str,
    label: &'static str,
    count: usize,
    open: bool,
) -> El {
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
        .child(
            ui::side_tw(label, design::SIDEBAR_META_SIZE + 1., MEDIUM, p.text_muted)
                .group_color(p.text),
        )
        // The count rolls as a mark the person made moves a row in or out
        // (those replies animate); a poll's change lands.
        .child(
            rune_kit::RollingText::new(format!("fold-{key}-count"), count.to_string())
                .value(count as f64)
                .font(ui::sidebar_style(
                    design::SIDEBAR_META_SIZE + 1.,
                    ui::REGULAR,
                ))
                .color(p.text_faint)
                .spring(design::motion::ROLL)
                .build(k),
        )
        .child(el().grow(1.0))
        .child(design::kit::chevron(
            &format!("fold-{key}-chevron"),
            open,
            design::kit::Turn::Disclose,
            12.,
            p.text_faint,
        ))
        // The header holds its place while its rows grow open or shut
        // under it (a growth list lands rows at once otherwise).
        .on_press_tx(move |s: &mut State, tx| {
            tx.keep_on_screen(format!("fold-{key}"));
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
pub(super) fn new_draft_card(s: &State, k: &Kit) -> Option<El> {
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
        .child(profile_mark(
            p,
            &profile_id,
            &name,
            if active { 1. } else { 0. },
        ))
        .child(
            el().grow(1.0)
                .min_w(36.)
                .child(ui::side_t(name, design::SIDEBAR_META_SIZE, p.text_muted).truncate()),
        );
    if let Some(b) = branch {
        meta = meta.child(branch_label(p, &b));
    }
    meta = meta.child(el().group_opacity(0., 1.).child(ui::ghost_icon(
        k,
        &icons::CLOSE,
        "Discard draft",
        ControlSize::Mini,
        draft::discard_new_draft,
    )));
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
            .child(ui::side_t(line, design::SIDEBAR_TITLE_SIZE, p.text).truncate()),
    )
}
