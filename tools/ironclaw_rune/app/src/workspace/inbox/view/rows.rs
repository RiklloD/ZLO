//! The inbox's rows (the render half of GPUI `workspace/inbox.rs`): full
//! cards, the shelves' slim rows, their status and timer, and the small
//! marks other sidebar parts reuse.

use zlo_client::inbox::{InboxAction, InboxRow, InboxState, InboxStatus};
use rune::{Color, El, col, el, row};
use rune_kit::{ControlSize, Kit};

use crate::design::{self, Palette, motion};
use crate::icons;
use crate::state::State;
use crate::workspace::inbox::subagents::Tree;
use crate::workspace::inbox::{
    branch_tail, mark, settle, settle_blocker, short_age, snooze_presets, subagents, title_of,
    wake_label,
};
use crate::workspace::ui::{self, SEMIBOLD, XS, t, tw};
use crate::workspace::{message_search, thread_drag};

use super::selected_bg;

/// Card row: profile · branch with the status or age, then the title;
/// hover actions pin, snooze, settle. No model readout: the profile names it.
pub(super) fn card(
    s: &State,
    tree: &Tree,
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
        .child(
            selected_bg(
                p,
                menu_open,
                ui::ghost_icon(
                    k,
                    &icons::ALARM_CLOCK,
                    "Snooze",
                    ControlSize::Mini,
                    move |s| {
                        let menu = &mut s.workspace.inbox.snooze_menu;
                        *menu = match menu {
                            Some(open) if *open == snooze_id => None,
                            _ => Some(snooze_id.clone()),
                        };
                    },
                ),
            )
            // The snooze menu (`snooze_menu`) hangs off this button.
            .key(format!("{}-trigger", snooze_key(&id))),
        )
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
        .child(profile_mark(
            p,
            &row_data.profile_id,
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
        if row_data.parent_thread_id.is_some() {
            "↳ "
        } else {
            ""
        },
        title_of(row_data)
    );
    let open_id = id.clone();
    let menu_id = id.clone();
    let (drag_id, drag_title) = (id.clone(), title_of(row_data).to_string());
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
        .on_press(move |s: &mut State| {
            if !thread_drag::take_press(s) {
                crate::workspace::open_thread(s, open_id.clone());
            }
        })
        .on_secondary_press(move |s: &mut State, at, tx| {
            crate::workspace::inbox::open_row_menu(s, &menu_id, at, tx)
        })
        // Drop it on a chat to hand that chat this thread's transcript.
        .on_drag(move |s: &mut State, d, tx| thread_drag::drag(s, &drag_id, &drag_title, d, tx))
        .child(meta)
        .child(
            ui::side_tw(
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
    if let Some(line) = subagents::line(s, tree, row_data, false) {
        card = card.child(line);
    }
    // A resorted row glides to its new place (GPUI `flip::ROW_SECS`).
    let mut out = col()
        .key(format!("card-{id}"))
        .w_full()
        .motion(motion::ROW_RESORT)
        .child(card);
    if let Some(list) = subagents::list(s, tree, row_data, now, k) {
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
    let hit = s
        .workspace
        .message_search
        .hit(&query, &row_data.thread_id)?;
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

/// The snooze menu's key; its button is keyed `{key}-trigger`.
fn snooze_key(thread_id: &str) -> String {
    format!("snooze-{thread_id}")
}

/// The open snooze menu as rune-kit's `Menu`, under its card's alarm
/// button: one item per preset; Escape or a press outside closes it. Built
/// from the inbox view (the rows are built lazily, without the overlay
/// layer); the button keyed `{key}-trigger` anchors it.
pub(super) fn snooze_menu(s: &State, k: &Kit, layer: &mut Vec<El>) {
    let Some(id) = s.workspace.inbox.snooze_menu.clone() else {
        return;
    };
    let mut menu = rune_kit::Menu::new(snooze_key(&id))
        .open(true)
        .placement(rune_kit::Placement::BottomEnd)
        .min_width(180.)
        .on_close(|s: &mut State| s.workspace.inbox.snooze_menu = None);
    for (label, until) in snooze_presets() {
        let id = id.clone();
        menu = menu.item(
            rune_kit::MenuItem::new(label).on_select(move |s: &mut State| {
                mark(s, id.clone(), InboxAction::Snooze, Some(until))
            }),
        );
    }
    // The trigger it returns is a stand-in; the card's button anchors it.
    let _ = menu.build(k, layer);
}

/// Slim row for Snoozed and Settled: dimmed mark, title, wake or age.
pub(super) fn slim(
    s: &State,
    tree: &Tree,
    row_data: &InboxRow,
    active: bool,
    now: i64,
    k: &Kit,
) -> El {
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
        _ => short_age(
            row_data.settled_at.unwrap_or(row_data.last_activity_at),
            now,
        ),
    };
    let (undo_id, delete_id, open_id, menu_id) = (id.clone(), id.clone(), id.clone(), id.clone());
    let undo_label = if snoozed {
        "Wake now"
    } else {
        "Un-settle session"
    };
    let mut buttons = row()
        .gap(1.)
        .center_y()
        .pin(None, Some(0.), Some(0.), Some(0.))
        .group_opacity(0., 1.)
        .child(ui::ghost_icon(
            k,
            if snoozed {
                &icons::BELL
            } else {
                &icons::UNDO_2
            },
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
        .fill(if active {
            ui::selected_wash(p)
        } else {
            Color::TRANSPARENT
        })
        .hover(if active {
            Color::TRANSPARENT
        } else {
            ui::hover_wash(p)
        })
        .hover_group()
        .on_press(move |s: &mut State| crate::workspace::open_thread(s, open_id.clone()))
        .on_secondary_press(move |s: &mut State, at, tx| {
            crate::workspace::inbox::open_row_menu(s, &menu_id, at, tx)
        })
        .child(
            el().opacity(0.4)
                .child(profile_mark(p, &row_data.profile_id, &name, 0.)),
        )
        .child(
            el().grow(1.0).min_w(0.0).child(
                ui::side_tw(
                    title_of(row_data),
                    design::SIDEBAR_TITLE_SIZE,
                    if unread { SEMIBOLD } else { ui::REGULAR },
                    if unread { p.text } else { p.text_muted },
                )
                .truncate(),
            ),
        )
        .child(el().child(when).child(buttons));
    let line = subagents::line(s, tree, row_data, true);
    let list = subagents::list(s, tree, row_data, now, k);
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
pub(in crate::workspace) fn profile_mark(
    p: &Palette,
    profile_id: &str,
    name: &str,
    lit: f32,
) -> El {
    let colors = [p.accent, p.success, p.warning, p.danger, p.text_muted];
    let hash = profile_id
        .bytes()
        .fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    // Identity keeps its color at rest (T3 reversed its grayscale pass);
    // hover and selection only bring it to full strength.
    let color = mix(
        p.text_muted,
        colors[hash as usize % colors.len()],
        0.7 + 0.3 * lit,
    );
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
        .child(
            el().min_w(0.0)
                .child(t(branch_tail(branch), XS, p.text_muted).truncate()),
        )
}

/// The status label: what the row is doing, or its age. The open thread
/// shows no Done or Woke: the person is looking at it.
pub(in crate::workspace) fn status_label(
    s: &State,
    row_data: &InboxRow,
    active: bool,
    now: i64,
) -> El {
    let p = &s.palette;
    let status = row_data
        .status
        .filter(|st| !(active && matches!(st, InboxStatus::Done | InboxStatus::Woke)));
    let key = format!("status-{}-{:?}", row_data.thread_id, status);
    let plain = |text: String, color: Color| t(text, XS, color);
    let body =
        match status {
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
    row()
        .gap(2.)
        .center_y()
        .child(dot(0.))
        .child(dot(0.2))
        .child(dot(0.4))
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
