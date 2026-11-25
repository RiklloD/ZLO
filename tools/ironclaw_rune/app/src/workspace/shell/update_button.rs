//! The sidebar footer's update button and its confirm dialog, after t3code's
//! `SidebarUpdatePill` and `UpdateEverythingDialog`.
//!
//! The button reads the update rows `remote::updates` keeps for every
//! computer (a background read each minute). Up to date, a press checks
//! again. With an update waiting, a press never starts it: it opens a
//! dialog saying what changes, on which computers, and what it interrupts,
//! and only its "Update" button starts the update. While an update runs, the
//! button grows into a capsule naming the step.

use rune::{Color, Cursor, El, ScrollSpec, col, el, row};
use rune_kit::{Dialog, Kit};
use zlo_client::environments::THIS_MAC;

use super::super::ui::{self, SEMIBOLD, SM, XS, t, tw};
use crate::design::{self, Palette, kit};
use crate::icons;
use crate::remote::updates::{self, Pending, RowState, UpdatesState};
use crate::state::{Page, State};

/// What the button shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FooterUpdate {
    /// Nothing waiting (or never read): a press checks.
    Idle {
        tip: String,
    },
    Checking,
    /// `machines` computers have an update waiting, `commits` at most.
    Available {
        machines: usize,
        commits: u32,
    },
    /// An update is running: `line` names the computer, `step` what it is
    /// doing (empty before it says).
    Updating {
        line: String,
        step: String,
    },
    /// The last update ended with a computer failing.
    Failed,
    /// Updates are off on every computer ZLO knows.
    Off,
}

