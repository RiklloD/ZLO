//! The sidebar footer's update button, after t3code's `SidebarUpdatePill`:
//! download first, restart to install later.
//!
//! The button reads the update rows `remote::updates` keeps for every
//! computer (a background read each minute):
//!
//! - up to date: a press checks again;
//! - an update waiting ("Update ready to download"): a press downloads it on
//!   every computer that is behind — each builds the new version into its
//!   staging folder while everything keeps running;
//! - downloading: the glyph breathes until the builds are done, then
//!   "Update downloaded" shows for a moment;
//! - downloaded ("… Click to restart and install"): a press opens the
//!   restart dialog; only its button installs, the others first and This Mac
//!   last (ZLO quits and reopens on the new build);
//! - installing: the button grows into a capsule naming the step.
//!
//! Resting on the button while an update waits or is downloaded opens a
//! card with what changed (the commit subjects), instead of the tooltip.

use std::time::Duration;

use rune::{Color, Cursor, El, ScrollSpec, Tx, col, el, row};
use rune_kit::{Dialog, HoverCard, Kit, Placement, Toast, ToastRegion};
use zlo_client::environments::THIS_MAC;

use super::super::ui::{self, SEMIBOLD, SM, XS, t, tw};
use crate::design::{self, Palette, kit};
use crate::icons;
use crate::remote::updates::{self, Pending, RowState, UpdatesState};
use crate::state::{Page, State};

/// The hover card's key; the button is keyed `{PREVIEW}-trigger` so the
/// card anchors to it.
const PREVIEW: &str = "update-preview";
/// How long the card stays after the pointer leaves the button or card
/// (long enough to cross the gap between them).
const PREVIEW_LINGER: Duration = Duration::from_millis(180);

/// What the button shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FooterUpdate {
    /// Nothing waiting (or never read): a press checks.
    Idle {
        tip: String,
    },
    Checking,
    /// `machines` computers have an update to download, `commits` at most.
    Available {
        machines: usize,
        commits: u32,
    },
    /// A download started here is building.
    Downloading,
    /// `machines` computers have an update ready to install; `to` is the
    /// downloaded commit when there is one computer.
    Downloaded {
        machines: usize,
        to: Option<String>,
    },
    /// An install is running: `line` names the computer, `step` what it is
    /// doing (empty before it says).
    Updating {
        line: String,
        step: String,
    },
    /// The last download or install ended with a computer failing.
    Failed,
    /// Updates are off on every computer ZLO knows.
    Off,
}

/// The hover card's open state: open while the pointer is on the button or
/// on the card, closed a moment after it left both.
#[derive(Default)]
pub struct UpdatePreview {
    pub open: bool,
    over_button: bool,
    over_card: bool,
    /// Bumped on every enter and leave: a close timer from an older one
    /// does nothing.
    generation: u64,
}

/// The button's state. `machines`: how many computers this Mac updates
/// (a row may be missing for one that never answered). `label` names a
/// computer by its key.
pub fn footer_update(
    u: &UpdatesState,
    machines: usize,
    label: impl Fn(&str) -> String,
) -> FooterUpdate {
    // Only a run started here: a row a background read saw updating (on a
    // computer that may then go offline for good) is Computers' to show.
    if u.updating {
        let step = u.rows.iter().find_map(|(key, row)| match row {
            RowState::Updating(step) => Some((key.clone(), step.clone())),
            _ => None,
        });
        return match step {
            Some((key, step)) => FooterUpdate::Updating {
                line: format!("Updating {}…", label(&key)),
                step,
            },
            None => FooterUpdate::Updating {
                line: "Updating…".into(),
                step: String::new(),
            },
        };
    }
    if u.failed_run {
        return FooterUpdate::Failed;
    }
    if u.checking {
        return FooterUpdate::Checking;
    }
    if u.downloading {
        return FooterUpdate::Downloading;
    }
    // Downloaded wins over waiting, as in t3code: install what is ready.
    let ready = updates::installable_keys(u);
    if !ready.is_empty() {
        let to = match ready.as_slice() {
            [key] => match u.rows.get(key) {
                Some(RowState::Downloaded(p) | RowState::Behind(p)) => Some(p.to.clone()),
                _ => None,
            },
            _ => None,
        };
        return FooterUpdate::Downloaded {
            machines: ready.len(),
            to,
        };
    }
    let waiting = updates::waiting_keys(u);
    if !waiting.is_empty() {
        let commits = waiting
            .iter()
            .filter_map(|key| match u.rows.get(key) {
                Some(RowState::Behind(p)) => Some(p.behind),
                _ => None,
            })
            .max()
            .unwrap_or(0);
        return FooterUpdate::Available {
            machines: waiting.len(),
            commits,
        };
    }
    if !u.rows.is_empty()
        && u.rows.len() >= machines
        && u.rows
            .values()
            .all(|row| matches!(row, RowState::NotSetUp { .. }))
    {
        return FooterUpdate::Off;
    }
    let tip = if u.rows.values().any(|row| *row == RowState::UpToDate) {
        "Up to date. Check for updates".to_string()
    } else {
        "Check for updates".to_string()
    };
    FooterUpdate::Idle { tip }
}

