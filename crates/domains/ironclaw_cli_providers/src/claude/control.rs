//! The stream-json control side of one bridged turn: the PreToolUse hook
//! registration, its answers, the prompt that follows the CLI's ack, live
//! reply text, and closing stdin once the result is in.
//!
//! Every hook callback is answered with an explicit allow or deny. Never an
//! error response: the CLI treats a failed hook as "no opinion" and goes on
//! with its own permission rules, which auto-approve reads.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use ironclaw_host_api::ids::ThreadId;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::Mutex;

use super::steer::SteerLedger;
use crate::bridge::{CliBridge, ToolDecision};
use crate::interaction;

const INIT_REQUEST_ID: &str = "ironclaw-init";
const HOOK_CALLBACK_ID: &str = "ironclaw-approval";

pub(super) type SharedStdin = Arc<Mutex<Option<ChildStdin>>>;
pub(super) type SharedSteers = Arc<std::sync::Mutex<SteerLedger>>;

/// The `initialize` request that routes every tool call through our hook.
pub(super) fn initialize_request() -> String {
    json!({
        "type": "control_request",
        "request_id": INIT_REQUEST_ID,
        "request": {
            "subtype": "initialize",
            "hooks": {"PreToolUse": [{"matcher": null, "hookCallbackIds": [HOOK_CALLBACK_ID]}]},
        },
    })
    .to_string()
}

/// The user message; its `uuid` (the run id) lands in the session journal,
/// which is how history rows find their run.
pub(super) fn prompt_message(run_id: &str, text: &str) -> String {
    user_line(run_id, json!(text))
}

/// [`prompt_message`] plus the attached pictures as image blocks ahead of
/// the text (T3 Code's order), so the model sees them without a tool call.
pub(super) async fn prompt_with_images(run_id: &str, text: &str) -> String {
    let images = crate::attachments::inline_images(text).await;
    if images.is_empty() {
        return prompt_message(run_id, text);
    }
    let mut content: Vec<serde_json::Value> = images
        .iter()
        .map(|image| {
            json!({
                "type": "image",
                "source": {"type": "base64", "media_type": image.mime, "data": image.base64},
            })
        })
        .collect();
    content.push(json!({"type": "text", "text": text}));
    user_line(run_id, json!(content))
}

fn user_line(run_id: &str, content: serde_json::Value) -> String {
    json!({
        "type": "user",
        "uuid": run_id,
        "session_id": "",
        "parent_tool_use_id": null,
        "message": {"role": "user", "content": content},
    })
    .to_string()
}

/// A steer as a user message. `priority: "next"`: the CLI reads it once the
/// tool calls in flight finish (`"now"` cancels them and reports a refusal).
pub(super) async fn steer_message(uuid: &str, text: &str) -> String {
    let line = prompt_with_images(uuid, text).await;
    match serde_json::from_str::<Value>(&line) {
        Ok(mut frame) => {
            frame["priority"] = json!("next");
            frame.to_string()
        }
        Err(error) => {
            tracing::debug!(%error, "claude cli: steer frame left without a priority");
            line
        }
    }
}

/// Write a steer into the running turn, recorded in the ledger under the
/// stdin lock: a concurrent close either sees it open or happened first
/// (and the write fails, so the engine queues the text instead).
pub(super) async fn write_steer(
    stdin: &SharedStdin,
    steers: &SharedSteers,
    uuid: &str,
    text: &str,
    line: &str,
) -> std::io::Result<()> {
    let mut guard = stdin.lock().await;
    let Some(pipe) = guard.as_mut() else {
        return Err(std::io::ErrorKind::BrokenPipe.into());
    };
    with_ledger(steers, |l| l.sent(uuid, text));
    let written = async {
        pipe.write_all(format!("{line}\n").as_bytes()).await?;
        pipe.flush().await
    }
    .await;
    if written.is_err() {
        with_ledger(steers, |l| l.unsent(uuid));
    }
    written
}