/// The button's state. `label` names a computer by its key.
pub fn footer_update(u: &UpdatesState, label: impl Fn(&str) -> String) -> FooterUpdate {
    let step = u.rows.iter().find_map(|(key, row)| match row {
        RowState::Updating(step) => Some((key.clone(), step.clone())),
        _ => None,
    });
    if u.updating || step.is_some() {
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
    let waiting: Vec<&Pending> = u
        .rows
        .values()
        .filter_map(|row| match row {
            RowState::Behind(p) => Some(p),
            _ => None,
        })
        .collect();
    if !waiting.is_empty() {
        return FooterUpdate::Available {
            machines: waiting.len(),
            commits: waiting.iter().map(|p| p.behind).max().unwrap_or(0),
        };
    }
    if !u.rows.is_empty()
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

fn press(s: &mut State) {
    match footer_update(&s.remote.updates, |_| String::new()) {
        FooterUpdate::Idle { .. } => updates::check(s),
        FooterUpdate::Checking => {}
        FooterUpdate::Available { .. } => s.workspace.confirm_update = true,
        FooterUpdate::Failed if updates::waiting(&s.remote.updates) > 0 => {
            s.workspace.confirm_update = true;
        }
        FooterUpdate::Updating { .. } | FooterUpdate::Failed | FooterUpdate::Off => {
            super::super::show(s, Page::Computers);
        }
    }
}

/// The footer's button: an icon while there is nothing to say, a capsule
/// with a line while there is. It takes the footer's free width.
pub fn button(s: &State, p: &Palette) -> El {
    let state = footer_update(&s.remote.updates, |key| computer_label(s, key));
    let (glyph, ink, line, tip): (&'static rune::Icon, Color, Option<String>, String) = match &state
    {
        FooterUpdate::Idle { tip } => (&icons::REFRESH_CW, p.text_muted, None, tip.clone()),
        FooterUpdate::Checking => (
            &icons::REFRESH_CW,
            p.text_muted,
            None,
            "Checking for updates…".into(),
        ),
        FooterUpdate::Available { commits, .. } => (
            &icons::DOWNLOAD,
            p.text,
            Some("Update".into()),
            match commits {
                1 => "Update ready: 1 commit".into(),
                n => format!("Update ready: {n} commits"),
            },
        ),
        FooterUpdate::Updating { line, step } => (
            &icons::REFRESH_CW,
            p.text_muted,
            Some(line.clone()),
            if step.is_empty() {
                format!("{line} Press to see every computer")
            } else {
                format!("{line} {step}. Press to see every computer")
            },
        ),
        FooterUpdate::Failed => (
            &icons::CIRCLE_ALERT,
            p.danger,
            Some("Update failed".into()),
            "An update failed. Press to see why or retry".into(),
        ),
        FooterUpdate::Off => (
            &icons::REFRESH_CW,
            p.text_faint,
            None,
            "Updates are off (turn them on in Computers)".into(),
        ),
    };
    let busy = matches!(
        state,
        FooterUpdate::Checking | FooterUpdate::Updating { .. }
    );
    let mut mark =
        el().size(16., 16.)
            .center()
            .child(kit::busy_glyph("update-glyph", glyph, busy, 16., ink));
    if matches!(state, FooterUpdate::Available { .. }) {
        mark = mark.child(
            el().key("update-dot")
                .transition(design::motion::glyph_pop())
                .pin(None, Some(-2.), Some(-2.), None)
                .size(6., 6.)
                .radius(3.)
                .fill(p.accent),
        );
    }
    let hover = if p.light {
        Color::BLACK.alpha(0.08)
    } else {
        Color::WHITE.alpha(0.10)
    };
    let mut b = row()
        .key("footer-update")
        .role("button")
        .label(tip.clone())
        .help(tip)
        .h(design::SIDEBAR_CONTROL)
        .min_w(design::SIDEBAR_CONTROL)
        .radius(design::SIDEBAR_CONTROL / 2.)
        .center_y()
        .gap(8.)
        .cursor(if state == FooterUpdate::Checking {
            Cursor::Default
        } else {
            Cursor::Pointer
        })
        .hover(hover)
        .hover_motion(design::motion::HOVER, design::motion::HOVER)
        .press_scale(design::motion::PRESS)
        .on_press(press);
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

/// The computers the dialog lists, in the order an update runs them.
fn listed(s: &State) -> Vec<(String, String, Pending)> {
    updates::waiting_keys(&s.remote.updates)
        .into_iter()
        .filter_map(|key| match s.remote.updates.rows.get(&key) {
            Some(RowState::Behind(p)) => Some((key.clone(), computer_label(s, &key), p.clone())),
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

/// The confirm dialog's title and description.
pub fn heading(listed: &[(String, String, Pending)]) -> (String, String) {
    match listed {
        [(key, label, p)] => {
            let title = if key == THIS_MAC {
                "Update ZLO?".to_string()
            } else {
                format!("Update {label}?")
            };
            let range = match &p.from {
                Some(from) => format!("{} → {}", updates::short(from), updates::short(&p.to)),
                None => format!("to {}", updates::short(&p.to)),
            };
            (title, format!("{range} · {}", commits(p.behind)))
        }
        many => {
            let most = many.iter().map(|(_, _, p)| p.behind).max().unwrap_or(0);
            (
                format!("Update {} computers?", many.len()),
                format!("Up to {} behind", commits(most)),
            )
        }
    }
}

/// The line saying what the update interrupts.
pub fn warning(running: usize, this_mac: bool) -> String {
    let threads = match running {
        0 => String::new(),
        1 => "1 running thread will be interrupted. ".into(),
        n => format!("{n} running threads will be interrupted. "),
    };
    let restart = if this_mac {
        "ZLO quits and reopens when This Mac has finished."
    } else {
        "Each computer restarts its backend when it has finished."
    };
    format!("{threads}{restart}")
}

/// "Update ZLO?": what changes, where, and what it interrupts. Cancel and
/// Escape leave everything as it was; only "Update" starts it.
pub fn confirm(s: &State, k: &Kit, layer: &mut Vec<El>) {
    if !s.workspace.confirm_update {
        return;
    }
    let p = &s.palette;
    let listed = listed(s);
    if listed.is_empty() {
        // The update went away under the dialog (checked elsewhere, or
        // done): nothing to confirm.
        return;
    }
    let (title, description) = heading(&listed);
    let this_mac = listed.iter().any(|(key, _, _)| key == THIS_MAC);
    // What changes: the subjects of the computer furthest behind.
    let furthest = listed
        .iter()
        .max_by_key(|(_, _, p)| p.behind)
        .map(|(_, _, p)| p.clone());
    let mut body = col().w_full().gap(12.).px(16.).pb(16.);
    if listed.len() > 1 {
        let mut table = col().key("update-machines").w_full().gap(4.);
        for (key, label, pending) in &listed {
            table = table.child(
                row()
                    .key(format!("update-machine-{key}"))
                    .w_full()
                    .gap(12.)
                    .center_y()
                    .child(t(label.clone(), SM, p.text).truncate().shrink().grow(1.))
                    .child(t(commits(pending.behind), XS, p.text_muted)),
            );
        }
        body = body.child(table);
    }
    if let Some(pending) = furthest.filter(|p| !p.subjects.is_empty()) {
        let mut list = col().w_full().gap(6.);
        for (ix, subject) in pending.subjects.iter().enumerate() {
            list = list.child(
                row()
                    .key(format!("subject-{ix}"))
                    .w_full()
                    .gap(8.)
                    .items_start()
                    .child(ui::dot(p.text_faint, 4.).mt(7.))
                    .child(t(subject.clone(), XS, p.text).wrap().shrink().grow(1.)),
            );
        }
        let more = pending.behind as usize - pending.subjects.len().min(pending.behind as usize);
        if more > 0 {
            list = list.child(t(format!("and {more} more"), XS, p.text_muted));
        }
        body = body.child(
            col()
                .w_full()
                .gap(8.)
                .child(tw("What's changed", XS, SEMIBOLD, p.text_muted))
                .child(
                    col()
                        .key("update-subjects")
                        .w_full()
                        .max_h(240.)
                        .min_h(0.)
                        .shrink()
                        .scroll(ScrollSpec {
                            stick_bottom: false,
                        })
                        .child(list),
                ),
        );
    }
    body = body
        .child(
            t(
                warning(s.workspace.inbox.running(), this_mac),
                XS,
                p.text_muted,
            )
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
                        .on_press(|s: &mut State| s.workspace.confirm_update = false)
                        .build(k),
                )
                .child(
                    rune_kit::Button::new("Update")
                        .variant(rune_kit::ButtonVariant::Contrast)
                        .icon(&icons::DOWNLOAD)
                        .on_press(|s: &mut State| {
                            s.workspace.confirm_update = false;
                            updates::update_waiting(s);
                        })
                        .build(k),
                ),
        );
    Dialog::new("confirm-update", title)
        .open(true)
        .fit()
        .width(460.)
        .description(description)
        .child(body)
        .on_close(|s: &mut State| s.workspace.confirm_update = false)
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
            footer_update(&u, label),
            FooterUpdate::Idle {
                tip: "Check for updates".into()
            }
        );
        u.rows.insert(THIS_MAC.into(), RowState::UpToDate);
        assert_eq!(
            footer_update(&u, label),
            FooterUpdate::Idle {
                tip: "Up to date. Check for updates".into()
            }
        );
        u.checking = true;
        assert_eq!(footer_update(&u, label), FooterUpdate::Checking);
        u.checking = false;

        u.rows
            .insert(THIS_MAC.into(), RowState::Behind(pending(7, &[])));
        u.rows
            .insert("studio".into(), RowState::Behind(pending(2, &[])));
        assert_eq!(
            footer_update(&u, label),
            FooterUpdate::Available {
                machines: 2,
                commits: 7
            }
        );

        u.rows
            .insert("studio".into(), RowState::Updating("building".into()));
        assert_eq!(
            footer_update(&u, label),
            FooterUpdate::Updating {
                line: "Updating STUDIO…".into(),
                step: "building".into()
            }
        );
        u.rows
            .insert("studio".into(), RowState::Updating(String::new()));
        assert_eq!(
            footer_update(&u, label),
            FooterUpdate::Updating {
                line: "Updating STUDIO…".into(),
                step: String::new()
            }
        );

        u.rows
            .insert("studio".into(), RowState::Failed("dirty".into()));
        u.failed_run = true;
        assert_eq!(footer_update(&u, label), FooterUpdate::Failed);
        u.failed_run = false;

        let mut off = UpdatesState::default();
        off.rows
            .insert(THIS_MAC.into(), RowState::NotSetUp { can_enable: true });
        assert_eq!(footer_update(&off, label), FooterUpdate::Off);
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
                "Update ZLO?".to_string(),
                "aaaaaaa → bbbbbbb · 7 commits".to_string()
            )
        );
        let remote = vec![("studio".to_string(), "Studio".to_string(), pending(1, &[]))];
        assert_eq!(heading(&remote).0, "Update Studio?");
        assert_eq!(heading(&remote).1, "aaaaaaa → bbbbbbb · 1 commit");
        let two = vec![remote[0].clone(), one[0].clone()];
        assert_eq!(
            heading(&two),
            (
                "Update 2 computers?".to_string(),
                "Up to 7 commits behind".to_string()
            )
        );

        assert_eq!(
            warning(0, true),
            "ZLO quits and reopens when This Mac has finished."
        );
        assert_eq!(
            warning(2, true),
            "2 running threads will be interrupted. ZLO quits and reopens when This Mac has finished."
        );
        assert!(warning(1, false).starts_with("1 running thread will"));
    }

    #[test]
    fn this_mac_is_listed_last() {
        let mut u = UpdatesState::default();
        u.rows
            .insert(THIS_MAC.into(), RowState::Behind(pending(1, &[])));
        u.rows
            .insert("zeta".into(), RowState::Behind(pending(1, &[])));
        u.rows.insert("alpha".into(), RowState::UpToDate);
        assert_eq!(
            updates::waiting_keys(&u),
            vec!["zeta".to_string(), THIS_MAC.to_string()]
        );
    }
}
