//! Inspector rendering (GPUI `InspectorPanel`'s `render_*` methods).

use ironclaw_gpui_client::operator::InspectorRun;
use ironclaw_gpui_client::operator::inspector::{
    DiagnosticActivityKind, InspectorModelCallStatus, ToolExecutionStatus,
};
use rune::{Cx, El, ScrollSpec, col, el, row, text};
use rune_kit::{Kit, Switch};

use super::super::logs::log_row;
use super::super::section_title;
use super::{InspectorPanel, ToolDetail, short_id};
use crate::design::{CONTROL_RADIUS, Palette};
use crate::icons;
use crate::jobs;
use crate::pages::c_widgets::{
    Look, MONO_XS, SM, SM_MEDIUM, Size, Tone, XS, XS_MEDIUM, button, card, empty_state, error_line,
    glyph, muted, rule, spinner, tag,
};
use crate::state::State;

const PROMPT_MAX_HEIGHT: f32 = 420.;

fn render_picker(s: &State, k: &Kit, panel: &InspectorPanel) -> El {
    let p = &s.palette;
    let threads = panel.threads.iter().enumerate().map(|(ix, t)| {
        let id = t.thread_id.clone();
        let selected = panel.thread.as_deref() == Some(id.as_str());
        let title = t.display_title().to_string();
        let mut r = row()
            .key(format!("inspect-thread-{ix}"))
            .role("button")
            .label(title.clone())
            .focusable(true)
            .w_full()
            .gap(8.)
            .center_y()
            .px(8.)
            .py(4.)
            .radius(CONTROL_RADIUS)
            .cursor(rune::Cursor::Pointer)
            .on_press(move |s: &mut State| super::select_thread(s, id.clone()))
            .child(
                text(title)
                    .font(if selected { SM_MEDIUM } else { SM })
                    .color(p.text)
                    .truncate()
                    .grow(1.)
                    .min_w(0.),
            );
        r = if selected {
            r.fill(p.accent_wash)
        } else {
            r.hover(p.hover)
        };
        if let Some(at) = t.updated_at {
            r = r.child(text(jobs::ago(at)).font(XS).color(p.text_muted));
        }
        r
    });
    let mut c = col().w_full().gap(8.).child(
        col()
            .key("inspect-threads")
            .w_full()
            .max_h(220.)
            .scroll(ScrollSpec {
                stick_bottom: false,
            })
            .children(threads),
    );
    if panel.thread.is_some() {
        let runs = panel.runs.iter().enumerate().map(|(ix, r)| {
            let id = r.id.clone();
            let selected = panel.run.as_deref() == Some(id.as_str());
            let label = match r.at {
                Some(at) => jobs::ago(at),
                None => short_id(&r.id),
            };
            button(
                label,
                if selected {
                    Look::Primary
                } else {
                    Look::Outline
                },
                Size::XSmall,
            )
            .on_press(move |s: &mut State| super::select_run(s, id.clone()))
            .build(k)
            .key(format!("inspect-run-{ix}"))
        });
        let mut line = row()
            .w_full()
            .gap(4.)
            .flex_wrap()
            .center_y()
            .child(muted(p, "Runs:"));
        if panel.runs.is_empty() {
            line = line.child(muted(p, "none yet"));
        }
        c = c.child(line.children(runs));
    }
    c
}

fn render_toolbar(s: &State, k: &Kit, panel: &InspectorPanel) -> El {
    let p = &s.palette;
    let on = panel.live.is_some();
    let mut r = row().w_full().gap(12.).center_y().child(
        Switch::new(on)
            .label("Follow live")
            .on_toggle(move |s: &mut State| super::set_live(s, !on))
            .build(k)
            .key("inspector-live"),
    );
    if let Some(n) = &panel.live_note {
        r = r.child(muted(p, n.clone()));
    }
    r = r.child(el().grow(1.));
    if let Some(thread) = panel.thread.clone() {
        r = r.child(
            button("Open conversation", Look::Ghost, Size::Small)
                .icon(&icons::EXTERNAL_LINK)
                .on_press(move |s: &mut State| crate::workspace::open_thread(s, thread.clone()))
                .build(k)
                .key("open-inspected-thread"),
        );
    }
    r
}