/// The button's tooltip (and the hover card's heading), in t3code's words.
pub fn tip(state: &FooterUpdate) -> String {
    match state {
        FooterUpdate::Idle { tip } => tip.clone(),
        FooterUpdate::Checking => "Checking for updates…".into(),
        FooterUpdate::Available { .. } => "Update ready to download".into(),
        FooterUpdate::Downloading => "Downloading update".into(),
        FooterUpdate::Downloaded { to: Some(to), .. } => format!(
            "Update {} downloaded. Click to restart and install.",
            updates::short(to)
        ),
        FooterUpdate::Downloaded { machines, .. } if *machines > 1 => {
            format!("Updates downloaded on {machines} computers. Click to restart and install.")
        }
        FooterUpdate::Downloaded { .. } => {
            "Update downloaded. Click to restart and install.".into()
        }
        FooterUpdate::Updating { line, step } if step.is_empty() => {
            format!("{line} Press to see every computer")
        }
        FooterUpdate::Updating { line, step } => {
            format!("{line} {step}. Press to see every computer")
        }
        FooterUpdate::Failed => {
            "An update failed (Settings → Computers says why). Press to check again".into()
        }
        FooterUpdate::Off => "Updates are off (turn them on in Computers)".into(),
    }
}

/// A computer's name as the Computers page shows it.
fn computer_label(s: &State, key: &str) -> String {
    if key == THIS_MAC {
        return "This Mac".into();
    }
    crate::remote::saved(s)
        .iter()
        .find(|env| env.environment_id == key)
        .map_or_else(|| "a computer".into(), |env| env.label.clone())
}

/// How many computers this Mac updates: the saved ones, and This Mac
/// unless this is a Light bundle.
fn fleet_size(s: &State) -> usize {
    crate::remote::saved(s).len() + usize::from(!s.remote.app_only)
}

fn state_of(s: &State) -> FooterUpdate {
    footer_update(&s.remote.updates, fleet_size(s), |key| {
        computer_label(s, key)
    })
}

fn press(s: &mut State) {
    s.workspace.update_preview.open = false;
    match footer_update(&s.remote.updates, fleet_size(s), |_| String::new()) {
        // A failed run checks again: the computer that failed shows as
        // behind again (with what it would get) and can be retried with
        // the others; Computers keeps why it failed.
        FooterUpdate::Idle { .. } | FooterUpdate::Failed => updates::check(s),
        FooterUpdate::Checking | FooterUpdate::Downloading => {}
        FooterUpdate::Available { .. } => updates::download_waiting(s),
        FooterUpdate::Downloaded { .. } => {
            s.workspace.confirm_update = Some(updates::installable_keys(&s.remote.updates));
        }
        FooterUpdate::Updating { .. } | FooterUpdate::Off => {
            super::super::show(s, Page::Computers);
        }
    }
}

