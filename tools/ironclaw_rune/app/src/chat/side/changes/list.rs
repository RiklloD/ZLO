//! The Changes tab's file list (GPUI `changes.rs`'s `render_change_list`):
//! staged and unstaged files, each with its actions. A row scrolls the diff
//! to its file.

use rune::{El, GrowthSpec, ScrollSpec, col, el, icon, row};
use rune_kit::Kit;

use super::super::ui::{self, xs};
use super::{ChangesState, confirmed, git_action, jump};
use crate::chat::side::git;
use crate::design::smooth_scroll::WheelGlide;
use crate::icons;
use crate::state::State;

/// One line of the file list.
#[derive(Clone, Copy)]
enum Line {
    /// "Staged · N" (true) or "Changes · N" with its all-button.
    Section(bool),
    /// A file: index into `changes`, and whether it is the staged side.
    File(usize, bool),
}

fn state_of(s: &State) -> Option<&ChangesState> {
    super::side(s).map(|x| &x.changes)
}

fn section(s: &State, k: &Kit, state: &ChangesState, staged_side: bool) -> El {
    let p = &s.palette;
    let busy = state.busy.is_some();
    let all: Vec<String> = state
        .changes
        .iter()
        .filter(|c| {
            if staged_side {
                c.staged()
            } else {
                c.unstaged()
            }
        })
        .map(|c| c.path.clone())
        .collect();
    let (label, key, stage) = if staged_side {
        (format!("Staged · {}", all.len()), "unstage-all", false)
    } else {
        (format!("Changes · {}", all.len()), "stage-all", true)
    };
    {
        row()
            .w_full()
            .px(8.)
            .pt(8.)
            .gap(4.)
            .items_center()
            .child(ui::line(label, xs(), p.text_faint).grow(1.))
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
    }
}

fn change_row(
    s: &State,
    k: &Kit,
    state: &ChangesState,
    change: &git::Change,
    staged_row: bool,
) -> El {
    let p = &s.palette;
    let busy = state.busy.is_some();
    {
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
            .child(
                ui::line(
                    letter.to_string(),
                    xs(),
                    if armed { p.danger } else { color },
                )
                .w(12.),
            )
            .child(ui::line(name, xs(), p.text).max_w(180.))
            .child(ui::line(dir, xs(), p.text_faint).grow(1.).min_w(0.))
            .on_press_tx(move |s: &mut State, tx| jump(s, &to, tx))
            .child(actions)
    }
}

pub(super) fn view(s: &State, k: &Kit, state: &ChangesState) -> El {
    let p = &s.palette;
    let any_staged = state.changes.iter().any(|c| c.staged());
    let any_unstaged = state.changes.iter().any(|c| c.unstaged());
    if !any_staged && !any_unstaged {
        return col();
    }
    let hidden = state.list_hidden;
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
            xs(),
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
    let mut lines = Vec::new();
    for staged_side in [true, false] {
        let side: Vec<Line> = state
            .changes
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                if staged_side {
                    c.staged()
                } else {
                    c.unstaged()
                }
            })
            .map(|(ix, _)| Line::File(ix, staged_side))
            .collect();
        if !side.is_empty() {
            lines.push(Line::Section(staged_side));
            lines.extend(side);
        }
    }
    let lines = std::rc::Rc::new(lines);
    let (keys, kit) = (lines.clone(), k.clone());
    let count = lines.len();
    col().w_full().child(header).child(
        col()
            .key("changes-list")
            .w_full()
            .max_h(220.)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .wheel_glide("changes-list")
            .child(
                col()
                    .key("changes-rows")
                    .w_full()
                    .growth(GrowthSpec::default())
                    .virtual_rows(
                        count,
                        26.,
                        move |s: &State, i| {
                            let path = |ix: usize| {
                                state_of(s)
                                    .and_then(|st| st.changes.get(ix))
                                    .map_or("", |c| c.path.as_str())
                            };
                            match keys.get(i) {
                                Some(Line::Section(true)) => "section-staged".into(),
                                Some(Line::Section(false)) | None => "section-changes".into(),
                                Some(Line::File(ix, staged)) => format!(
                                    "file-{}-{}",
                                    if *staged { "s" } else { "u" },
                                    path(*ix)
                                )
                                .into(),
                            }
                        },
                        move |s: &State, i| {
                            let Some(state) = state_of(s) else {
                                return el();
                            };
                            let line = match lines.get(i) {
                                Some(Line::Section(staged)) => section(s, &kit, state, *staged),
                                Some(Line::File(ix, staged)) => match state.changes.get(*ix) {
                                    Some(c) => change_row(s, &kit, state, c, *staged),
                                    None => el(),
                                },
                                None => el(),
                            };
                            col().w_full().px(4.).child(line)
                        },
                    ),
            ),
    )
}
