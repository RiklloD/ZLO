//! Model-facing tools: `t3_capabilities`, `t3_delegate`, `t3_task`,
//! `t3_thread`, `t3_queue`, `t3_schedule` — the T3 orchestrator surface, same semantics as
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
use crate::worktree::{MergeStrategy, OnConflict};

/// How agents in orchestrated threads start subagents: only through these
/// tools. Runtimes whose agent has its own subagent tools add it to the
/// system prompt next to switching those tools off.
pub const DELEGATION_PROMPT: &str = include_str!("../prompts/delegation.md");

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
            "reasoning_effort": {"type": "string", "description": "Optional effort, e.g. low, medium, high."},
            "profile": {"type": "string", "description": "t3_delegate with context='brief' only: run the child as this profile (name from t3_capabilities). Omit to keep this thread's profile."}
        }
    })
}

/// `environment` on every tool that can run on a linked machine.
fn environment_prop() -> Value {
    json!({"type": "string", "description": "Run on a linked machine (id or label from t3_capabilities' environments); omit for this machine. t3_delegate also takes 'auto': the machine with the most headroom. On another machine, context='inherit' sends this conversation along as text, and this thread's git folder travels too where the environment supports it (Verpin cloud): its pushed base plus the unpushed commits, uncommitted and untracked files (secrets like .env are left out)."})
}

/// Add `environment` to a tool schema's properties.
fn with_environment(mut schema: Value) -> Value {
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.insert("environment".into(), environment_prop());
    }
    schema
}

