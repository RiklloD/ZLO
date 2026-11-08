//! Activity: what the agent is doing and what waits for you (GPUI
//! `chat/side/activity.rs`). MonoCode's Activity tab as a checklist per
//! turn: every tool call, approval and run notice of the conversation,
//! newest turn first, read from the same live rows the transcript draws.
//! Open approvals sit on top with their buttons.

use std::collections::HashSet;

use zlo_client::GateDecision;
use zlo_client::contract::ProductGateKind;
use zlo_client::transcript::{GateState, Row, ToolStatus};
use rune::{El, col, el, icon, row};
use rune_kit::{Kit, Spinner};

use super::ui::{self, sm, sm_med, xs, xs_med};
use super::{bridge, side, side_mut};
use crate::icons;
use crate::state::State;

/// Older turns load on demand, a few at a time.
const TURNS_PAGE: usize = 6;

#[derive(Default)]
pub(super) struct ActivityState {
    /// Steps the reader expanded, by row key.
    open: HashSet<String>,
    /// Turns shown beyond the first page.
    more: usize,
}

impl ActivityState {
    /// Expand a step, as a click on it does (scenes).
    pub(super) fn open_step(&mut self, key: &str) {
        self.open.insert(key.to_string());
    }
}

/// One turn: its prompt and the steps under it.
struct Turn<'a> {
    prompt: &'a str,
    steps: Vec<&'a Row>,
}

fn turns(rows: &[Row]) -> Vec<Turn<'_>> {
    let mut out: Vec<Turn> = Vec::new();
    for row in rows {
        match row {
            Row::User { text, .. } => out.push(Turn {
                prompt: text,
                steps: Vec::new(),
            }),
            Row::Tool { .. } | Row::Gate { .. } | Row::RunNotice { .. } | Row::Skills { .. } => {
                if out.is_empty() {
                    out.push(Turn {
                        prompt: "",
                        steps: Vec::new(),
                    });
                }
                if let Some(turn) = out.last_mut() {
                    turn.steps.push(row);
                }
            }
            _ => {}
        }
    }
    out.retain(|t| !t.steps.is_empty());
    out.reverse();
    out
}

fn is_open_gate(r: &Row) -> bool {
    matches!(
        r,
        Row::Gate {
            state: GateState::Open,
            ..
        }
    )
}

/// Open approvals and questions, for the tab badge.
pub(super) fn waiting(s: &State) -> usize {
    bridge::rows(s).iter().filter(|r| is_open_gate(r)).count()
}