/// The pointer came to (`inside`) or left the button (`card`: the hover
/// card). Leaving closes the card a moment later unless it came back.
fn hover(s: &mut State, card: bool, inside: bool) {
    let preview = &mut s.workspace.update_preview;
    if card {
        preview.over_card = inside;
    } else {
        preview.over_button = inside;
    }
    preview.generation += 1;
    if inside {
        preview.open = true;
        return;
    }
    let generation = preview.generation;
    crate::jobs::after_if(s, PREVIEW_LINGER, move |s, _| {
        let preview = &mut s.workspace.update_preview;
        if preview.generation != generation || preview.over_button || preview.over_card {
            return false;
        }
        let was = preview.open;
        preview.open = false;
        was
    });
}

/// The footer's button: an icon while there is nothing to say, a capsule
/// with a line while there is. It takes the footer's free width.
pub fn button(s: &State, p: &Palette) -> El {
    let state = state_of(s);
    let tip = tip(&state);
    let (glyph, ink, line): (&'static rune::Icon, Color, Option<String>) = match &state {
        FooterUpdate::Idle { .. } | FooterUpdate::Checking => {
            (&icons::REFRESH_CW, p.text_muted, None)
        }
        FooterUpdate::Available { .. } | FooterUpdate::Downloading => {
            (&icons::DOWNLOAD, p.text, None)
        }
        FooterUpdate::Downloaded { .. } => (&icons::REFRESH_CW, p.text, None),
        FooterUpdate::Updating { line, .. } => {
            (&icons::REFRESH_CW, p.text_muted, Some(line.clone()))
        }
        FooterUpdate::Failed => (&icons::CIRCLE_ALERT, p.danger, Some("Update failed".into())),
        FooterUpdate::Off => (&icons::REFRESH_CW, p.text_faint, None),
    };
    let busy = matches!(
        state,
        FooterUpdate::Checking | FooterUpdate::Downloading | FooterUpdate::Updating { .. }
    );
    let mut mark =
        el().size(16., 16.)
            .center()
            .child(kit::busy_glyph("update-glyph", glyph, busy, 16., ink));
    match state {
        FooterUpdate::Available { .. } => {
            mark = mark.child(
                el().key("update-dot")
                    .transition(design::motion::glyph_pop())
                    .pin(None, Some(-2.), Some(-2.), None)
                    .size(6., 6.)
                    .radius(3.)
                    .fill(p.accent),
            );
        }
        // Downloaded: the restart glyph with a check badge, as in t3code.
        FooterUpdate::Downloaded { .. } => {
            mark = mark.child(
                el().key("update-ready")
                    .transition(design::motion::glyph_pop())
                    .pin(None, None, Some(-3.), Some(-3.))
                    .size(9., 9.)
                    .radius(4.5)
                    .fill(p.text)
                    .center()
                    .child(ui::glyph(&icons::CHECK, 7., p.bg)),
            );
        }
        _ => {}
    }
    let hover_wash = if p.light {
        Color::BLACK.alpha(0.08)
    } else {
        Color::WHITE.alpha(0.10)
    };
    let mut b = row()
        .key(format!("{PREVIEW}-trigger"))
        .role("button")
        .label(tip.clone())
        .h(design::SIDEBAR_CONTROL)
        .min_w(design::SIDEBAR_CONTROL)
        .radius(design::SIDEBAR_CONTROL / 2.)
        .center_y()
        .gap(8.)
        .cursor(match state {
            FooterUpdate::Checking | FooterUpdate::Downloading => Cursor::Default,
            _ => Cursor::Pointer,
        })
        .hover(hover_wash)
        .hover_motion(design::motion::HOVER, design::motion::HOVER)
        .press_scale(design::motion::PRESS)
        .on_press(press);
    // With something to preview, resting on the button opens the card;
    // otherwise the plain tooltip. The hover is followed in every state, so
    // leaving is never missed when the card comes or goes under the pointer.
    b = b.on_hover_change(|s: &mut State, inside, _tx: &mut Tx| hover(s, false, inside));
    if preview(s).is_none() {
        b = b.help(tip);
    }
    match line {
        None => b = b.w(design::SIDEBAR_CONTROL).justify_center().child(mark),
        Some(line) => {
            let fill = match state {
                FooterUpdate::Failed => p.danger.alpha(0.12),
                _ if p.light => Color::BLACK.alpha(0.05),
                _ => Color::WHITE.alpha(0.06),
            };
            b = b
                .shrink()
                .min_w(0.)
                .px(10.)
                .fill(fill)
                .transition(rune::Transition::opacity())
                .child(mark)
                .child(
                    ui::side_t(line, design::SIDEBAR_TITLE_SIZE, ink.alpha(0.9))
                        .truncate()
                        .shrink()
                        .min_w(0.),
                );
        }
    }
    b
}

