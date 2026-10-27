//! Activity: what the agent is doing and what waits for you. MonoCode's
//! Activity tab as a checklist per turn: every tool call, approval and run
//! notice of the conversation, newest turn first, read from the same live
//! rows the transcript draws. Open approvals sit on top with their buttons.

use std::collections::HashSet;

use ironclaw_gpui_client::GateDecision;
use ironclaw_gpui_client::contract::ProductGateKind;
use ironclaw_gpui_client::transcript::{GateState, Row, ToolStatus};

use super::{ChatView, empty, heading, scroll};
use crate::design::{Palette, kit};
use crate::ui::*;

/// Older turns load on demand, a few at a time.
const TURNS_PAGE: usize = 6;

#[derive(Default)]
pub(super) struct State {
    /// Steps the reader expanded, by row key.
    open: HashSet<String>,
    /// Turns shown beyond the first page.
    more: usize,
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

impl ChatView {
    /// Open approvals and questions, for the tab badge.
    pub(super) fn activity_waiting(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    Row::Gate {
                        state: GateState::Open,
                        ..
                    }
                )
            })
            .count()
    }

    pub(super) fn render_activity(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let rows = self.rows.clone();
        let running = self.running_run().is_some();
        let waiting: Vec<&Row> = rows
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    Row::Gate {
                        state: GateState::Open,
                        ..
                    }
                )
            })
            .collect();
        let turns = turns(&rows);
        let mut col = scroll("activity-list");

        if running {
            col = col.child(
                h_flex()
                    .mx_3()
                    .mt_1()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .rounded(px(8.))
                    .bg(p.accent_wash)
                    .child(kit::pulse_dots("activity-working", p.accent))
                    .child(div().flex_1().text_sm().child("Working"))
                    .child(
                        Button::new("activity-stop")
                            .label("Stop")
                            .icon(IconName::Square)
                            .xsmall()
                            .outline()
                            .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                    ),
            );
        }

        if !waiting.is_empty() {
            col = col.child(heading(format!("Needs you · {}", waiting.len()), cx));
            for row in waiting {
                col = col.child(self.render_waiting(row, cx));
            }
        }

        if turns.is_empty() && !running {
            return col
                .child(empty(
                    "No steps yet. Tool calls, approvals and failures of this conversation show up here as they happen.",
                    cx,
                ))
                .into_any_element();
        }

        let shown = TURNS_PAGE + self.side.activity.more;
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
            col = col.child(
                v_flex()
                    .px_3()
                    .pt_3()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .text_xs()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                                    .text_color(if ix == 0 { p.text } else { p.text_muted })
                                    .child(prompt),
                            )
                            .child(div().flex_none().text_color(p.text_faint).child(summary)),
                    )
                    .children(turn.steps.into_iter().map(|row| self.render_step(row, cx))),
            );
        }
        if total > shown {
            col = col.child(
                h_flex().px_3().pt_2().child(
                    Button::new("activity-more")
                        .label(format!(
                            "Show {} older turns",
                            (total - shown).min(TURNS_PAGE)
                        ))
                        .ghost()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.side.activity.more += TURNS_PAGE;
                            cx.notify();
                        })),
                ),
            );
        }
        col.into_any_element()
    }

    /// An open gate as a card with its answers.
    fn render_waiting(&self, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let Row::Gate {
            run_id,
            gate_ref,
            kind,
            headline,
            body,
            allow_always,
            reason,
            ..
        } = row
        else {
            return div().into_any_element();
        };
        let approval = *kind == ProductGateKind::Approval;
        let id = gate_ref.clone();
        let answer = |label: &'static str, decision: GateDecision, primary: bool| {
            let (run, gate) = (run_id.clone(), gate_ref.clone());
            Button::new(SharedString::from(format!("gate-{label}-{id}")))
                .label(label)
                .xsmall()
                .when(primary, |b| b.primary())
                .when(!primary, |b| b.outline())
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.resolve_gate(run.clone(), gate.clone(), decision, cx)
                }))
        };
        v_flex()
            .mx_3()
            .mb_2()
            .p_3()
            .gap_2()
            .rounded(px(8.))
            .border_1()
            .border_color(p.warning.opacity(0.5))
            .bg(p.warning.opacity(0.08))
            .child(
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(
                        Icon::new(IconName::TriangleAlert)
                            .xsmall()
                            .text_color(p.warning)
                            .mt(px(2.)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .whitespace_normal()
                            .child(headline.clone()),
                    ),
            )
            .when_some(reason.clone().or(body.clone()), |d, text| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(p.text_muted)
                        .whitespace_normal()
                        .line_clamp(4)
                        .child(text),
                )
            })
            .child(if approval {
                h_flex()
                    .gap_1()
                    .child(answer(
                        "Approve",
                        GateDecision::Approve { always: false },
                        true,
                    ))
                    .when(*allow_always, |d| {
                        d.child(answer(
                            "Always",
                            GateDecision::Approve { always: true },
                            false,
                        ))
                    })
                    .child(answer("Deny", GateDecision::Decline, false))
                    .into_any_element()
            } else {
                div()
                    .text_xs()
                    .text_color(p.text_faint)
                    .child("Answer it in the conversation.")
                    .into_any_element()
            })
            .into_any_element()
    }

    /// One checklist line: status mark, title, and its detail when opened.
    fn render_step(&self, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let (key, mark, title, detail): (&str, AnyElement, String, Option<String>) = match row {
            Row::Tool {
                key,
                title,
                detail,
                status,
                reason,
            } => {
                let mark = match status {
                    ToolStatus::Running => Spinner::new().xsmall().into_any_element(),
                    ToolStatus::Completed => Icon::new(IconName::Check)
                        .xsmall()
                        .text_color(p.success)
                        .into_any_element(),
                    ToolStatus::Failed => Icon::new(IconName::CircleX)
                        .xsmall()
                        .text_color(p.danger)
                        .into_any_element(),
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
                let (icon, color, verb) = match state {
                    GateState::Open => (IconName::TriangleAlert, p.warning, "Waiting"),
                    GateState::Resolving(_) => (IconName::Loader, p.text_muted, "Answering"),
                    GateState::Closed(Some(GateDecision::Decline)) => {
                        (IconName::Ban, p.danger, "Denied")
                    }
                    GateState::Closed(Some(_)) => (IconName::CircleCheck, p.success, "Approved"),
                    GateState::Closed(None) => (IconName::CircleCheck, p.text_faint, "Answered"),
                };
                (
                    key,
                    Icon::new(icon)
                        .xsmall()
                        .text_color(color)
                        .into_any_element(),
                    format!("{verb}: {headline}"),
                    None,
                )
            }
            Row::RunNotice {
                key, text, failed, ..
            } => (
                key,
                Icon::new(if *failed {
                    IconName::CircleAlert
                } else {
                    IconName::Info
                })
                .xsmall()
                .text_color(if *failed { p.danger } else { p.text_faint })
                .into_any_element(),
                text.lines().next().unwrap_or_default().to_string(),
                text.contains('\n').then(|| text.clone()),
            ),
            Row::Skills { key, names } => (
                key,
                Icon::new(IconName::BookOpen)
                    .xsmall()
                    .text_color(p.text_faint)
                    .into_any_element(),
                format!("Skills: {}", names.join(", ")),
                None,
            ),
            _ => return div().into_any_element(),
        };
        let open = self.side.activity.open.contains(key);
        let toggle_key = key.to_string();
        let has_detail = detail.is_some();
        v_flex()
            .child(
                h_flex()
                    .id(SharedString::from(format!("step-{key}")))
                    .gap_2()
                    .px_1()
                    .py(px(3.))
                    .items_center()
                    .rounded(px(6.))
                    .text_xs()
                    .text_color(p.text_muted)
                    .when(has_detail, |d| {
                        d.cursor_pointer()
                            .hover(|s| s.bg(kit::hover_wash(&p)).text_color(p.text))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let open = &mut this.side.activity.open;
                                if !open.remove(&toggle_key) {
                                    open.insert(toggle_key.clone());
                                }
                                cx.notify();
                            }))
                    })
                    .child(
                        div()
                            .w(px(14.))
                            .flex_none()
                            .flex()
                            .justify_center()
                            .child(mark),
                    )
                    .child(div().flex_1().min_w_0().truncate().child(title)),
            )
            .when(open, |d| {
                d.when_some(detail, |d, detail| {
                    d.child(
                        div()
                            .ml(px(22.))
                            .mb_1()
                            .p_2()
                            .rounded(px(6.))
                            .bg(kit::hover_wash(&p))
                            .text_xs()
                            .text_color(p.text_muted)
                            .whitespace_normal()
                            .child(detail),
                    )
                })
            })
            .into_any_element()
    }
}