pub(super) fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let rows = bridge::rows(s);
    let running = bridge::running(s);
    let waiting: Vec<&Row> = rows.iter().filter(|r| is_open_gate(r)).collect();
    let turns = turns(rows);
    let mut list = col().w_full();

    if running {
        list = list.child(
            row()
                .key("activity-working")
                .mx(12.)
                .mt(4.)
                .px(12.)
                .py(8.)
                .gap(8.)
                .items_center()
                .radius(8.)
                .fill(p.accent_wash)
                .child(Spinner::new().size(12.).build(k))
                .child(ui::line("Working", sm(), p.text).grow(1.))
                .child(
                    ui::outline("Stop")
                        .icon(&icons::SQUARE)
                        .on_press(bridge::stop)
                        .build(k)
                        .key("activity-stop"),
                ),
        );
    }

    if !waiting.is_empty() {
        list = list.child(ui::heading(s, format!("Needs you · {}", waiting.len())));
        for row in waiting {
            list = list.child(waiting_card(s, k, row));
        }
    }

    if turns.is_empty() && !running {
        let empty = ui::empty(
            s,
            "No steps yet. Tool calls, approvals and failures of this conversation show up here as they happen.",
        );
        return ui::scroll("activity-list", list.child(empty));
    }

    let shown = TURNS_PAGE + side(s).map_or(0, |x| x.activity.more);
    let total = turns.len();
    for (ix, turn) in turns.into_iter().take(shown).enumerate() {
        let failed = turn
            .steps
            .iter()
            .filter(|s| {
                matches!(
                    s,
                    Row::Tool {
                        status: ToolStatus::Failed,
                        ..
                    }
                ) || matches!(s, Row::RunNotice { failed: true, .. })
            })
            .count();
        let prompt = if turn.prompt.is_empty() {
            "Earlier".to_string()
        } else {
            turn.prompt.lines().next().unwrap_or_default().to_string()
        };
        let summary = match failed {
            0 => format!("{} steps", turn.steps.len()),
            n => format!("{} steps · {n} failed", turn.steps.len()),
        };
        let mut group = col().px(12.).pt(12.).gap(2.).child(
            row()
                .gap(8.)
                .items_center()
                .child(
                    ui::line(
                        prompt,
                        xs_med(),
                        if ix == 0 { p.text } else { p.text_muted },
                    )
                    .grow(1.)
                    .min_w(0.),
                )
                .child(ui::line(summary, xs(), p.text_faint)),
        );
        for step in turn.steps {
            group = group.child(step_row(s, step));
        }
        list = list.child(group.key(format!("activity-turn-{ix}")));
    }
    if total > shown {
        list = list.child(
            row().px(12.).pt(8.).child(
                ui::btn(format!(
                    "Show {} older turns",
                    (total - shown).min(TURNS_PAGE)
                ))
                .on_press(|s: &mut State| {
                    if let Some(x) = side_mut(s) {
                        x.activity.more += TURNS_PAGE;
                    }
                })
                .build(k)
                .key("activity-more"),
            ),
        );
    }
    ui::scroll("activity-list", list)
}

/// An open gate as a card with its answers.
fn waiting_card(s: &State, k: &Kit, row_: &Row) -> El {
    let p = &s.palette;
    let Row::Gate {
        run_id,
        gate_ref,
        kind,
        headline,
        body,
        allow_always,
        reason,
        ..
    } = row_
    else {
        return el();
    };
    let approval = *kind == ProductGateKind::Approval;
    let answer = |label: &'static str, decision: GateDecision, primary: bool| {
        let (run, gate) = (run_id.clone(), gate_ref.clone());
        let b = if primary {
            ui::primary(label)
        } else {
            ui::outline(label)
        };
        b.on_press(move |s: &mut State| {
            bridge::resolve_gate(s, run.clone(), gate.clone(), decision)
        })
        .build(k)
        .key(format!("gate-{label}-{gate_ref}"))
    };
    let mut card = col()
        .key(format!("activity-gate-{gate_ref}"))
        .mx(12.)
        .mb(8.)
        .padding(12.)
        .gap(8.)
        .radius(8.)
        .fill(p.warning.alpha(0.08))
        .border(1., p.warning.alpha(0.5))
        .child(
            row()
                .gap(8.)
                .items_start()
                .child(icon(&icons::TRIANGLE_ALERT, 12.).color(p.warning).mt(2.))
                .child(ui::para(headline.clone(), sm_med(), p.text).grow(1.)),
        );
    if let Some(text) = reason.clone().or(body.clone()) {
        card = card.child(ui::para(text, xs(), p.text_muted).max_lines(4));
    }
    card = card.child(if approval {
        let mut answers = row().gap(4.).child(answer(
            "Approve",
            GateDecision::Approve { always: false },
            true,
        ));
        if *allow_always {
            answers = answers.child(answer(
                "Always",
                GateDecision::Approve { always: true },
                false,
            ));
        }
        let (run, gate) = (run_id.clone(), gate_ref.clone());
        answers
            .child(answer("Deny", GateDecision::Decline, false))
            .child(
                ui::outline("Reject…")
                    .on_press(move |s: &mut State| {
                        bridge::start_reject(s, run.clone(), gate.clone())
                    })
                    .build(k)
                    .key(format!("gate-reject-{gate_ref}")),
            )
    } else {
        ui::line("Answer it in the conversation.", xs(), p.text_faint)
    });
    // "No, do X instead", answered right here when opened from here.
    if approval && let Some(editor) = bridge::reject_editor(s, k, gate_ref) {
        card = card.child(editor);
    }
    card
}