/// What the hover card shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub title: String,
    /// The range: `from → to · N commits`.
    pub detail: Option<String>,
    /// Newest first.
    pub subjects: Vec<String>,
    /// Commits beyond the listed subjects.
    pub more: usize,
}

/// The card for the button's state, when it has changes to list: the
/// computer furthest behind among those the next press acts on.
pub fn preview_for(state: &FooterUpdate, u: &UpdatesState) -> Option<Preview> {
    let keys = match state {
        FooterUpdate::Available { .. } => updates::waiting_keys(u),
        FooterUpdate::Downloaded { .. } => updates::installable_keys(u),
        _ => return None,
    };
    let pending = keys
        .iter()
        .filter_map(|key| match u.rows.get(key) {
            Some(RowState::Behind(p) | RowState::Downloaded(p)) => Some(p),
            _ => None,
        })
        .max_by_key(|p| p.behind)?;
    if pending.subjects.is_empty() {
        return None;
    }
    let detail = match (&pending.from, pending.behind) {
        (Some(from), n) if n > 0 => Some(format!(
            "{} → {} · {}",
            updates::short(from),
            updates::short(&pending.to),
            commits(n)
        )),
        (_, n) if n > 0 => Some(format!("{} · {}", updates::short(&pending.to), commits(n))),
        _ => Some(updates::short(&pending.to).to_string()),
    };
    Some(Preview {
        title: tip(state),
        detail,
        subjects: pending.subjects.clone(),
        more: (pending.behind as usize).saturating_sub(pending.subjects.len()),
    })
}

fn preview(s: &State) -> Option<Preview> {
    preview_for(&state_of(s), &s.remote.updates)
}

/// The hover card over the button: the heading, then "What's changed".
pub fn preview_card(s: &State, k: &Kit, layer: &mut Vec<El>) {
    if !s.workspace.update_preview.open || s.workspace.confirm_update.is_some() {
        return;
    }
    let Some(preview) = preview(s) else {
        return;
    };
    let p = &s.palette;
    let mut head = col()
        .w_full()
        .gap(2.)
        .px(4.)
        .child(tw(preview.title, SM, ui::MEDIUM, p.text).wrap());
    if let Some(detail) = preview.detail {
        head = head.child(t(detail, XS, p.text_muted));
    }
    let mut list = col().w_full().gap(6.);
    for (ix, subject) in preview.subjects.iter().enumerate() {
        list = list.child(
            row()
                .key(format!("preview-subject-{ix}"))
                .w_full()
                .gap(8.)
                .items_start()
                .child(ui::dot(p.text_muted, 4.).mt(7.))
                .child(t(subject.clone(), XS, p.text).wrap().shrink().grow(1.)),
        );
    }
    let mut changes = col()
        .w_full()
        .gap(8.)
        .px(4.)
        .pt(12.)
        .child(tw("What's changed", XS, SEMIBOLD, p.text))
        .child(list);
    if preview.more > 0 {
        let more = match preview.more {
            1 => "Plus 1 more change.".to_string(),
            n => format!("Plus {n} more changes."),
        };
        changes = changes.child(tw(more, XS, SEMIBOLD, p.text));
    }
    let _ = HoverCard::new(PREVIEW)
        .open(true)
        .placement(Placement::TopStart)
        .gutter(8.)
        .width(360.)
        .role("dialog")
        .label("What's in the update")
        .on_hover(|s: &mut State, over| hover(s, true, over))
        .child(
            col()
                .key("update-preview-body")
                .w_full()
                .max_h(440.)
                .min_h(0.)
                .py(8.)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(head)
                .child(changes),
        )
        .build(k, layer);
}

