//! Model-facing tools: `t3_capabilities`, `t3_delegate`, `t3_task`,
//! `t3_thread`, `t3_queue` — the T3 orchestrator surface, same semantics as
//! the t3code plugin. Hosts register each [`ToolSpec`] with their tool
//! system and route calls to [`Engine::call_tool`]. Every call returns a
//! JSON document; correctable failures come back as `{ok: false, error,
//! message}` so the model can adapt instead of the run ending.

use ironclaw_host_api::ids::ThreadId;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::delegation::{DelegateRequest, Target};
use crate::engine::{CallContext, Engine, Result};
use crate::error::OrchestratorError;
use crate::model::*;
use crate::runtime::SteerHow;

pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
}

fn target_schema() -> Value {
    json!({
        "type": "object",
        "description": "Provider/model for the child. Omit to inherit this thread's. Pick from t3_capabilities.",
        "properties": {
            "provider": {"type": "string", "description": "Provider slug from t3_capabilities."},
            "model": {"type": "string", "description": "A model advertised by that provider. Provider alone picks its first model."},
            "reasoning_effort": {"type": "string", "description": "Optional effort, e.g. low, medium, high."}
        }
    })
}

pub const TOOLS: [ToolSpec; 5] = [
    ToolSpec {
        name: "t3_capabilities",
        description: "T3 orchestrator: list the providers and models this thread can delegate to, the inherited provider/model, and orchestration features and limits. Call before choosing a target for t3_delegate.",
        schema: || json!({"type": "object", "properties": {}}),
    },
    ToolSpec {
        name: "t3_delegate",
        description: "T3 orchestrator: delegate one task to a child agent in a new, visible thread. The child gets ONLY the task text, never this thread's history, so include every fact, path and constraint it needs. mode='async' (default) returns a taskId immediately; the result is delivered back to this thread as a new turn when it finishes, so end your turn instead of polling. mode='wait' blocks until done or timeout_s (the child keeps running on timeout). For each review round, call t3_delegate again with the full brief, prior findings and objections, and a new client_request_id.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "Complete, self-contained brief for the child."},
                    "title": {"type": "string", "description": "Short label shown in the UI."},
                    "role": {"type": "string", "enum": ["implementation", "research", "review", "design", "test", "general"]},
                    "target": target_schema(),
                    "mode": {"type": "string", "enum": ["async", "wait"]},
                    "timeout_s": {"type": "number", "description": "Wait budget for mode=wait (default 600)."},
                    "client_request_id": {"type": "string", "description": "Stable per logical request; retries return the same task."}
                },
                "required": ["task"]
            })
        },
    },
    ToolSpec {
        name: "t3_task",
        description: "T3 orchestrator: inspect or stop delegated tasks of this thread. action=status reads one task (status, workState, provider/model, duration, usage, summary = final result or error); reading a finished result acknowledges its automatic delivery. action=list lists this thread's tasks. action=cancel stops the child and everything it delegated.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["status", "list", "cancel"]},
                    "task_id": {"type": "string"},
                    "reason": {"type": "string"}
                },
                "required": ["action"]
            })
        },
    },
    ToolSpec {
        name: "t3_thread",
        description: "T3 orchestrator thread operations. action=runs lists a thread's runs (turns) with status and token usage. action=fork branches a thread from a run (default: latest) into a new thread that receives a portable context handoff; target may pick a different provider/model. action=merge_back brings a fork's work since the fork point back into its source thread (or target_thread). action=lineage shows forks and merges. action=send messages another thread: mode auto (start idle / steer active), queue (durable follow-up), steer (inject into the active turn) or restart (redirect the active turn). Omit thread to act on this thread.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["runs", "fork", "merge_back", "lineage", "send"]},
                    "thread": {"type": "string", "description": "Thread id; defaults to this thread."},
                    "run": {"type": "integer", "description": "Run ordinal (1-based) for fork/merge_back; default latest."},
                    "title": {"type": "string"},
                    "target": target_schema(),
                    "target_thread": {"type": "string", "description": "merge_back destination; default the fork's source."},
                    "message": {"type": "string"},
                    "mode": {"type": "string", "enum": ["auto", "queue", "steer", "restart"]}
                },
                "required": ["action"]
            })
        },
    },
    ToolSpec {
        name: "t3_queue",
        description: "T3 orchestrator durable message queue for a thread: follow-ups that survive restarts and run in order when the thread is idle. Actions: list, add (text), edit (id, text), reorder (id, before_id or null = end), remove (id), clear, hold / release (pause or resume draining), promote (id; deliver into the active turn as a steer, or how='restart' to redirect it). A user Stop holds the queue until the next message.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "add", "edit", "reorder", "remove", "clear", "hold", "release", "promote"]},
                    "thread": {"type": "string"},
                    "id": {"type": "string"},
                    "text": {"type": "string"},
                    "before_id": {"type": ["string", "null"]},
                    "how": {"type": "string", "enum": ["steer", "restart"]}
                },
                "required": ["action"]
            })
        },
    },
];

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct TaskArgs {
    action: String,
    task_id: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ThreadArgs {
    action: String,
    thread: Option<String>,
    run: Option<usize>,
    title: Option<String>,
    target: Option<Target>,
    target_thread: Option<String>,
    message: Option<String>,
    mode: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct QueueArgs {
    action: String,
    thread: Option<String>,
    id: Option<String>,
    text: Option<String>,
    before_id: Option<String>,
    how: Option<String>,
}

fn args<T: serde::de::DeserializeOwned + Default>(raw: &Value) -> Result<T> {
    if raw.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(raw.clone())
        .map_err(|e| OrchestratorError::invalid(format!("invalid arguments: {e}")))
}

fn thread_arg(raw: Option<&str>, ctx: &CallContext) -> Result<ThreadId> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => Ok(ThreadId::new(id)?),
        None => Ok(ctx.thread_id.clone()),
    }
}