pub(super) fn with_ledger<T>(steers: &SharedSteers, f: impl FnOnce(&mut SteerLedger) -> T) -> T {
    match steers.lock() {
        Ok(mut ledger) => f(&mut ledger),
        // A poisoned ledger only follows a panic elsewhere; its data is intact.
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

/// EOF on stdin once the ledger says nothing is open any more (or `force`).
/// Decided under the stdin lock, so a steer is never written after it.
async fn close_when_settled(stdin: &SharedStdin, steers: &SharedSteers, force: bool) {
    let mut guard = stdin.lock().await;
    if !force && !with_ledger(steers, |l| l.may_close()) {
        return;
    }
    if let Some(mut pipe) = guard.take() {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "the CLI may already be gone"
        )]
        let _ = pipe.shutdown().await;
    }
}

pub(super) async fn write_line(stdin: &SharedStdin, line: &str) -> std::io::Result<()> {
    let mut guard = stdin.lock().await;
    let Some(pipe) = guard.as_mut() else {
        return Err(std::io::ErrorKind::BrokenPipe.into());
    };
    pipe.write_all(format!("{line}\n").as_bytes()).await?;
    pipe.flush().await
}

/// EOF on stdin: the CLI finishes and exits.
pub(super) async fn close(stdin: &SharedStdin) {
    if let Some(mut pipe) = stdin.lock().await.take() {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "the CLI may already be gone"
        )]
        let _ = pipe.shutdown().await;
    }
}

fn hook_answer(request_id: &str, decision: &ToolDecision) -> String {
    // A rejection's feedback rides the hook's own reason field: Claude Code
    // hands `permissionDecisionReason` to the model as the denied call's
    // result, so "no, do X instead" reaches it without a separate steer.
    let (verdict, reason) = match decision.denial_message() {
        None => ("allow", "Approved in IronClaw.".to_string()),
        Some(reason) => ("deny", reason),
    };
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": {"hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": verdict,
                "permissionDecisionReason": reason,
            }},
        },
    })
    .to_string()
}

fn success(request_id: &str, response: Value) -> String {
    json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": request_id, "response": response},
    })
    .to_string()
}

fn error(request_id: &str, message: &str) -> String {
    json!({
        "type": "control_response",
        "response": {"subtype": "error", "request_id": request_id, "error": message},
    })
    .to_string()
}

/// What the turn loop should do after a line.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Step {
    Continue,
    /// The CLI refused the hook registration: tools would run unchecked.
    InitRefused,
}

/// The live handles one bridged turn shares with the runtime.
pub(super) struct TurnIo {
    pub bridge: Arc<CliBridge>,
    pub stdin: SharedStdin,
    pub thread: ThreadId,
    pub run_id: String,
    pub ready: Arc<std::sync::atomic::AtomicBool>,
    pub steers: SharedSteers,
}

pub(super) struct BridgedTurn {
    pub bridge: Arc<CliBridge>,
    pub stdin: SharedStdin,
    pub thread: ThreadId,
    pub run_id: String,
    /// Sent once the CLI acknowledged the hook registration.
    pub prompt: Option<String>,
    /// Set once that prompt is written: a steer before it would land first.
    ready: Arc<std::sync::atomic::AtomicBool>,
    /// What the CLI has read of the prompt and the steers.
    steers: SharedSteers,
    /// The session's task store; a finished task tool call re-reads it into
    /// the thread's plan. `None` leaves the plan alone.
    task_store: Option<PathBuf>,
    /// Tool use ids of task-tool calls let through in this turn.
    task_calls: HashSet<String>,
    /// The next text delta opens a new text block.
    new_block: bool,
    new_thinking_block: bool,
}

impl BridgedTurn {
    pub(super) fn new(turn: TurnIo, prompt: String, task_store: Option<PathBuf>) -> Self {
        Self {
            bridge: turn.bridge,
            stdin: turn.stdin,
            thread: turn.thread,
            run_id: turn.run_id,
            prompt: Some(prompt),
            ready: turn.ready,
            steers: turn.steers,
            task_store,
            task_calls: HashSet::new(),
            new_block: false,
            new_thinking_block: false,
        }
    }

    pub(super) fn awaiting_ack(&self) -> bool {
        self.prompt.is_some()
    }

    /// The result is in and only unacknowledged steers hold stdin open.
    pub(super) fn waiting_for_steer_ack(&self) -> bool {
        with_ledger(&self.steers, |l| l.waiting_for_ack())
    }