fn render_run(s: &State, k: &Kit, panel: &InspectorPanel, run: &InspectorRun) -> El {
    let p = &s.palette;
    let st = &run.stats;
    let stats = format!(
        "{} model calls  ·  {} in / {} out tokens ({} cached)  ·  {} ms model time  ·  {} tools ({} ok, {} failed)",
        st.total_model_calls,
        st.input_tokens.known_total,
        st.output_tokens.known_total,
        st.cache_read_input_tokens.known_total,
        st.total_latency_ms.known_total,
        st.total_tool_calls,
        st.successful_tool_calls,
        st.failed_tool_calls,
    );
    let calls = run.model_calls.iter().enumerate().map(|(ix, m)| {
        let (tone, label) = match m.status {
            InspectorModelCallStatus::Succeeded => (Tone::Success, "ok"),
            InspectorModelCallStatus::Failed => (Tone::Danger, "failed"),
            InspectorModelCallStatus::Started => (Tone::Secondary, "running"),
        };
        let model = match &m.effective_model {
            Some(effective) => format!("{} → {}", m.requested_model.content(), effective.content()),
            None => m.requested_model.content().to_string(),
        };
        let usage = m
            .usage
            .as_ref()
            .map(|u| {
                format!(
                    "{} in / {} out",
                    u.input_tokens.map_or("?".into(), |n| n.to_string()),
                    u.output_tokens.map_or("?".into(), |n| n.to_string())
                )
            })
            .unwrap_or_default();
        let mut r = row()
            .w_full()
            .gap(8.)
            .py(4.)
            .center_y()
            .child(muted(p, format!("#{}", m.iteration)))
            .child(
                text(model)
                    .font(SM)
                    .color(p.text)
                    .truncate()
                    .grow(1.)
                    .min_w(0.),
            )
            .child(muted(p, usage));
        if let Some(ms) = m.duration_ms {
            r = r.child(muted(p, format!("{ms} ms")));
        }
        col()
            .key(format!("call-{ix}"))
            .w_full()
            .child(rule(p))
            .child(r.child(tag(p, tone, label)))
    });
    let tools = run.tool_executions.iter().enumerate().map(|(ix, t)| {
        let id = t.activity_id.to_string();
        let open = panel
            .tool
            .as_ref()
            .filter(|(o, _)| *o == id)
            .map(|(_, d)| d);
        let status = match t.status {
            ToolExecutionStatus::Succeeded => tag(p, Tone::Success, "ok"),
            ToolExecutionStatus::Failed => tag(p, Tone::Danger, "failed"),
            ToolExecutionStatus::Started => tag(p, Tone::Secondary, "running"),
        };
        let name = t.capability_name.content().to_string();
        let mut head = row()
            .key(format!("inspect-tool-{ix}"))
            .role("button")
            .label(name.clone())
            .focusable(true)
            .w_full()
            .gap(8.)
            .py(4.)
            .center_y()
            .cursor(rune::Cursor::Pointer)
            .on_press(move |s: &mut State| super::toggle_tool(s, id.clone()))
            .child(glyph(
                if open.is_some() {
                    &icons::CHEVRON_DOWN
                } else {
                    &icons::CHEVRON_RIGHT
                },
                12.,
                p.text,
            ))
            .child(
                text(name)
                    .font(SM)
                    .color(p.text)
                    .truncate()
                    .grow(1.)
                    .min_w(0.),
            );
        if let Some(ms) = t.duration_ms {
            head = head.child(muted(p, format!("{ms} ms")));
        }
        if let Some(b) = t.output_bytes {
            head = head.child(muted(p, jobs::size(b)));
        }
        let mut c = col().w_full().child(rule(p)).child(head.child(status));
        if let Some(detail) = open {
            c = c.child(tool_detail(p, k, detail));
        }
        c
    });
    let activity = run.activity.iter().enumerate().map(|(ix, a)| {
        let e = &a.event;
        let mut r = row()
            .key(format!("activity-{ix}"))
            .w_full()
            .gap(8.)
            .child(
                text(
                    e.occurred_at
                        .with_timezone(&chrono::Local)
                        .format("%H:%M:%S")
                        .to_string(),
                )
                .font(XS)
                .color(p.text_muted),
            )
            .child(text(activity_label(e.kind)).font(XS_MEDIUM).color(p.text));
        if let Some(summary) = e.summary.as_ref() {
            r = r.child(
                text(summary.content().to_string())
                    .font(XS)
                    .color(p.text_muted)
                    .truncate()
                    .grow(1.)
                    .min_w(0.),
            );
        }
        r
    });
    let mut c = col()
        .w_full()
        .child(card(p).child(muted(p, stats)))
        .child(section_title(s, "Model calls"));
    if run.model_calls.is_empty() {
        c = c.child(muted(p, "None."));
    }
    c = c
        .child(col().w_full().children(calls))
        .child(section_title(s, "Tools"));
    if run.tool_executions.is_empty() {
        c = c.child(muted(p, "None."));
    }
    c.child(col().w_full().children(tools))
        .child(section_title(s, "Activity"))
        .child(col().w_full().gap(2.).children(activity))
}