fn required<'a>(value: &'a Option<String>, field: &str) -> Result<&'a str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| OrchestratorError::invalid(format!("{field} is required")))
}

impl Engine {
    /// Run one tool call. Never fails: errors become the T3 error document.
    pub async fn call_tool(&self, name: &str, ctx: &CallContext, input: &Value) -> Value {
        match self.dispatch_tool(name, ctx, input).await {
            Ok(value) => value,
            Err(error) => {
                if !error.is_correctable() {
                    tracing::warn!(tool = name, %error, "orchestrator tool failed");
                }
                error.to_tool_json()
            }
        }
    }

    async fn dispatch_tool(&self, name: &str, ctx: &CallContext, input: &Value) -> Result<Value> {
        self.ensure_thread(&ctx.thread_id, &ctx.scope, None).await?;
        match name {
            "t3_capabilities" => self.capabilities(ctx).await,
            "t3_delegate" => self.delegate(ctx, args::<DelegateRequest>(input)?).await,
            "t3_task" => {
                let a: TaskArgs = args(input)?;
                match a.action.as_str() {
                    "list" => self.task_list(&ctx.thread_id).await,
                    "status" => {
                        self.task_status(ctx, &TaskId::new(required(&a.task_id, "task_id")?)?)
                            .await
                    }
                    "cancel" => {
                        let reason = a.reason.as_deref().unwrap_or("cancelled by parent agent");
                        self.cancel_task(
                            ctx,
                            &TaskId::new(required(&a.task_id, "task_id")?)?,
                            reason,
                        )
                        .await
                    }
                    _ => Err(OrchestratorError::invalid(
                        "action must be status, list or cancel",
                    )),
                }
            }
            "t3_thread" => {
                let a: ThreadArgs = args(input)?;
                let thread_id = thread_arg(a.thread.as_deref(), ctx)?;
                match a.action.as_str() {
                    "runs" => {
                        self.runs(&self.owned_thread(&thread_id, &ctx.scope).await?)
                            .await
                    }
                    "lineage" => {
                        self.owned_thread(&thread_id, &ctx.scope).await?;
                        self.lineage(&thread_id).await
                    }
                    "fork" => {
                        let source = self.owned_thread(&thread_id, &ctx.scope).await?;
                        self.fork(
                            ctx,
                            &source,
                            a.run,
                            a.title.as_deref(),
                            a.target.as_ref(),
                            Author::Agent,
                        )
                        .await
                    }
                    "merge_back" => {
                        let fork = self.owned_thread(&thread_id, &ctx.scope).await?;
                        let target = a.target_thread.as_deref().map(ThreadId::new).transpose()?;
                        self.merge_back(ctx, &fork, target.as_ref(), a.run, None, Author::Agent)
                            .await
                    }
                    "send" => {
                        let mode = match a.mode.as_deref() {
                            None | Some("") => SendMode::Auto,
                            Some(m) => SendMode::parse(m).map_err(|_| {
                                OrchestratorError::invalid(
                                    "mode must be auto, queue, steer or restart",
                                )
                            })?,
                        };
                        self.send(ctx, &thread_id, a.message.as_deref().unwrap_or(""), mode)
                            .await
                    }
                    _ => Err(OrchestratorError::invalid(
                        "action must be runs, fork, merge_back, lineage or send",
                    )),
                }
            }
            "t3_queue" => {
                let a: QueueArgs = args(input)?;
                let thread_id = thread_arg(a.thread.as_deref(), ctx)?;
                let thread = self.owned_thread(&thread_id, &ctx.scope).await?;
                let item = || -> Result<QueueItemId> { QueueItemId::new(required(&a.id, "id")?) };
                match a.action.as_str() {
                    "list" => self.queue_list(&thread_id).await,
                    "add" => {
                        self.queue_add(&thread_id, a.text.as_deref().unwrap_or(""), Author::Agent)
                            .await
                    }
                    "clear" => self.queue_clear(&thread_id).await,
                    "hold" => self.queue_hold(&thread_id, true).await,
                    "release" => self.queue_hold(&thread_id, false).await,
                    "edit" => {
                        self.queue_edit(&thread_id, &item()?, a.text.as_deref().unwrap_or(""))
                            .await
                    }
                    "remove" => self.queue_remove(&thread_id, &item()?).await,
                    "reorder" => {
                        let before = a.before_id.as_deref().map(QueueItemId::new).transpose()?;
                        self.queue_reorder(&thread_id, &item()?, before.as_ref())
                            .await
                    }
                    "promote" => {
                        let how = match a.how.as_deref() {
                            Some("restart") => SteerHow::Restart,
                            _ => SteerHow::Steer,
                        };
                        self.queue_promote(&thread, &item()?, how).await
                    }
                    other => Err(OrchestratorError::invalid(format!(
                        "unknown action {other}"
                    ))),
                }
            }
            other => Err(OrchestratorError::invalid(format!("unknown tool {other}"))),
        }
    }
}
