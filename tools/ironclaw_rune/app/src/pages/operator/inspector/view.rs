//! Inspector rendering.

use crate::pages::common::controls::{
    Look, Size, button_sized, muted, outlined_tag, rule, spinner,
};
use crate::pages::common::ts::{MONO_XS, sm, sm_medium, xs, xs_medium};
use crate::pages::common::{Tone, card, empty_state, error_line};
use rune::{Cx, El, ScrollSpec, col, el, row, text};
use rune_kit::{Kit, Switch};
use zlo_client::operator::InspectorRun;
use zlo_client::operator::inspector::{
    DiagnosticActivityKind, InspectorModelCallStatus, ToolExecutionStatus,
};

use super::super::logs::log_row;
use super::super::section_title;
use super::{InspectorPanel, ToolDetail, short_id};
use crate::design::kit::{self, Turn};
use crate::design::motion;
use crate::design::{CONTROL_RADIUS, Palette};
use crate::icons;
use crate::jobs;
use crate::state::State;

const PROMPT_MAX_HEIGHT: f32 = 420.;

/// The key of a thread's row in the picker.
fn thread_key(id: &str) -> String {
    format!("inspect-thread-{id}")
}

/// The selection pill behind the thread list: the selected row's own wash
/// and corners, gliding (`motion::PILL`) to the row the person picks.
/// Rows are one line each with no gap, so the selected row sits `ix` row
/// heights down; the height is the rows' as laid out last frame. Before the
/// first layout there is no pill and the selected row paints its own fill.
fn thread_pill(s: &State, cx: &Cx<State>, panel: &InspectorPanel) -> Option<El> {
    let ix = panel
        .threads
        .iter()
        .position(|t| panel.thread.as_deref() == Some(t.thread_id.as_str()))?;
    let first = panel.threads.first()?;
    let h = cx.window_layout(&thread_key(&first.thread_id))?[3];
    (h > 0.).then(|| {
        el().key("inspect-thread-pill")
            .pin(Some(0.), Some(ix as f32 * h), Some(0.), None)
            .h(h)
            .radius(CONTROL_RADIUS)
            .fill(s.palette.accent_wash)
            .a11y_hidden()
            .motion(motion::PILL)
            .transition(rune::Transition::opacity())
    })
}

fn render_picker(s: &State, cx: &Cx<State>, k: &Kit, panel: &InspectorPanel) -> El {
    let p = &s.palette;
    let pill = thread_pill(s, cx, panel);
    let has_pill = pill.is_some();
    let threads = panel.threads.iter().map(|t| {
        let id = t.thread_id.clone();
        let selected = panel.thread.as_deref() == Some(id.as_str());
        let title = t.display_title().to_string();
        let mut r = row()
            .key(thread_key(&id))
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
                    .font(if selected { sm_medium() } else { sm() })
                    .color(p.text)
                    .truncate()
                    .grow(1.)
                    .min_w(0.),
            );
        r = if !selected {
            r.hover(p.hover).hover_motion(motion::HOVER, motion::HOVER)
        } else if has_pill {
            r
        } else {
            r.fill(p.accent_wash)
        };
        if let Some(at) = t.updated_at {
            r = r.child(text(jobs::ago(at)).font(xs()).color(p.text_muted));
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
            .key("inspect-threads")
            .children(pill)
            .children(threads),
    );
    if panel.thread.is_some() {
        let runs = panel.runs.iter().map(|r| {
            let id = r.id.clone();
            let selected = panel.run.as_deref() == Some(id.as_str());
            let label = match r.at {
                Some(at) => jobs::ago(at),
                None => short_id(&r.id),
            };
            button_sized(
                label,
                crate::pages::common::choice_look(selected),
                Size::XSmall,
            )
            .on_press(move |s: &mut State| super::select_run(s, id.clone()))
            .build(k)
            .key(format!("inspect-run-{}", r.id))
            .press_scale(motion::PRESS)
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
        // While the stream follows the run, a green dot pulses its halo
        // beside the note (15 fps, still under Reduce Motion); it goes when
        // the stream stops.
        let mut note = row().key("live-note").gap(6.).center_y();
        if on && n == super::FOLLOWING {
            note = note.child(kit::status_dot("live-dot", p.success, true));
        }
        r = r.child(note.child(muted(p, n.clone())));
    }
    r = r.child(el().grow(1.));
    if let Some(thread) = panel.thread.clone() {
        r = r.child(
            button_sized("Open conversation", Look::Ghost, Size::Small)
                .icon(&icons::EXTERNAL_LINK)
                .on_press(move |s: &mut State| crate::workspace::open_thread(s, thread.clone()))
                .build(k)
                .key("open-inspected-thread")
                .press_scale(motion::PRESS),
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
                    .font(sm())
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
            .child(r.child(outlined_tag(p, tone, label)))
    });
    let tools = run.tool_executions.iter().enumerate().map(|(ix, t)| {
        let id = t.activity_id.to_string();
        let open = panel
            .tool
            .as_ref()
            .filter(|(o, _)| *o == id)
            .map(|(_, d)| d);
        let status = match t.status {
            ToolExecutionStatus::Succeeded => outlined_tag(p, Tone::Success, "ok"),
            ToolExecutionStatus::Failed => outlined_tag(p, Tone::Danger, "failed"),
            ToolExecutionStatus::Started => outlined_tag(p, Tone::Secondary, "running"),
        };
        let name = t.capability_name.content().to_string();
        let id_key = id.clone();
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
            .child(kit::chevron(
                &format!("inspect-tool-{id_key}-chev"),
                open.is_some(),
                Turn::Disclose,
                12.,
                p.text,
            ))
            .child(
                text(name)
                    .font(sm())
                    .color(p.text)
                    .truncate()
                    .grow(1.)
                    .min_w(0.),
            );
        if let Some(ms) = t.duration_ms {
            head = head.child(muted(p, format!("{ms} ms")));
        }
        if let Some(b) = t.output_bytes {
            head = head.child(muted(p, zlo_client::attachments::format_bytes(b)));
        }
        let mut c = col().w_full().child(rule(p)).child(head.child(status));
        if let Some(detail) = open {
            // The detail wipes open under its row; inside, the spinner
            // gives way to the arguments and result in place.
            c = c.child(
                col()
                    .key("tool-detail")
                    .w_full()
                    .transition(motion::reveal())
                    .child(tool_detail(p, k, detail)),
            );
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
                .font(xs())
                .color(p.text_muted),
            )
            .child(text(activity_label(e.kind)).font(xs_medium()).color(p.text));
        if let Some(summary) = e.summary.as_ref() {
            r = r.child(
                text(summary.content().to_string())
                    .font(xs())
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
    let load = button_sized(
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
                        .key("inspector-prompt")
                        .child(code_block(p, body)),
                ),
            )
        }
    };
    col()
        .w_full()
        .gap(8.)
        .child(row().child(load.press_scale(motion::PRESS)))
        .children(body.map(|b| b.key("prompt-body").transition(motion::reveal())))
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
    cx: &mut Cx<State>,
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
    c = c.child(render_picker(s, cx, k, panel));
    if panel.run.is_some() {
        // The spinner, the empty note and the run crossfade in place.
        let run_view = match &panel.snapshot {
            None => spinner(k).key("run-loading"),
            Some(None) => empty_state(
                p,
                "No diagnostics for this run. The gateway keeps them in memory for a few recent runs only.",
            )
            .key("run-empty"),
            Some(Some(run)) => render_run(s, k, panel, run).key("run-view"),
        }
        .transition(motion::fade());
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