pub const TOOLS: [ToolSpec; 8] = [
    ToolSpec {
        name: "t3_capabilities",
        description: "T3 orchestrator: list the providers and models this thread can delegate to, the inherited provider/model, and orchestration features and limits. Call before choosing a target for t3_delegate.",
        schema: || with_environment(json!({"type": "object", "properties": {}})),
    },
    ToolSpec {
        name: "t3_delegate",
        description: "T3 orchestrator: start a subagent, the ONLY way to start one (never a built-in subagent/agent/task tool). It runs in a new thread the user can see and steer. context='inherit': a subagent on this task (another model, a reviewer, a parallel slice): it gets this conversation so far and keeps this thread's profile. context='brief' (default): a side task, optionally as another target.profile: the child gets ONLY the task text, so include every fact, path and constraint it needs. mode='async' (default) returns a taskId immediately; the result is delivered back to this thread as a new turn when it finishes, so end your turn instead of polling. mode='wait' blocks until done or timeout_s (the child keeps running on timeout). For each review round, call t3_delegate again with the full brief, prior findings and objections, and a new client_request_id. isolation='worktree' gives the child its own git worktree and zlo/<slug> branch made from the HEAD of this thread's folder (uncommitted edits here stay out of it; fails if the folder is not a git repo): use it for children that edit code in parallel. A shared child of a thread that works in a worktree works in that same worktree. The project's .zlo/worktree.toml setup commands run only after the person approves them. When the child ends, a clean worktree is removed; one with changes is kept for t3_thread action=merge_worktree. Nothing is committed for the child: it commits what should be merged on its branch.",
        schema: || {
            with_environment(json!({
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "The child's task. With context='brief' it is all the child knows, so make it self-contained."},
                    "context": {"type": "string", "enum": ["inherit", "brief"], "description": "inherit: the child also gets this conversation so far (same profile; on another environment it travels as text with the same provider/model). brief (default): only the task."},
                    "isolation": {"type": "string", "enum": ["shared", "worktree"], "description": "shared (default): the child works in this thread's folder. worktree: its own git worktree and branch from this folder's committed HEAD; uncommitted edits here are not in it."},
                    "title": {"type": "string", "description": "Short label shown in the UI."},
                    "role": {"type": "string", "enum": ["implementation", "research", "review", "design", "test", "general"]},
                    "target": target_schema(),
                    "mode": {"type": "string", "enum": ["async", "wait"]},
                    "timeout_s": {"type": "number", "description": "Wait budget for mode=wait (default 600)."},
                    "client_request_id": {"type": "string", "description": "Stable per logical request; retries return the same task."}
                },
                "required": ["task"]
            }))
        },
    },
    ToolSpec {
        name: "t3_task",
        description: "T3 orchestrator: inspect or stop delegated tasks of this thread. action=status reads one task (status, workState, provider/model, duration, usage, summary = final result or error); reading a finished result acknowledges its automatic delivery. action=list lists this thread's tasks. action=cancel stops the child and everything it delegated.",
        schema: || {
            with_environment(json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["status", "list", "cancel"]},
                    "task_id": {"type": "string"},
                    "reason": {"type": "string"}
                },
                "required": ["action"]
            }))
        },
    },
    ToolSpec {
        name: "t3_thread",
        description: "T3 orchestrator thread operations. action=runs lists a thread's runs (turns) with status and token usage. action=fork branches a thread from a run (default: latest) into a new thread that receives a portable context handoff; target may pick a different provider/model. action=merge_back brings a fork's work since the fork point back into its source thread (or target_thread). action=lineage shows forks and merges. action=send messages another thread: mode auto (start idle / steer active), queue (durable follow-up), steer (inject into the active turn) or restart (redirect the active turn). action=worktree_status shows a thread's git worktree (branch, path, ahead/behind this checkout, dirty files). action=merge_worktree brings a finished child's worktree branch into the checkout it was made from (strategy merge|squash|cherry_pick). It refuses a worktree with uncommitted changes (nothing is committed for anyone) or whose HEAD left its branch with work on it, and a checkout with uncommitted tracked changes or a merge/rebase in progress; a merge that does not finish is rolled back (conflicts too, unless on_conflict='leave') and comes back as an error listing the files. action=remove_worktree removes the folder: refused while it has uncommitted changes; a branch with unmerged commits is kept and archived. Omit thread to act on this thread.",
        schema: || {
            with_environment(json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["runs", "fork", "merge_back", "lineage", "send", "worktree_status", "merge_worktree", "remove_worktree"]},
                    "thread": {"type": "string", "description": "Thread id; defaults to this thread."},
                    "run": {"type": "integer", "description": "Run ordinal (1-based) for fork/merge_back; default latest."},
                    "title": {"type": "string"},
                    "target": target_schema(),
                    "target_thread": {"type": "string", "description": "merge_back destination; default the fork's source."},
                    "message": {"type": "string"},
                    "mode": {"type": "string", "enum": ["auto", "queue", "steer", "restart"]},
                    "strategy": {"type": "string", "enum": ["merge", "squash", "cherry_pick"], "description": "merge_worktree: how the branch lands (default merge, a --no-ff merge commit)."},
                    "on_conflict": {"type": "string", "enum": ["abort", "leave"], "description": "merge_worktree: abort (default) restores the checkout; leave keeps the conflicted merge there."}
                },
                "required": ["action"]
            }))
        },
    },
    ToolSpec {
        name: "t3_queue",
        description: "T3 orchestrator durable message queue for a thread: follow-ups that survive restarts and run in order when the thread is idle. Actions: list, add (text), edit (id, text), reorder (id, before_id or null = end), remove (id), clear, hold / release (pause or resume draining), promote (id; deliver into the active turn as a steer, or how='restart' to redirect it). A user Stop holds the queue until the next message.",
        schema: || {
            with_environment(json!({
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
            }))
        },
    },
    ToolSpec {
        name: "t3_schedule",
        description: "T3 orchestrator scheduled prompts that run even when no turn is active. action=create with prompt and schedule as an OBJECT: {type:'interval', everyMs:3600000} (at least 60000) or {type:'fixed_time', timeOfDay:'09:00', weekdays:[1,2,3,4,5], timezone:'Europe/Zurich'} (0=Sunday; timezone defaults to the backend's local zone) or {type:'cron', expression:'0 9 * * MON-FRI', timezone:'Europe/Zurich'} (5-field cron; write weekdays as names, numbers count 1=Sunday) or {type:'webhook'} (runs per request to the returned loopback webhookUrl; prompt placeholders {{body.path}}, {{headers.name}}, {{query.name}}, {{body}}, {{request}}; optional signature {header, encoding:'hex'|'base64', prefix, secret} for HMAC-SHA256, or generate:true instead of secret to have the backend make one the user reveals on the Automations page). Runs post into THIS thread by default as a queued message (bind_to_current_thread=true; it waits while the thread is busy or stopped); false starts a fresh thread per run. Report nextRunAt. Also: list, update (id + fields; enabled=false pauses; create also takes enabled; an existing webhook signature can only be changed by the user on the Automations page), delete (id), run_now (id).",
        schema: || {
            with_environment(json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["create", "list", "update", "delete", "run_now"]},
                    "id": {"type": "string"},
                    "prompt": {"type": "string"},
                    "title": {"type": "string"},
                    "schedule": {"type": "object"},
                    "bind_to_current_thread": {"type": "boolean"},
                    "enabled": {"type": "boolean"},
                    "signature": {"type": ["object", "null"]},
                    "client_request_id": {"type": "string"}
                },
                "required": ["action"]
            }))
        },
    },
    ToolSpec {
        name: "t3_request_secret",
        description: "Ask the person for one secret (token, password, key) in a private card they can answer from any of their devices, instead of in the chat. One secret per call; never combine values. The value never enters the conversation. delivery='secretRef' (default): returns a one-use secretRef to pass to a tool that accepts one (e.g. t3_schedule signature {secret_ref}); you never see the value. delivery='file': returns an owner-only file path for commands, e.g. `sudo -S -p '' cmd < \"$file\"` or `$(cat \"$file\")` inside the command; never print it; it is deleted after 24 hours. Blocks until answered, declined or timeout_ms (default 10 minutes). If the person already pasted the value in this thread, pass it as value instead (no card).",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "label": {"type": "string", "description": "Card title, e.g. 'Deploy token'."},
                    "reason": {"type": "string", "description": "What it is for and where to get it."},
                    "url": {"type": "string", "description": "Sign-in or setup page for the value, shown as a button."},
                    "placeholder": {"type": "string"},
                    "delivery": {"type": "string", "enum": ["secretRef", "file"]},
                    "value": {"type": "string", "description": "Only a value the person pasted in this thread."},
                    "timeout_ms": {"type": "integer", "description": "1000..3600000, default 600000."}
                },
                "required": ["label", "reason"]
            })
        },
    },
    ToolSpec {
        name: "t3_environment_link",
        description: "Ask the person to link another of their machines (e.g. 'delegate this to my vps') so tools can run there with environment. Shows a card where they pick the machine and the access (read_only, operate, full) and paste a pairing code from it when needed; blocks until they answer or timeout_ms. Pass hint with their words for the machine; pass url only for an address they gave you (https, Tailscale, LAN or loopback), never a guess. Returns {status:'linked', environmentId, label, access}.",
        schema: || {
            json!({
                "type": "object",
                "properties": {
                    "hint": {"type": "string", "description": "The person's words for the machine, e.g. 'vps'."},
                    "environment_id": {"type": "string", "description": "A known machine's id, when mentioned."},
                    "url": {"type": "string", "description": "Only an address the person gave."},
                    "access": {"type": "string", "enum": ["read_only", "operate", "full"], "description": "Access to ask for; the person may lower it."},
                    "timeout_ms": {"type": "integer"}
                }
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
    strategy: Option<String>,
    on_conflict: Option<String>,
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

    /// Run one tool call that arrived over a peer link: never forwarded
    /// again (linked work cannot use this machine's own links).
    pub(crate) async fn call_tool_checked(
        &self,
        name: &str,
        ctx: &CallContext,
        input: &Value,
    ) -> Result<Value> {
        let mut input = input.clone();
        if let Some(map) = input.as_object_mut() {
            map.remove("environment");
        }
        self.dispatch_local(name, ctx, &input).await
    }

    async fn dispatch_tool(&self, name: &str, ctx: &CallContext, input: &Value) -> Result<Value> {
        if let Some(forwarded) = self.forward_tool(name, ctx, input).await {
            return forwarded;
        }
        self.dispatch_local(name, ctx, input).await
    }

    async fn dispatch_local(&self, name: &str, ctx: &CallContext, input: &Value) -> Result<Value> {
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
                    "worktree_status" => {
                        let thread = self.owned_thread(&thread_id, &ctx.scope).await?;
                        self.worktree_status(&thread).await
                    }
                    "merge_worktree" => {
                        let thread = self.owned_thread(&thread_id, &ctx.scope).await?;
                        let strategy = MergeStrategy::parse(a.strategy.as_deref())?;
                        let on_conflict = OnConflict::parse(a.on_conflict.as_deref())?;
                        self.merge_worktree(&thread, strategy, on_conflict, Some(&ctx.thread_id))
                            .await
                    }
                    "remove_worktree" => {
                        let thread = self.owned_thread(&thread_id, &ctx.scope).await?;
                        self.remove_worktree(&thread).await
                    }
                    _ => Err(OrchestratorError::invalid(
                        "action must be runs, fork, merge_back, lineage, send, worktree_status, merge_worktree or remove_worktree",
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
            "t3_schedule" => {
                let authority = crate::schedules::ScheduleAuthority::Model;
                self.schedule_tool(ctx, args(input)?, authority).await
            }
            "t3_request_secret" => self.request_secret_tool(ctx, input).await,
            "t3_environment_link" => self.environment_link_tool(ctx, input).await,
            other => Err(OrchestratorError::invalid(format!("unknown tool {other}"))),
        }
    }
}