/// A mono text block on the muted wash (arguments, results, the prompt).
fn code_block(p: &Palette, body: String) -> El {
    col()
        .w_full()
        .padding(8.)
        .radius(CONTROL_RADIUS)
        .fill(p.hover)
        .child(
            text(body)
                .font(MONO_XS)
                .color(p.text)
                .wrap_anywhere()
                .selectable(true),
        )
}

fn tool_detail(p: &Palette, k: &Kit, detail: &ToolDetail) -> El {
    let block = |label: &str, body: String| {
        col()
            .w_full()
            .gap(2.)
            .child(muted(p, label.to_string()))
            .child(code_block(p, body))
    };
    match detail {
        ToolDetail::Loading => spinner(k),
        ToolDetail::Missing => muted(p, "The gateway no longer has this tool's details."),
        ToolDetail::Failed(e) => error_line(p, e),
        ToolDetail::Loaded(t) => {
            let mut c = col().w_full().gap(8.).pb(8.).pl(20.);
            if let Some(a) = t.arguments.as_ref() {
                c = c.child(block("Arguments", a.content().to_string()));
            }
            if let Some(r) = t.result.as_ref() {
                let label = if r.truncated() {
                    "Result (cut)"
                } else {
                    "Result"
                };
                c = c.child(block(label, r.content().to_string()));
            }
            if let Some(f) = t.failure_summary.as_ref() {
                c = c.child(block("Failure", f.content().to_string()));
            }
            c
        }
    }
}

