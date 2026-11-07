//! The Changes tab's diff annotations (review comments, Codex findings,
//! the comment draft) and its scene fixtures (GPUI `changes.rs`'s
//! `render_annotation`).

use rune::{El, col, el, icon, row};
use rune_kit::diff::{Annotation, DiffState, LineRange, LineRef, Side};
use rune_kit::{Kit, TextInput};

use super::super::ui::{self, xs, xs_med};
use super::{Comment, codex, save_draft, stats, sync_annotations};
use crate::chat::side::{SideState, git, side, side_mut, side_total};
use crate::icons;
use crate::state::State;

pub(super) fn annotation(s: &State, k: &Kit, a: &Annotation) -> El {
    let p = &s.palette;
    let base = col()
        .mx(8.)
        .my(4.)
        .padding(8.)
        .radius(8.)
        .fill(p.surface_raised)
        .border(1., p.accent.alpha(0.4));
    if let Some(ix) =
        a.id.strip_prefix("finding-")
            .and_then(|n| n.parse::<usize>().ok())
    {
        let findings = codex::findings(s);
        let Some((_, f)) = findings.get(ix) else {
            return el();
        };
        let tint = codex::priority_color(f.priority, p);
        let mut body = col().gap(2.).child(
            row()
                .gap(4.)
                .child(ui::line(
                    format!("Codex {}", f.priority_label()),
                    xs(),
                    tint,
                ))
                .child(ui::line(f.title.clone(), xs_med(), p.text).shrink()),
        );
        if !f.body.is_empty() {
            body = body.child(ui::para(f.body.clone(), xs(), p.text_muted));
        }
        return base.border(1., tint.alpha(0.5)).child(body);
    }
    if let Some(id) =
        a.id.strip_prefix("comment-")
            .and_then(|n| n.parse::<u64>().ok())
    {
        let text = side(s)
            .and_then(|x| x.changes.comments.iter().find(|c| c.id == id))
            .map(|c| c.text.clone())
            .unwrap_or_default();
        return base.child(
            row()
                .gap(8.)
                .items_start()
                .child(icon(&icons::MESSAGE_SQUARE_PLUS, 12.).color(p.accent))
                .child(ui::para(text, xs(), p.text).grow(1.))
                .child(
                    ui::ibtn(&icons::CLOSE, "Remove comment")
                        .on_press(move |s: &mut State| {
                            if let Some(x) = side_mut(s) {
                                x.changes.comments.retain(|c| c.id != id);
                            }
                            sync_annotations(s);
                        })
                        .build(k)
                        .key(format!("comment-remove-{id}")),
                ),
        );
    }
    let Some(draft) = side(s).and_then(|x| x.changes.draft.as_ref()) else {
        return el();
    };
    base.gap(4.)
        .child(
            TextInput::new(&draft.input)
                .width(None)
                .placeholder("Comment for the agent")
                .label("Comment for the agent")
                .on_edit(|s: &mut State| match side_total(s).changes.draft.as_mut() {
                    Some(d) => &mut d.input,
                    None => Box::leak(Box::default()),
                })
                .on_submit(|s: &mut State, _| save_draft(s))
                .build(k)
                .key("comment-draft-input"),
        )
        .child(
            row()
                .gap(4.)
                .justify_end()
                .child(
                    ui::btn("Cancel")
                        .on_press(|s: &mut State| {
                            if let Some(x) = side_mut(s) {
                                x.changes.draft = None;
                            }
                            sync_annotations(s);
                        })
                        .build(k)
                        .key("comment-cancel"),
                )
                .child(
                    ui::primary("Comment")
                        .on_press(save_draft)
                        .build(k)
                        .key("comment-save"),
                ),
        )
}

/// A fixture: the working diff from a patch, as a refresh would land it.
pub(in crate::chat::side) fn fixture(
    x: &mut SideState,
    branch: &str,
    changes: Vec<git::Change>,
    patch: &str,
    turn: Option<&str>,
) {
    let state = &mut x.changes;
    state.loaded = true;
    state.branch = git::Branch {
        name: branch.into(),
        upstream: Some(format!("origin/{branch}")),
        ahead: 2,
        behind: 0,
    };
    state.changes = changes;
    state.working_patch = patch.into();
    state.shown = patch.into();
    state.diff = Some(DiffState::new(rune_kit::diff::parse(patch)));
    if let Some(turn) = turn {
        state.turn_stats = Some(stats(&rune_kit::diff::parse(turn)));
        state.turn_patch = turn.into();
    }
}

/// A fixture comment on line `line` (new side) of the diff's first file.
pub(in crate::chat::side) fn fixture_comment(x: &mut SideState, line: u32, text: &str) {
    let state = &mut x.changes;
    let path = state
        .diff
        .as_ref()
        .and_then(|d| d.files().first())
        .map(|f| f.path().to_string())
        .unwrap_or_default();
    state.next_comment += 1;
    let at = LineRef::new(Side::New, line);
    state.comments.push(Comment {
        id: state.next_comment,
        range: LineRange::line(0, at),
        path,
        text: text.into(),
    });
    if let Some(diff) = state.diff.as_mut() {
        diff.set_annotations(vec![Annotation::new(
            0,
            at,
            format!("comment-{}", state.next_comment),
        )]);
    }
}