/// "Update downloaded", for a few seconds after a download finished.
pub fn downloaded_toast(s: &State, k: &Kit, layer: &mut Vec<El>) {
    if !s.remote.updates.downloaded_toast {
        return;
    }
    let toast = Toast::new("update-downloaded", "Update downloaded")
        .description("Restart the app from the update button to install it.")
        .icon(k, &icons::CIRCLE_CHECK)
        .closable()
        .on_close(|s: &mut State| s.remote.updates.downloaded_toast = false)
        .build(k);
    layer.push(ToastRegion::new().toast(toast).build(k));
}

/// The computers the dialog lists, in the order an install runs them.
/// `keys`: the dialog's computers, already in that order.
fn listed(s: &State, keys: &[String]) -> Vec<(String, String, Pending)> {
    // The same rule the install dispatches by (`updates::install_job`), so
    // the dialog never lists a computer its button would skip.
    keys.iter()
        .filter(|key| updates::install_job(&s.remote.updates, key).is_some())
        .filter_map(|key| match s.remote.updates.rows.get(key) {
            Some(RowState::Downloaded(p) | RowState::Behind(p)) => {
                Some((key.clone(), computer_label(s, key), p.clone()))
            }
            _ => None,
        })
        .collect()
}

fn commits(n: u32) -> String {
    match n {
        1 => "1 commit".into(),
        n => format!("{n} commits"),
    }
}

/// The restart dialog's title and description.
pub fn heading(listed: &[(String, String, Pending)]) -> (String, String) {
    match listed {
        [(key, label, p)] => {
            let title = if key == THIS_MAC {
                "Install update and restart ZLO?".to_string()
            } else {
                format!("Install update and restart {label}?")
            };
            let range = match &p.from {
                Some(from) => format!("{} → {}", updates::short(from), updates::short(&p.to)),
                None => format!("to {}", updates::short(&p.to)),
            };
            let description = if p.behind > 0 {
                format!("{range} · {}", commits(p.behind))
            } else {
                range
            };
            (title, description)
        }
        many => {
            let most = many.iter().map(|(_, _, p)| p.behind).max().unwrap_or(0);
            (
                format!("Install updates on {} computers?", many.len()),
                if most > 0 {
                    format!("Up to {} behind", commits(most))
                } else {
                    "Each restarts on its new build".into()
                },
            )
        }
    }
}

/// The line saying what the install interrupts. `running`: the threads
/// running on the computer in use when it is one of those updating (ZLO
/// sees no other computer's threads).
pub fn warning(running: usize, this_mac: bool) -> String {
    let threads = match running {
        0 => "Any running tasks will be interrupted. ".to_string(),
        1 => "1 running thread here will be interrupted. ".into(),
        n => format!("{n} running threads here will be interrupted. "),
    };
    let restart = if this_mac {
        "ZLO quits and reopens on the new version."
    } else {
        "Each computer restarts its backend on the new version."
    };
    format!("{threads}{restart}")
}

