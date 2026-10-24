//! Run inspector: pick a conversation and one of its runs, then see model
//! calls, tool executions, activity, token stats, the exact prompt, and the
//! thread's log lines. "Follow live" re-reads the snapshot on every update.
//!
//! The gateway keeps diagnostics in memory for a few recent runs per session,
//! so older runs show "no diagnostics".

use chrono::{DateTime, Utc};
use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::operator::inspector::{
    DiagnosticActivityKind, InspectorModelCallStatus, ToolExecutionStatus,
};
use ironclaw_gpui_client::operator::{
    InspectorPrompt, InspectorRun, InspectorSignal, InspectorStream,
};
use ironclaw_gpui_client::product_wire::{RebornLogEntry, RebornOperatorLogsQuery};
use ironclaw_gpui_client::wire::ThreadSummary;

use super::logs::log_row;
use super::{muted, section_title};
use crate::jobs;
use crate::pages::{PageEvent, empty_state, error_line};
use crate::ui::*;

const THREADS: u32 = 30;
const TIMELINE: u32 = 200;
const THREAD_LOGS: u32 = 50;
const PROMPT_MAX_HEIGHT: f32 = 420.;

struct RunRef {
    id: String,
    at: Option<DateTime<Utc>>,
}

/// One tool's detail, read on demand (`inspector_tool`).
enum ToolDetail {
    Loading,
    Loaded(Box<ironclaw_gpui_client::operator::inspector::ToolExecutionDiagnostic>),
    Missing,
    Failed(SharedString),
}

pub struct InspectorPanel {
    backend: Backend,
    threads: Vec<ThreadSummary>,
    thread: Option<String>,
    runs: Vec<RunRef>,
    run: Option<String>,
    /// `Some(None)`: the gateway kept no diagnostics for this run.
    snapshot: Option<Option<InspectorRun>>,
    prompt: Option<Result<Option<InspectorPrompt>, SharedString>>,
    tool: Option<(String, ToolDetail)>,
    logs: Option<Vec<RebornLogEntry>>,
    fetching: bool,
    dirty: bool,
    live: Option<(InspectorStream, Task<()>)>,
    live_note: Option<SharedString>,
    error: Option<SharedString>,
}

impl EventEmitter<PageEvent> for InspectorPanel {}

