//! The Changes tab's file list (GPUI `changes.rs`'s `render_change_list`):
//! staged and unstaged files, each with its actions. A row scrolls the diff
//! to its file.

use rune::{El, ScrollSpec, col, icon, row};
use rune_kit::Kit;

use super::super::ui::{self, XS};
use super::{ChangesState, confirmed, git_action, jump};
use crate::chat::side::git;
use crate::design::smooth_scroll::WheelGlide;
use crate::icons;
use crate::state::State;

pub(super) fn view(s: &State, k: &Kit, state: &ChangesState) -> El {
    let p = &s.palette;
    let staged: Vec<&git::Change> = state.changes.iter().filter(|c| c.staged()).collect();
    let unstaged: Vec<&git::Change> = state.changes.iter().filter(|c| c.unstaged()).collect();
    if staged.is_empty() && unstaged.is_empty() {
        return col();
    }
    let hidden = state.list_hidden;
    let busy = state.busy.is_some();
    let section = |label: String, key: &'static str, all: Vec<String>, stage: bool| {
        row()
            .w_full()
            .px(8.)
            .pt(8.)
            .gap(4.)
            .items_center()
            .child(ui::line(label, XS, p.text_faint).grow(1.))
            .child(
                ui::btn(if stage { "Stage all" } else { "Unstage all" })
                    .disabled(busy)
                    .on_press(move |s: &mut State| {
                        let paths = all.clone();
                        git_action(s, "stage", move |repo| {
                            if stage {
                                git::stage(repo, &paths)?;
                            } else {
                                git::unstage(repo, &paths)?;
                            }
                            Ok(None)
                        })
                    })
                    .build(k)
                    .key(key),
            )
    };
    let change_row = |change: &git::Change, staged_row: bool| {
        let letter = change.letter();
        let color = match letter {
            'M' => p.warning,
            'A' | 'U' => p.success,
            'D' => p.danger,
            _ => p.accent,
        };
        let (dir, name) = match change.path.rsplit_once('/') {
            Some((d, n)) => (d.to_string(), n.to_string()),
            None => (String::new(), change.path.clone()),
        };
        let path = change.path.clone();
        let to = change.path.clone();
        let c = change.clone();
        let discard_key = format!("discard:{}", change.path);
        let armed = state.confirm.as_deref() == Some(discard_key.as_str());
        let id = if staged_row { "s" } else { "u" };
        let actions = if staged_row {
            ui::ibtn(&icons::MINUS, "Unstage")
                .disabled(busy)
                .on_press(move |s: &mut State| {
                    let paths = vec![path.clone()];
                    git_action(s, "stage", move |repo| {
                        git::unstage(repo, &paths).map(|()| None)
                    })
                })
                .build(k)
                .key(format!("unstage-{}", change.path))
        } else {
            let stage_path = path.clone();
            row()
                .child(
                    ui::ibtn(
                        if armed {
                            &icons::TRIANGLE_ALERT
                        } else {
                            &icons::UNDO_2
                        },
                        if armed {
                            "Click again to discard"
                        } else {
                            "Discard changes"
                        },
                    )
                    .label("Discard changes")
                    .variant(if armed {
                        rune_kit::IconButtonVariant::Neutral
                    } else {
                        rune_kit::IconButtonVariant::Ghost
                    })
                    .disabled(busy)
                    .on_press(move |s: &mut State| {
                        if !confirmed(s, &discard_key) {
                            return;
                        }
                        let change = c.clone();
                        git_action(s, "discard", move |repo| {
                            git::discard(repo, &change).map(|()| None)
                        })
                    })
                    .build(k)
                    .key(format!("discard-{}", change.path)),
                )
                .child(
                    ui::ibtn(&icons::PLUS, "Stage")
                        .disabled(busy)
                        .on_press(move |s: &mut State| {
                            let paths = vec![stage_path.clone()];
                            git_action(s, "stage", move |repo| {
                                git::stage(repo, &paths).map(|()| None)
                            })
                        })
                        .build(k)
                        .key(format!("stage-{}", change.path)),
                )
        };
        ui::hover_row(s, format!("change-{id}-{}", change.path))
            .min_h(24.)
            .gap(6.)
            .child(ui::line(letter.to_string(), XS, if armed { p.danger } else { color }).w(12.))
            .child(ui::line(name, XS, p.text).max_w(180.))
            .child(ui::line(dir, XS, p.text_faint).grow(1.).min_w(0.))
            .on_press_tx(move |s: &mut State, tx| jump(s, &to, tx))
            .child(actions)
    };
    let count = state.changes.len();
    let header = row()
        .key("changes-list-toggle")
        .w_full()
        .px(8.)
        .pt(8.)
        .gap(4.)
        .items_center()
        .cursor(rune::Cursor::Pointer)
        .child(
            icon(
                if hidden {
                    &icons::CHEVRON_RIGHT
                } else {
                    &icons::CHEVRON_DOWN
                },
                12.,
            )
            .color(p.text_muted),
        )
        .child(ui::line(
            format!("{count} changed file{}", if count == 1 { "" } else { "s" }),
            XS,
            p.text_muted,
        ))
        .on_press(|s: &mut State| {
            if let Some(x) = super::side_mut(s) {
                x.changes.list_hidden = !x.changes.list_hidden;
            }
        });
    if hidden {
        return header;
    }
    let mut list = col().w_full().px(4.);
    if !staged.is_empty() {
        let all = staged.iter().map(|c| c.path.clone()).collect();
        list = list.child(section(
            format!("Staged · {}", staged.len()),
            "unstage-all",
            all,
            false,
        ));
        for c in &staged {
            list = list.child(change_row(c, true));
        }
    }
    if !unstaged.is_empty() {
        let all = unstaged.iter().map(|c| c.path.clone()).collect();
        list = list.child(section(
            format!("Changes · {}", unstaged.len()),
            "stage-all",
            all,
            true,
        ));
        for c in &unstaged {
            list = list.child(change_row(c, false));
        }
    }
    col().w_full().child(header).child(
        col()
            .key("changes-list")
            .w_full()
            .max_h(220.)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .wheel_glide("changes-list")
            .child(list),
    )
}