/// "Install update and restart ZLO?": where, and what it interrupts.
/// Cancel and Escape leave everything as it was; only "Restart and
/// install" starts it.
pub fn confirm(s: &State, k: &Kit, layer: &mut Vec<El>) {
    let Some(keys) = &s.workspace.confirm_update else {
        return;
    };
    let p = &s.palette;
    let listed = listed(s, keys);
    if listed.is_empty() {
        // The update went away under the dialog (checked elsewhere, or
        // done); `set_row` closes it.
        return;
    }
    let run: Vec<String> = listed.iter().map(|(key, _, _)| key.clone()).collect();
    let active = s.onboarding.computer.key();
    let running = if run.contains(&active) {
        s.workspace.inbox.running()
    } else {
        0
    };
    let (title, description) = heading(&listed);
    let this_mac = listed.iter().any(|(key, _, _)| key == THIS_MAC);
    let mut body = col().w_full().gap(12.).px(16.).pb(16.);
    if listed.len() > 1 {
        let mut table = col().key("update-machines").w_full().gap(4.);
        for (key, label, pending) in &listed {
            let what = if pending.behind > 0 {
                commits(pending.behind)
            } else {
                updates::short(&pending.to).to_string()
            };
            table = table.child(
                row()
                    .key(format!("update-machine-{key}"))
                    .w_full()
                    .gap(12.)
                    .center_y()
                    .child(t(label.clone(), SM, p.text).truncate().shrink().grow(1.))
                    .child(t(what, XS, p.text_muted)),
            );
        }
        body = body.child(table);
    }
    body = body
        .child(
            t(warning(running, this_mac), XS, p.text_muted)
                .wrap()
                .w_full(),
        )
        .child(
            row()
                .w_full()
                .gap(8.)
                .justify_end()
                .child(
                    rune_kit::Button::new("Cancel")
                        .on_press(|s: &mut State| s.workspace.confirm_update = None)
                        .build(k),
                )
                .child(
                    rune_kit::Button::new("Restart and install")
                        .variant(rune_kit::ButtonVariant::Contrast)
                        .icon(&icons::REFRESH_CW)
                        .on_press(move |s: &mut State| {
                            s.workspace.confirm_update = None;
                            updates::install_keys(s, run.clone());
                        })
                        .build(k),
                ),
        );
    Dialog::new("confirm-update", title)
        .open(true)
        .fit()
        .width(440.)
        .description(description)
        .child(body)
        .on_close(|s: &mut State| s.workspace.confirm_update = None)
        .build(k, layer);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(behind: u32, subjects: &[&str]) -> Pending {
        Pending {
            behind,
            from: Some("a".repeat(40)),
            to: "b".repeat(40),
            subjects: subjects.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn label(key: &str) -> String {
        if key == THIS_MAC {
            "This Mac".into()
        } else {
            key.to_uppercase()
        }
    }

    #[test]
    fn the_button_says_what_is_going_on() {
        let mut u = UpdatesState::default();
        assert_eq!(
            footer_update(&u, 2, label),
            FooterUpdate::Idle {
                tip: "Check for updates".into()
            }
        );
        u.rows.insert(THIS_MAC.into(), RowState::UpToDate);
        assert_eq!(
            footer_update(&u, 2, label),
            FooterUpdate::Idle {
                tip: "Up to date. Check for updates".into()
            }
        );
        u.checking = true;
        assert_eq!(footer_update(&u, 2, label), FooterUpdate::Checking);
        u.checking = false;

        u.rows
            .insert(THIS_MAC.into(), RowState::Behind(pending(7, &[])));
        u.rows
            .insert("studio".into(), RowState::Behind(pending(2, &[])));
        let available = footer_update(&u, 2, label);
        assert_eq!(
            available,
            FooterUpdate::Available {
                machines: 2,
                commits: 7
            }
        );
        assert_eq!(tip(&available), "Update ready to download");

        u.downloading = true;
        assert_eq!(footer_update(&u, 2, label), FooterUpdate::Downloading);
        u.downloading = false;

        // One computer downloaded: install comes first, naming the build.
        u.rows
            .insert(THIS_MAC.into(), RowState::Downloaded(pending(7, &[])));
        let ready = footer_update(&u, 2, label);
        assert_eq!(
            ready,
            FooterUpdate::Downloaded {
                machines: 1,
                to: Some("b".repeat(40))
            }
        );
        assert_eq!(
            tip(&ready),
            "Update bbbbbbb downloaded. Click to restart and install."
        );
        // A backend too old to download is installed in one go with it.
        u.no_download.insert("studio".into());
        assert_eq!(
            footer_update(&u, 2, label),
            FooterUpdate::Downloaded {
                machines: 2,
                to: None
            }
        );

        // A row a background read saw updating is not a run of ours.
        u.rows
            .insert("studio".into(), RowState::Updating("building".into()));
        assert!(matches!(
            footer_update(&u, 2, label),
            FooterUpdate::Downloaded { machines: 1, .. }
        ));
        u.updating = true;
        assert_eq!(
            footer_update(&u, 2, label),
            FooterUpdate::Updating {
                line: "Updating STUDIO…".into(),
                step: "building".into()
            }
        );

        u.updating = false;
        u.rows
            .insert("studio".into(), RowState::Failed("dirty".into()));
        u.failed_run = true;
        assert_eq!(footer_update(&u, 2, label), FooterUpdate::Failed);
        u.failed_run = false;

        let mut off = UpdatesState::default();
        off.rows
            .insert(THIS_MAC.into(), RowState::NotSetUp { can_enable: true });
        assert_eq!(footer_update(&off, 1, label), FooterUpdate::Off);
        // A computer that never answered may have updates on: still a check.
        assert!(matches!(
            footer_update(&off, 2, label),
            FooterUpdate::Idle { .. }
        ));
    }

    #[test]
    fn the_hover_card_lists_what_changed_for_the_next_press() {
        let mut u = UpdatesState::default();
        u.rows.insert(
            THIS_MAC.into(),
            RowState::Behind(pending(5, &["Three", "Two"])),
        );
        u.rows
            .insert("studio".into(), RowState::Behind(pending(1, &["One"])));
        let state = footer_update(&u, 2, label);
        let card = preview_for(&state, &u).expect("a card");
        assert_eq!(card.title, "Update ready to download");
        assert_eq!(
            card.detail.as_deref(),
            Some("aaaaaaa → bbbbbbb · 5 commits")
        );
        assert_eq!(card.subjects, vec!["Three", "Two"], "the furthest behind");
        assert_eq!(card.more, 3);

        // Downloaded: the card speaks of the download.
        u.rows.insert(
            THIS_MAC.into(),
            RowState::Downloaded(pending(5, &["Three", "Two"])),
        );
        let state = footer_update(&u, 2, label);
        let card = preview_for(&state, &u).expect("a card");
        assert!(card.title.contains("Click to restart and install"));

        // Nothing to list: the plain tooltip instead.
        let mut bare = UpdatesState::default();
        bare.rows
            .insert(THIS_MAC.into(), RowState::Behind(pending(2, &[])));
        let state = footer_update(&bare, 1, label);
        assert_eq!(preview_for(&state, &bare), None);
        assert_eq!(
            preview_for(&FooterUpdate::Checking, &u),
            None,
            "only a waiting or downloaded update has a card"
        );
    }

    #[test]
    fn the_dialog_names_the_range_and_what_it_interrupts() {
        let one = vec![(
            THIS_MAC.to_string(),
            "This Mac".to_string(),
            pending(7, &[]),
        )];
        assert_eq!(
            heading(&one),
            (
                "Install update and restart ZLO?".to_string(),
                "aaaaaaa → bbbbbbb · 7 commits".to_string()
            )
        );
        let remote = vec![("studio".to_string(), "Studio".to_string(), pending(1, &[]))];
        assert_eq!(heading(&remote).0, "Install update and restart Studio?");
        assert_eq!(heading(&remote).1, "aaaaaaa → bbbbbbb · 1 commit");
        let unknown = vec![("studio".to_string(), "Studio".to_string(), pending(0, &[]))];
        assert_eq!(heading(&unknown).1, "aaaaaaa → bbbbbbb");
        let two = vec![remote[0].clone(), one[0].clone()];
        assert_eq!(
            heading(&two),
            (
                "Install updates on 2 computers?".to_string(),
                "Up to 7 commits behind".to_string()
            )
        );

        assert_eq!(
            warning(0, true),
            "Any running tasks will be interrupted. ZLO quits and reopens on the new version."
        );
        assert_eq!(
            warning(2, true),
            "2 running threads here will be interrupted. ZLO quits and reopens on the new version."
        );
        assert!(warning(1, false).starts_with("1 running thread here will"));
    }

    #[test]
    fn this_mac_is_listed_last() {
        let mut u = UpdatesState::default();
        u.rows
            .insert(THIS_MAC.into(), RowState::Downloaded(pending(1, &[])));
        u.rows
            .insert("zeta".into(), RowState::Downloaded(pending(1, &[])));
        u.rows.insert("alpha".into(), RowState::UpToDate);
        assert_eq!(
            updates::installable_keys(&u),
            vec!["zeta".to_string(), THIS_MAC.to_string()]
        );
    }
}