impl InspectorPanel {
    pub fn new(backend: Backend, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            threads: Vec::new(),
            thread: None,
            runs: Vec::new(),
            run: None,
            snapshot: None,
            prompt: None,
            tool: None,
            logs: None,
            fetching: false,
            dirty: false,
            live: None,
            live_note: None,
            error: None,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.list_threads(THREADS, None).await },
            |this, result, cx| {
                match result {
                    Ok(page) => this.threads = page.threads,
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
        if let Some(thread) = self.thread.clone() {
            self.load_runs(thread, cx);
        }
        self.fetch_snapshot(cx);
    }

    fn select_thread(&mut self, thread: String, cx: &mut Context<Self>) {
        self.stop_live();
        self.thread = Some(thread.clone());
        self.run = None;
        self.runs.clear();
        self.clear_run();
        self.logs = None;
        cx.notify();
        self.load_runs(thread, cx);
    }

    /// Runs come from the thread timeline; newest first.
    fn load_runs(&mut self, thread: String, cx: &mut Context<Self>) {
        let logs_thread = thread.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.timeline(&thread, TIMELINE).await },
            |this, result, cx| {
                match result {
                    Ok(page) => {
                        this.runs = runs_of(
                            page.messages
                                .iter()
                                .map(|m| (m.turn_run_id.clone(), m.created_at)),
                        );
                        if this.run.is_none()
                            && let Some(first) = this.runs.first().map(|r| r.id.clone())
                        {
                            this.select_run(first, cx);
                        }
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
        let query = RebornOperatorLogsQuery {
            limit: Some(THREAD_LOGS),
            thread_id: Some(logs_thread),
            ..Default::default()
        };
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.thread_logs(&query).await },
            |this, result, cx| {
                match result {
                    Ok(page) => this.logs = Some(page.entries),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn clear_run(&mut self) {
        self.snapshot = None;
        self.prompt = None;
        self.tool = None;
    }

    fn select_run(&mut self, run: String, cx: &mut Context<Self>) {
        self.stop_live();
        self.run = Some(run);
        self.clear_run();
        self.fetch_snapshot(cx);
    }

    /// One snapshot read at a time; updates during a read mark it dirty.
    fn fetch_snapshot(&mut self, cx: &mut Context<Self>) {
        let (Some(thread), Some(run)) = (self.thread.clone(), self.run.clone()) else {
            return;
        };
        if self.fetching {
            self.dirty = true;
            return;
        }
        self.fetching = true;
        let (want_thread, want_run) = (thread.clone(), run.clone());
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.inspector_snapshot(&thread, &run).await },
            move |this, result, cx| {
                this.fetching = false;
                // Drop a late answer for a run that is no longer selected.
                if this.thread.as_deref() == Some(want_thread.as_str())
                    && this.run.as_deref() == Some(want_run.as_str())
                {
                    match result {
                        Ok(snapshot) => {
                            this.snapshot = Some(snapshot);
                            this.error = None;
                        }
                        Err(e) => this.error = Some(e.to_string().into()),
                    }
                }
                if std::mem::take(&mut this.dirty) {
                    this.fetch_snapshot(cx);
                }
                cx.notify();
            },
        );
    }

    fn load_prompt(&mut self, cx: &mut Context<Self>) {
        let (Some(thread), Some(run)) = (self.thread.clone(), self.run.clone()) else {
            return;
        };
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.inspector_prompt(&thread, &run).await },
            |this, result, cx| {
                this.prompt = Some(result.map_err(|e| e.to_string().into()));
                cx.notify();
            },
        );
    }

    fn toggle_tool(&mut self, activity_id: String, cx: &mut Context<Self>) {
        if self.tool.as_ref().is_some_and(|(id, _)| *id == activity_id) {
            self.tool = None;
            cx.notify();
            return;
        }
        let (Some(thread), Some(run)) = (self.thread.clone(), self.run.clone()) else {
            return;
        };
        self.tool = Some((activity_id.clone(), ToolDetail::Loading));
        cx.notify();
        let id = activity_id.clone();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.inspector_tool(&thread, &run, &id).await },
            move |this, result, cx| {
                let detail = match result {
                    Ok(Some(tool)) => ToolDetail::Loaded(Box::new(tool)),
                    Ok(None) => ToolDetail::Missing,
                    Err(e) => ToolDetail::Failed(e.to_string().into()),
                };
                if let Some((open, slot)) = this.tool.as_mut()
                    && *open == activity_id
                {
                    *slot = detail;
                }
                cx.notify();
            },
        );
    }

    fn set_live(&mut self, on: bool, cx: &mut Context<Self>) {
        if !on {
            self.stop_live();
            cx.notify();
            return;
        }
        let (Some(thread), Some(run)) = (self.thread.clone(), self.run.clone()) else {
            return;
        };
        let (stream, mut rx) = self.backend.inspector_updates(&thread, &run);
        let pump = cx.spawn(async move |this, cx| {
            while let Some(first) = rx.recv().await {
                // A burst of updates becomes one snapshot read.
                let mut last = first;
                while let Ok(next) = rx.try_recv() {
                    last = next;
                }
                let alive = this.update(cx, |this, cx| {
                    match last {
                        InspectorSignal::Live => this.live_note = Some("Following live".into()),
                        InspectorSignal::Changed => this.fetch_snapshot(cx),
                        InspectorSignal::Closed(reason) => {
                            this.live = None;
                            this.live_note = (!reason.is_empty())
                                .then(|| format!("Live updates stopped: {reason}").into());
                        }
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        self.live_note = Some("Connecting".into());
        self.live = Some((stream, pump));
        cx.notify();
    }

    fn stop_live(&mut self) {
        // Dropping the stream frees the gateway's stream slot.
        self.live = None;
        self.live_note = None;
    }

    fn render_picker(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let threads = self.threads.iter().enumerate().map(|(ix, t)| {
            let id = t.thread_id.clone();
            let selected = self.thread.as_deref() == Some(id.as_str());
            h_flex()
                .id(("inspect-thread", ix))
                .role(Role::Button)
                .aria_label(t.display_title().to_string())
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .text_sm()
                .cursor_pointer()
                .when(selected, |d| d.bg(theme.accent).font_medium())
                .when(!selected, |d| {
                    crate::design::kit::smooth_bg(d, format!("inspect-thread-{ix}"), cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| this.select_thread(id.clone(), cx)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(t.display_title().to_string()),
                )
                .when_some(t.updated_at, |d, at| {
                    d.child(muted(jobs::ago(at), cx).flex_none())
                })
        });
        let runs = self.runs.iter().enumerate().map(|(ix, r)| {
            let id = r.id.clone();
            let selected = self.run.as_deref() == Some(id.as_str());
            let label = match r.at {
                Some(at) => jobs::ago(at),
                None => short_id(&r.id),
            };
            Button::new(("inspect-run", ix))
                .label(label)
                .xsmall()
                .when(selected, |b| b.primary())
                .when(!selected, |b| b.outline())
                .on_click(cx.listener(move |this, _, _, cx| this.select_run(id.clone(), cx)))
        });
        v_flex()
            .gap_2()
            .child(
                v_flex()
                    .id("inspect-threads")
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    .children(threads),
            )
            .when(self.thread.is_some(), |d| {
                d.child(
                    h_flex()
                        .gap_1()
                        .flex_wrap()
                        .items_center()
                        .child(muted("Runs:", cx))
                        .when(self.runs.is_empty(), |d| d.child(muted("none yet", cx)))
                        .children(runs),
                )
            })
    }

    fn render_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let thread = self.thread.clone();
        h_flex()
            .gap_3()
            .items_center()
            .child(
                Switch::new("inspector-live")
                    .checked(self.live.is_some())
                    .label("Follow live")
                    .accessibility_label("Follow live")
                    .on_click(
                        cx.listener(|this, checked: &bool, _, cx| this.set_live(*checked, cx)),
                    ),
            )
            .when_some(self.live_note.clone(), |d, n| d.child(muted(n, cx)))
            .child(div().flex_1())
            .when_some(thread, |d, thread| {
                d.child(
                    Button::new("open-inspected-thread")
                        .icon(IconName::ExternalLink)
                        .label("Open conversation")
                        .ghost()
                        .small()
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(PageEvent::OpenThread(thread.clone()))
                        })),
                )
            })
    }

    fn render_run(&self, run: &InspectorRun, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let s = &run.stats;
        let stats = format!(
            "{} model calls  ·  {} in / {} out tokens ({} cached)  ·  {} ms model time  ·  {} tools ({} ok, {} failed)",
            s.total_model_calls,
            s.input_tokens.known_total,
            s.output_tokens.known_total,
            s.cache_read_input_tokens.known_total,
            s.total_latency_ms.known_total,
            s.total_tool_calls,
            s.successful_tool_calls,
            s.failed_tool_calls,
        );
        let calls = run.model_calls.iter().map(|m| {
            let (tag, label) = match m.status {
                InspectorModelCallStatus::Succeeded => (Tag::success(), "ok"),
                InspectorModelCallStatus::Failed => (Tag::danger(), "failed"),
                InspectorModelCallStatus::Started => (Tag::secondary(), "running"),
            };
            let model = match &m.effective_model {
                Some(effective) => {
                    format!("{} → {}", m.requested_model.content(), effective.content())
                }
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
            h_flex()
                .gap_2()
                .py_1()
                .items_center()
                .border_t_1()
                .border_color(theme.border)
                .child(muted(format!("#{}", m.iteration), cx).flex_none())
                .child(div().flex_1().min_w_0().text_sm().truncate().child(model))
                .child(muted(usage, cx).flex_none())
                .when_some(m.duration_ms, |d, ms| {
                    d.child(muted(format!("{ms} ms"), cx).flex_none())
                })
                .child(tag.small().child(label))
        });
        let tools = run.tool_executions.iter().enumerate().map(|(ix, t)| {
            let id = t.activity_id.to_string();
            let open = self
                .tool
                .as_ref()
                .filter(|(open, _)| *open == id)
                .map(|(_, d)| d);
            let tag = match t.status {
                ToolExecutionStatus::Succeeded => Tag::success().small().child("ok"),
                ToolExecutionStatus::Failed => Tag::danger().small().child("failed"),
                ToolExecutionStatus::Started => Tag::secondary().small().child("running"),
            };
            v_flex()
                .border_t_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .id(("inspect-tool", ix))
                        .role(Role::Button)
                        .aria_label(t.capability_name.content().to_string())
                        .gap_2()
                        .py_1()
                        .items_center()
                        .cursor_pointer()
                        .on_click(
                            cx.listener(move |this, _, _, cx| this.toggle_tool(id.clone(), cx)),
                        )
                        .child(
                            Icon::new(if open.is_some() {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .xsmall(),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .truncate()
                                .child(t.capability_name.content().to_string()),
                        )
                        .when_some(t.duration_ms, |d, ms| {
                            d.child(muted(format!("{ms} ms"), cx))
                        })
                        .when_some(t.output_bytes, |d, b| d.child(muted(jobs::size(b), cx)))
                        .child(tag),
                )
                .when_some(open, |d, detail| d.child(tool_detail(detail, cx)))
        });
        let activity = run.activity.iter().map(|a| {
            let e = &a.event;
            h_flex()
                .gap_2()
                .text_xs()
                .child(
                    div().flex_none().text_color(theme.muted_foreground).child(
                        e.occurred_at
                            .with_timezone(&chrono::Local)
                            .format("%H:%M:%S")
                            .to_string(),
                    ),
                )
                .child(
                    div()
                        .flex_none()
                        .font_medium()
                        .child(activity_label(e.kind)),
                )
                .when_some(e.summary.as_ref(), |d, s| {
                    d.child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.muted_foreground)
                            .child(s.content().to_string()),
                    )
                })
        });
        v_flex()
            .child(card(cx).child(muted(stats, cx)))
            .child(section_title("Model calls"))
            .when(run.model_calls.is_empty(), |d| d.child(muted("None.", cx)))
            .child(v_flex().children(calls))
            .child(section_title("Tools"))
            .when(run.tool_executions.is_empty(), |d| {
                d.child(muted("None.", cx))
            })
            .child(v_flex().children(tools))
            .child(section_title("Activity"))
            .child(v_flex().gap_0p5().children(activity))
    }

    fn render_prompt(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let load = Button::new("load-prompt")
            .label(if self.prompt.is_some() {
                "Reload prompt"
            } else {
                "Show prompt"
            })
            .outline()
            .small()
            .on_click(cx.listener(|this, _, _, cx| this.load_prompt(cx)));
        let body: Option<AnyElement> = match &self.prompt {
            None => None,
            Some(Err(e)) => Some(error_line(e, cx).into_any_element()),
            Some(Ok(None)) => {
                Some(muted("No prompt was captured for this run.", cx).into_any_element())
            }
            Some(Ok(Some(p))) => {
                let facts = format!(
                    "{} components{}  ·  {} messages  ·  {} tools offered  ·  ~{} tokens{}{}",
                    p.components.len(),
                    if p.components_truncated { " (cut)" } else { "" },
                    p.message_count,
                    p.capability_count,
                    p.total_estimated_tokens
                        .map_or("?".into(), |n| n.to_string()),
                    p.context_limit
                        .map(|l| format!(" of {l}"))
                        .unwrap_or_default(),
                    p.effective_model
                        .as_ref()
                        .map(|m| format!("  ·  {}", m.content()))
                        .unwrap_or_default(),
                );
                let skills = p
                    .active_skills
                    .iter()
                    .map(|s| s.content().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                let text = p.reconstructed_prompt.content().to_string();
                Some(
                    v_flex()
                        .gap_2()
                        .child(muted(facts, cx))
                        .when(!skills.is_empty(), |d| {
                            d.child(muted(format!("Skills: {skills}"), cx))
                        })
                        .when(p.reconstructed_prompt.truncated(), |d| {
                            d.child(muted(
                                format!(
                                    "Showing the first {} of {} bytes.",
                                    text.len(),
                                    p.reconstructed_prompt.original_bytes()
                                ),
                                cx,
                            ))
                        })
                        .child(
                            div()
                                .id("inspector-prompt")
                                .max_h(px(PROMPT_MAX_HEIGHT))
                                .overflow_y_scroll()
                                .p_2()
                                .rounded(theme.radius)
                                .bg(theme.muted)
                                .text_xs()
                                .font_family(theme.mono_font_family.clone())
                                .whitespace_normal()
                                .child(text),
                        )
                        .into_any_element(),
                )
            }
        };
        v_flex()
            .gap_2()
            .child(h_flex().child(load))
            .children(body)
            .into_any_element()
    }
}

fn tool_detail(detail: &ToolDetail, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let block = |label: &'static str, text: String| {
        v_flex().gap_0p5().child(muted(label, cx)).child(
            div()
                .p_2()
                .rounded(theme.radius)
                .bg(theme.muted)
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .whitespace_normal()
                .child(text),
        )
    };
    match detail {
        ToolDetail::Loading => Spinner::new().into_any_element(),
        ToolDetail::Missing => {
            muted("The gateway no longer has this tool's details.", cx).into_any_element()
        }
        ToolDetail::Failed(e) => error_line(e, cx).into_any_element(),
        ToolDetail::Loaded(t) => v_flex()
            .gap_2()
            .pb_2()
            .pl_5()
            .when_some(t.arguments.as_ref(), |d, a| {
                d.child(block("Arguments", a.content().to_string()))
            })
            .when_some(t.result.as_ref(), |d, r| {
                let label = if r.truncated() {
                    "Result (cut)"
                } else {
                    "Result"
                };
                d.child(block(label, r.content().to_string()))
            })
            .when_some(t.failure_summary.as_ref(), |d, f| {
                d.child(block("Failure", f.content().to_string()))
            })
            .into_any_element(),
    }
}

/// Distinct run ids in timeline order, newest first, each with its first time.
fn runs_of(messages: impl Iterator<Item = (Option<String>, Option<DateTime<Utc>>)>) -> Vec<RunRef> {
    let mut runs: Vec<RunRef> = Vec::new();
    for (id, at) in messages {
        let Some(id) = id else { continue };
        match runs.iter_mut().find(|r| r.id == id) {
            Some(run) => run.at = run.at.or(at),
            None => runs.push(RunRef { id, at }),
        }
    }
    runs.reverse();
    runs
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
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

impl Render for InspectorPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let run_view: Option<AnyElement> = match &self.snapshot {
            _ if self.run.is_none() => None,
            None => Some(Spinner::new().into_any_element()),
            Some(None) => Some(
                empty_state(
                    "No diagnostics for this run. The gateway keeps them in memory for a few recent runs only.",
                    cx,
                )
                .into_any_element(),
            ),
            Some(Some(run)) => Some(self.render_run(run, cx).into_any_element()),
        };
        let logs: Vec<AnyElement> = self
            .logs
            .iter()
            .flatten()
            .map(|e| log_row(e, cx).into_any_element())
            .collect();
        v_flex()
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .child(section_title("Conversation"))
            .when(self.threads.is_empty(), |d| {
                d.child(muted("No conversations yet.", cx))
            })
            .child(self.render_picker(cx))
            .when(self.run.is_some(), |d| {
                d.child(div().pt_4().child(self.render_toolbar(cx)))
                    .children(run_view)
                    .child(section_title("Prompt"))
                    .child(self.render_prompt(cx))
            })
            .when(self.thread.is_some(), |d| {
                d.child(section_title("Thread logs"))
                    .when(self.logs.as_ref().is_some_and(Vec::is_empty), |d| {
                        d.child(muted("No log lines carry this conversation's id.", cx))
                    })
                    .children(logs)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::runs_of;

    #[test]
    fn runs_are_distinct_and_newest_first() {
        let t = |s: &str| s.parse().ok();
        let runs = runs_of(
            [
                (Some("a".to_string()), t("2026-10-23T10:00:00Z")),
                (None, None),
                (Some("a".to_string()), t("2026-10-23T10:00:05Z")),
                (Some("b".to_string()), t("2026-10-23T11:00:00Z")),
            ]
            .into_iter(),
        );
        let ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["b", "a"]);
        assert_eq!(runs[1].at, t("2026-10-23T10:00:00Z"));
    }
}