    /// The CLI ignored a steer for [`super::steer::ACK_TIMEOUT`]: end its
    /// input so it can finish; the steer is reported unread.
    pub(super) async fn give_up_on_steers(&self) {
        with_ledger(&self.steers, SteerLedger::abandon);
        close_when_settled(&self.stdin, &self.steers, true).await;
    }

    pub(super) async fn on_line(&mut self, line: &str) -> Step {
        let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
            return Step::Continue;
        };
        with_ledger(&self.steers, |l| l.observe(&frame));
        match frame.get("type").and_then(Value::as_str) {
            Some("control_response") => return self.on_ack(&frame["response"]).await,
            Some("control_request") => self.on_request(&frame).await,
            Some("stream_event") if frame["parent_tool_use_id"].is_null() => {
                self.on_stream_event(&frame["event"])
            }
            // Tool results come back as the user's turn: they finish the rows.
            Some("user") if frame["parent_tool_use_id"].is_null() => self.on_tool_results(&frame),
            // Input ends once every command written is done (see `steer.rs`).
            Some("result" | "command_lifecycle") => {
                close_when_settled(&self.stdin, &self.steers, false).await
            }
            _ => {}
        }
        Step::Continue
    }

    async fn on_ack(&mut self, response: &Value) -> Step {
        if response["request_id"].as_str() != Some(INIT_REQUEST_ID) {
            return Step::Continue;
        }
        if response["subtype"].as_str() != Some("success") {
            close(&self.stdin).await;
            return Step::InitRefused;
        }
        if let Some(prompt) = self.prompt.take() {
            with_ledger(&self.steers, |l| l.prompt(&self.run_id));
            match write_line(&self.stdin, &prompt).await {
                Ok(()) => self.ready.store(true, std::sync::atomic::Ordering::SeqCst),
                Err(error) => tracing::debug!(%error, "claude cli: prompt write failed"),
            }
        }
        Step::Continue
    }

    async fn on_request(&mut self, frame: &Value) {
        let Some(request_id) = frame["request_id"].as_str().map(str::to_string) else {
            return;
        };
        let request = &frame["request"];
        match request["subtype"].as_str() {
            Some("hook_callback") => {
                let input = &request["input"];
                let tool = input["tool_name"].as_str().unwrap_or("tool").to_string();
                let tool_input = input["tool_input"].clone();
                // The hook payload has no reason field, but Bash and Task
                // inputs carry the model's own one-line `description`.
                let reason = crate::approval_reason::explicit(&tool_input, &["description"]);
                let known = request["callback_id"].as_str() == Some(HOOK_CALLBACK_ID);
                let use_id = input["tool_use_id"].as_str().map(str::to_string);
                if known
                    && self
                        .on_interaction_tool(&request_id, &tool, &tool_input, use_id.as_deref())
                        .await
                {
                    return;
                }
                let (bridge, stdin) = (Arc::clone(&self.bridge), Arc::clone(&self.stdin));
                let (thread, run) = (self.thread.clone(), self.run_id.clone());
                // Claimed here, in stream order, before the answer is spawned.
                let call_id = input["tool_use_id"].as_str().map(str::to_string);
                let claim = known.then(|| bridge.claim_call(&thread, &run).with_call_id(call_id));
                // Answered off the read loop: the CLI keeps streaming meanwhile.
                tokio::spawn(async move {
                    let decision = if let Some(claim) = claim {
                        bridge
                            .decide_call(&thread, &run, &tool, &tool_input, reason, claim)
                            .await
                    } else {
                        ToolDecision::Deny("Unknown hook.".into())
                    };
                    if let Err(error) =
                        write_line(&stdin, &hook_answer(&request_id, &decision)).await
                    {
                        tracing::debug!(%error, "claude cli: hook answer not delivered");
                    }
                });
            }
            // An MCP server asks the person for structured input.
            Some("elicitation") => self.on_elicitation(request_id, request),
            // Only reached if a call slips past the hook; nobody prompts here.
            Some("can_use_tool") => {
                let answer = success(
                    &request_id,
                    json!({"behavior": "deny", "message": "Tool calls are approved through IronClaw."}),
                );
                #[expect(clippy::let_underscore_must_use, reason = "the CLI is gone")]
                let _ = write_line(&self.stdin, &answer).await;
            }
            _ => {
                #[expect(clippy::let_underscore_must_use, reason = "the CLI is gone")]
                let _ = write_line(&self.stdin, &error(&request_id, "unsupported")).await;
            }
        }
    }

    /// `TodoWrite` and `AskUserQuestion` are not approvals: the first
    /// updates the thread's plan and runs, the second becomes a form whose
    /// answer goes back as the hook's reason. `true` when handled here.
    async fn on_interaction_tool(
        &mut self,
        request_id: &str,
        tool: &str,
        input: &Value,
        call_id: Option<&str>,
    ) -> bool {
        match tool {
            // Bookkeeping in the session's own task store, or loading those
            // tools' schemas: no approval. The plan follows once a call ran.
            t if interaction::CLAUDE_TASK_TOOLS.contains(&t)
                || interaction::claude_task_tool_load(t, input) =>
            {
                if let Some(id) = call_id {
                    self.task_calls.insert(id.to_string());
                }
                let answer = hook_answer(request_id, &ToolDecision::Allow);
                if let Err(error) = write_line(&self.stdin, &answer).await {
                    tracing::debug!(%error, "claude cli: hook answer not delivered");
                }
                true
            }
            interaction::CLAUDE_TODO_TOOL => {
                if let Some(plan) = interaction::claude_todo_plan(input) {
                    self.bridge.plan_updated(&self.thread, &self.run_id, plan);
                }
                let answer = hook_answer(request_id, &ToolDecision::Allow);
                if let Err(error) = write_line(&self.stdin, &answer).await {
                    tracing::debug!(%error, "claude cli: hook answer not delivered");
                }
                true
            }
            interaction::CLAUDE_QUESTION_TOOL => {
                let Some(form) = interaction::claude_question_form(input) else {
                    return false; // not a shape we can draw: the ordinary gate asks
                };
                let (bridge, stdin) = (Arc::clone(&self.bridge), Arc::clone(&self.stdin));
                let (thread, run) = (self.thread.clone(), self.run_id.clone());
                let claim = bridge.claim_call(&thread, &run);
                let request_id = request_id.to_string();
                let tool = tool.to_string();
                tokio::spawn(async move {
                    let outcome = bridge.ask(&thread, &run, &tool, form.clone(), claim).await;
                    let reply = match outcome.answer() {
                        Some(answer) => interaction::claude_question_reply(&form, Some(answer)),
                        None => outcome.note(),
                    };
                    // Denied with the answer as the reason: the tool itself
                    // never runs, so the model reads exactly this text.
                    let answer = hook_answer(&request_id, &ToolDecision::Deny(reply));
                    if let Err(error) = write_line(&stdin, &answer).await {
                        tracing::debug!(%error, "claude cli: question answer not delivered");
                    }
                });
                true
            }
            _ => false,
        }
    }

    /// An MCP server's elicitation, relayed by the CLI: a form-mode request
    /// becomes a question; anything else (URL mode, a schema we cannot draw)
    /// is declined.
    fn on_elicitation(&self, request_id: String, request: &Value) {
        let server = request["mcp_server_name"].as_str().map(str::to_string);
        let message = request["message"].as_str().map(str::to_string);
        let form = (request["mode"].as_str().unwrap_or("form") == "form")
            .then(|| interaction::elicitation_form(server, message, &request["requested_schema"]))
            .flatten();
        let (bridge, stdin) = (Arc::clone(&self.bridge), Arc::clone(&self.stdin));
        let (thread, run) = (self.thread.clone(), self.run_id.clone());
        let claim = form.as_ref().map(|_| bridge.claim_call(&thread, &run));
        tokio::spawn(async move {
            let reply = match (form, claim) {
                (Some(form), Some(claim)) => {
                    let outcome = bridge
                        .ask(&thread, &run, "mcp_elicitation", form.clone(), claim)
                        .await;
                    interaction::elicitation_reply(&form, outcome.answer())
                }
                _ => json!({"action": "decline"}),
            };
            if let Err(error) = write_line(&stdin, &success(&request_id, reply)).await {
                tracing::debug!(%error, "claude cli: elicitation answer not delivered");
            }
        });
    }

    fn on_tool_results(&mut self, frame: &Value) {
        let Some(blocks) = frame["message"]["content"].as_array() else {
            return;
        };
        let mut tasks_changed = false;
        for block in blocks.iter().filter(|b| b["type"] == "tool_result") {
            if let Some(id) = block["tool_use_id"].as_str() {
                let failed = block["is_error"].as_bool().unwrap_or(false);
                if self.task_calls.remove(id) {
                    tasks_changed |= !failed;
                    continue;
                }
                self.bridge
                    .tool_finished(&self.thread, &self.run_id, id, failed);
            }
        }
        if let (true, Some(store)) = (tasks_changed, self.task_store.clone()) {
            let (bridge, thread, run) = (
                Arc::clone(&self.bridge),
                self.thread.clone(),
                self.run_id.clone(),
            );
            tokio::spawn(async move {
                if let Some(plan) = interaction::claude_task_plan(&store).await {
                    bridge.plan_updated(&thread, &run, plan);
                }
            });
        }
    }

    fn on_stream_event(&mut self, event: &Value) {
        match event["type"].as_str() {
            Some("content_block_start") if event["content_block"]["type"] == "text" => {
                self.new_block = true;
            }
            Some("content_block_start") if event["content_block"]["type"] == "thinking" => {
                self.new_thinking_block = true;
            }
            Some("content_block_delta") if event["delta"]["type"] == "thinking_delta" => {
                if let Some(text) = event["delta"]["thinking"].as_str() {
                    self.bridge.thinking_delta(
                        &self.thread,
                        &self.run_id,
                        text,
                        self.new_thinking_block,
                    );
                    self.new_thinking_block = false;
                }
            }
            Some("content_block_delta") if event["delta"]["type"] == "text_delta" => {
                if let Some(text) = event["delta"]["text"].as_str() {
                    self.bridge
                        .text_delta(&self.thread, &self.run_id, text, self.new_block);
                    self.new_block = false;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::agent_plan::{QuestionAnswer, QuestionValue};
    use tokio::io::{AsyncBufReadExt, BufReader};

    /// A bridged turn whose stdin is a `cat` child, so every answer the
    /// driver writes can be read back from `cat`'s stdout.
    fn turn_over_cat(
        bridge: &Arc<CliBridge>,
    ) -> (
        BridgedTurn,
        tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
        tokio::process::Child,
    ) {
        let mut child = tokio::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let out = BufReader::new(child.stdout.take().unwrap()).lines();
        let thread = ThreadId::new("claude-forms").unwrap();
        bridge.turn_started(&thread, "run-c");
        let turn = BridgedTurn::new(
            TurnIo {
                bridge: Arc::clone(bridge),
                stdin,
                thread,
                run_id: "run-c".into(),
                ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                steers: SharedSteers::default(),
            },
            String::new(),
            None,
        );
        (turn, out, child)
    }

    fn hook(request_id: &str, tool: &str, input: Value) -> String {
        json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "hook_callback",
                "callback_id": HOOK_CALLBACK_ID,
                "input": {"tool_name": tool, "tool_input": input, "tool_use_id": "toolu_1"},
            },
        })
        .to_string()
    }

    async fn next_frame(
        out: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    ) -> Value {
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), out.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[tokio::test]
    async fn todo_write_updates_the_plan_and_runs_without_a_gate() {
        let bridge = CliBridge::new(std::time::Duration::from_secs(5));
        let (mut turn, mut out, mut child) = turn_over_cat(&bridge);
        let todos = json!({"todos": [
            {"content": "Plan", "status": "completed", "activeForm": "Planning"},
            {"content": "Build", "status": "in_progress", "activeForm": "Building"}
        ]});
        turn.on_line(&hook("h1", "TodoWrite", todos)).await;
        let frame = next_frame(&mut out).await;
        assert_eq!(frame["response"]["request_id"], "h1");
        assert_eq!(
            frame["response"]["response"]["hookSpecificOutput"]["permissionDecision"],
            "allow"
        );
        let (_, _, plan) = bridge.plan_of(&turn.thread).unwrap();
        assert_eq!(plan.progress(), (1, 2));
        let snap = bridge.snapshot(&turn.thread).unwrap();
        assert!(
            snap.gates.is_empty() && snap.tools.is_empty(),
            "no gate, no raw tool row"
        );
        #[expect(
            clippy::let_underscore_must_use,
            reason = "test teardown; the child may already have exited"
        )]
        let _ = child.kill().await;
    }

    #[tokio::test]
    async fn ask_user_question_waits_on_the_form_and_returns_the_answer() {
        let bridge = CliBridge::new(std::time::Duration::from_secs(5));
        let (mut turn, mut out, mut child) = turn_over_cat(&bridge);
        let mut events = bridge.subscribe();
        let input = json!({"questions": [{
            "question": "Which color?", "header": "Color", "multiSelect": false,
            "options": [{"label": "Red", "description": ""}, {"label": "Blue", "description": ""}]
        }]});
        turn.on_line(&hook("h2", "AskUserQuestion", input)).await;
        let gate_ref = loop {
            if let crate::bridge::LiveEvent::QuestionOpened { question, .. } =
                events.recv().await.unwrap()
            {
                break question.gate_ref;
            }
        };
        let mut answer = QuestionAnswer::default();
        answer.values.insert(
            "Which color?".into(),
            QuestionValue::Choices(vec!["Teal".into()]),
        );
        assert_eq!(
            bridge.answer_question(&turn.thread, &gate_ref, Some(answer)),
            Ok(true)
        );
        let frame = next_frame(&mut out).await;
        let output = &frame["response"]["response"]["hookSpecificOutput"];
        assert_eq!(frame["response"]["request_id"], "h2");
        assert_eq!(output["permissionDecision"], "deny");
        let reason = output["permissionDecisionReason"].as_str().unwrap();
        assert!(reason.contains("Which color?: Teal"), "{reason}");
        #[expect(
            clippy::let_underscore_must_use,
            reason = "test teardown; the child may already have exited"
        )]
        let _ = child.kill().await;
    }

    #[tokio::test]
    async fn mcp_elicitation_is_a_form_and_a_dismissal_declines() {
        let bridge = CliBridge::new(std::time::Duration::from_secs(5));
        let (mut turn, mut out, mut child) = turn_over_cat(&bridge);
        let mut events = bridge.subscribe();
        let request = json!({
            "type": "control_request",
            "request_id": "e1",
            "request": {
                "subtype": "elicitation",
                "mcp_server_name": "github",
                "message": "Pick a repo",
                "requested_schema": {"type": "object", "properties": {"repo": {"type": "string"}}, "required": ["repo"]},
            },
        });
        turn.on_line(&request.to_string()).await;
        let question = loop {
            if let crate::bridge::LiveEvent::QuestionOpened { question, .. } =
                events.recv().await.unwrap()
            {
                break question;
            }
        };
        assert_eq!(question.form.source.as_deref(), Some("github"));
        assert_eq!(
            bridge.answer_question(&turn.thread, &question.gate_ref, None),
            Ok(true)
        );
        let frame = next_frame(&mut out).await;
        assert_eq!(frame["response"]["request_id"], "e1");
        assert_eq!(frame["response"]["response"], json!({"action": "decline"}));
        #[expect(
            clippy::let_underscore_must_use,
            reason = "test teardown; the child may already have exited"
        )]
        let _ = child.kill().await;
    }

    #[test]
    fn hook_answers_are_success_frames_with_a_decision() {
        let deny: Value =
            serde_json::from_str(&hook_answer("r1", &ToolDecision::Deny("no".into()))).unwrap();
        assert_eq!(deny["response"]["subtype"], "success");
        assert_eq!(
            deny["response"]["response"]["hookSpecificOutput"]["permissionDecision"],
            "deny"
        );
        let allow: Value = serde_json::from_str(&hook_answer("r2", &ToolDecision::Allow)).unwrap();
        assert_eq!(
            allow["response"]["response"]["hookSpecificOutput"]["permissionDecision"],
            "allow"
        );
    }
}