/// One checklist line: status mark, title, and its detail when opened.
fn step_row(s: &State, row_: &Row) -> El {
    let p = &s.palette;
    let mark = |i: &'static rune::Icon, c| icon(i, 12.).color(c);
    let (key, mark, title, detail): (&str, El, String, Option<String>) = match row_ {
        Row::Tool {
            key,
            title,
            detail,
            status,
            reason,
        } => {
            let mark = match status {
                ToolStatus::Running => {
                    mark(&icons::LOADER_CIRCLE, p.text_muted).looping(rune::LoopSpec::rotation(1.0))
                }
                ToolStatus::Completed => mark(&icons::CHECK, p.success),
                ToolStatus::Failed => mark(&icons::CIRCLE_X, p.danger),
            };
            let detail = match (reason, detail) {
                (Some(r), Some(d)) => Some(format!("{r}\n\n{d}")),
                (r, d) => r.clone().or(d.clone()),
            };
            (key, mark, title.clone(), detail)
        }
        Row::Gate {
            key,
            headline,
            state,
            ..
        } => {
            let (i, color, verb) = match state {
                GateState::Open => (&icons::TRIANGLE_ALERT, p.warning, "Waiting"),
                GateState::Resolving(_) => (&icons::LOADER, p.text_muted, "Answering"),
                GateState::Closed(Some(GateDecision::Decline)) => (&icons::BAN, p.danger, "Denied"),
                GateState::Closed(Some(_)) => (&icons::CIRCLE_CHECK, p.success, "Approved"),
                GateState::Closed(None) => (&icons::CIRCLE_CHECK, p.text_faint, "Answered"),
            };
            (key, mark(i, color), format!("{verb}: {headline}"), None)
        }
        Row::RunNotice {
            key, text, failed, ..
        } => (
            key,
            if *failed {
                mark(&icons::CIRCLE_ALERT, p.danger)
            } else {
                mark(&icons::INFO, p.text_faint)
            },
            text.lines().next().unwrap_or_default().to_string(),
            text.contains('\n').then(|| text.clone()),
        ),
        Row::Skills { key, names } => (
            key,
            mark(&icons::BOOK_OPEN, p.text_faint),
            format!("Skills: {}", names.join(", ")),
            None,
        ),
        _ => return el(),
    };
    let open = side(s).is_some_and(|x| x.activity.open.contains(key));
    let mut line = row()
        .key(format!("step-{key}"))
        .gap(8.)
        .px(4.)
        .py(3.)
        .items_center()
        .radius(6.)
        .child(row().w(14.).justify_center().child(mark))
        .child(ui::line(title, xs(), p.text_muted).grow(1.).min_w(0.));
    if detail.is_some() {
        let toggle_key = key.to_string();
        line = line
            .hover(p.hover)
            .cursor(rune::Cursor::Pointer)
            .on_press(move |s: &mut State| {
                if let Some(x) = side_mut(s)
                    && !x.activity.open.remove(&toggle_key)
                {
                    x.activity.open.insert(toggle_key.clone());
                }
            });
    }
    let mut out = col().child(line);
    if open && let Some(detail) = detail {
        out = out.child(
            el().ml(22.)
                .mb(4.)
                .padding(8.)
                .radius(6.)
                .fill(p.hover)
                .child(ui::para(detail, xs(), p.text_muted)),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(t: &str) -> Row {
        Row::User {
            key: t.into(),
            text: t.into(),
            pending: false,
            attachments: Vec::new(),
        }
    }

    fn tool(k: &str) -> Row {
        Row::Tool {
            key: k.into(),
            title: k.into(),
            detail: None,
            status: ToolStatus::Completed,
            reason: None,
        }
    }

    #[test]
    fn turns_group_steps_newest_first() {
        let rows = [
            tool("early"),
            user("a"),
            tool("t1"),
            user("b"),
            user("c"),
            tool("t2"),
        ];
        let t = turns(&rows);
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].prompt, "c");
        assert_eq!(t[2].prompt, "");
    }
}