fn render_prompt(s: &State, k: &Kit, panel: &InspectorPanel) -> El {
    let p = &s.palette;
    let load = button(
        if panel.prompt.is_some() {
            "Reload prompt"
        } else {
            "Show prompt"
        },
        Look::Outline,
        Size::Small,
    )
    .on_press(|s: &mut State| super::load_prompt(s))
    .build(k)
    .key("load-prompt");
    let body: Option<El> = match &panel.prompt {
        None => None,
        Some(Err(e)) => Some(error_line(p, e)),
        Some(Ok(None)) => Some(muted(p, "No prompt was captured for this run.")),
        Some(Ok(Some(pr))) => {
            let facts = format!(
                "{} components{}  ·  {} messages  ·  {} tools offered  ·  ~{} tokens{}{}",
                pr.components.len(),
                if pr.components_truncated {
                    " (cut)"
                } else {
                    ""
                },
                pr.message_count,
                pr.capability_count,
                pr.total_estimated_tokens
                    .map_or("?".into(), |n| n.to_string()),
                pr.context_limit
                    .map(|l| format!(" of {l}"))
                    .unwrap_or_default(),
                pr.effective_model
                    .as_ref()
                    .map(|m| format!("  ·  {}", m.content()))
                    .unwrap_or_default(),
            );
            let skills = pr
                .active_skills
                .iter()
                .map(|x| x.content().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let body = pr.reconstructed_prompt.content().to_string();
            let mut c = col().w_full().gap(8.).child(muted(p, facts));
            if !skills.is_empty() {
                c = c.child(muted(p, format!("Skills: {skills}")));
            }
            if pr.reconstructed_prompt.truncated() {
                c = c.child(muted(
                    p,
                    format!(
                        "Showing the first {} of {} bytes.",
                        body.len(),
                        pr.reconstructed_prompt.original_bytes()
                    ),
                ));
            }
            Some(
                c.child(
                    col()
                        .key("inspector-prompt")
                        .w_full()
                        .max_h(PROMPT_MAX_HEIGHT)
                        .scroll(ScrollSpec {
                            stick_bottom: false,
                        })
                        .child(code_block(p, body)),
                ),
            )
        }
    };
    col()
        .w_full()
        .gap(8.)
        .child(row().child(load))
        .children(body)
}

fn activity_label(kind: DiagnosticActivityKind) -> &'static str {
    match kind {
        DiagnosticActivityKind::TurnStarted => "Turn started",
        DiagnosticActivityKind::PromptPrepared => "Prompt prepared",
        DiagnosticActivityKind::ModelCallStarted => "Model call",
        DiagnosticActivityKind::ModelCallCompleted => "Model done",
        DiagnosticActivityKind::ModelCallFailed => "Model failed",
        DiagnosticActivityKind::Progress => "Progress",
        DiagnosticActivityKind::ToolStarted => "Tool started",
        DiagnosticActivityKind::ToolCompleted => "Tool done",
        DiagnosticActivityKind::ToolFailed => "Tool failed",
        DiagnosticActivityKind::GateBlocked => "Waiting on a gate",
        DiagnosticActivityKind::FinalResponseCompleted => "Answer done",
        DiagnosticActivityKind::StreamDisconnected => "Stream lost",
        DiagnosticActivityKind::StreamResumed => "Stream resumed",
    }
}

pub fn view(
    s: &State,
    _cx: &mut Cx<State>,
    k: &Kit,
    _layer: &mut Vec<El>,
    panel: &InspectorPanel,
) -> El {
    let p = &s.palette;
    let mut c = col().w_full();
    if let Some(e) = &panel.error {
        c = c.child(error_line(p, e));
    }
    c = c.child(section_title(s, "Conversation"));
    if panel.threads.is_empty() {
        c = c.child(muted(p, "No conversations yet."));
    }
    c = c.child(render_picker(s, k, panel));
    if panel.run.is_some() {
        let run_view = match &panel.snapshot {
            None => spinner(k),
            Some(None) => empty_state(
                p,
                "No diagnostics for this run. The gateway keeps them in memory for a few recent runs only.",
            ),
            Some(Some(run)) => render_run(s, k, panel, run),
        };
        c = c
            .child(col().w_full().pt(16.).child(render_toolbar(s, k, panel)))
            .child(run_view)
            .child(section_title(s, "Prompt"))
            .child(render_prompt(s, k, panel));
    }
    if panel.thread.is_some() {
        c = c.child(section_title(s, "Thread logs"));
        if panel.logs.as_ref().is_some_and(Vec::is_empty) {
            c = c.child(muted(p, "No log lines carry this conversation's id."));
        }
        c = c.children(panel.logs.iter().flatten().map(|e| log_row(p, e)));
    }
    c
}
