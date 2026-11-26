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

use super::input::Input;
use super::steer::SteerLedger;
use super::tasks::TaskRefresher;
use crate::bridge::{CliBridge, Receipt, ToolDecision};
use crate::interaction;

const INIT_REQUEST_ID: &str = "ironclaw-init";
const HOOK_CALLBACK_ID: &str = "ironclaw-approval";

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

/// [`prompt_with_images`] with the turn's recalled memories as their own
/// text block at the head (the transcript drops that block).
pub(super) async fn prompt_with_recalled(
    run_id: &str,
    text: &str,
    recalled: Option<&str>,
) -> String {
    let line = prompt_with_images(run_id, text).await;
    let Some(recalled) = recalled else {
        return line;
    };
    match serde_json::from_str::<serde_json::Value>(&line) {
        Ok(mut frame) => {
            let content = &mut frame["message"]["content"];
            let mut blocks = match content.take() {
                serde_json::Value::String(text) => vec![json!({"type": "text", "text": text})],
                serde_json::Value::Array(blocks) => blocks,
                other => vec![other],
            };
            blocks.insert(0, json!({"type": "text", "text": recalled}));
            *content = json!(blocks);
            frame.to_string()
        }
        Err(error) => {
            tracing::debug!(%error, "claude cli: prompt frame unreadable; sent without memory");
            line
        }
    }
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

pub(super) fn with_ledger<T>(steers: &SharedSteers, f: impl FnOnce(&mut SteerLedger) -> T) -> T {
    match steers.lock() {
        Ok(mut ledger) => f(&mut ledger),
        // A poisoned ledger only follows a panic elsewhere; its data is intact.
        Err(poisoned) => f(&mut poisoned.into_inner()),
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
    pub input: Input,
    pub thread: ThreadId,
    pub run_id: String,
    pub steers: SharedSteers,
    /// How long an unanswered tool call counts as work (see `busy`).
    pub tool_bound: std::time::Duration,
}

pub(super) struct BridgedTurn {
    pub bridge: Arc<CliBridge>,
    /// The turn's input owner: every frame to the CLI goes through it.
    pub input: Input,
    pub thread: ThreadId,
    pub run_id: String,
    /// Sent once the CLI acknowledged the hook registration.
    pub prompt: Option<String>,
    /// What the CLI has read of the prompt and the steers.
    steers: SharedSteers,
    /// The session's task store; a finished task tool call re-reads it into
    /// the thread's plan. `None` leaves the plan alone.
    tasks: Option<TaskRefresher>,
    /// Tool use ids of task-tool calls let through in this turn.
    task_calls: HashSet<String>,
    /// The next text delta opens a new text block.
    new_block: bool,
    new_thinking_block: bool,
    /// Tool calls the model made whose results are not back yet, and when
    /// each was seen; one older than `tool_bound` no longer counts as work.
    inflight: std::collections::HashMap<String, std::time::Instant>,
    tool_bound: std::time::Duration,
    /// Hook and elicitation answers still waiting (on a person, mostly).
    answering: Arc<Answers>,
}

/// Hook and elicitation answers in flight, and a wake-up for the read loop
/// when one finishes (it may then start counting silence).
#[derive(Default)]
pub(super) struct Answers {
    count: std::sync::atomic::AtomicUsize,
    finished: tokio::sync::Notify,
}

impl Answers {
    /// Resolves once an answer in flight has finished since the last call.
    pub(super) async fn finished(&self) {
        self.finished.notified().await;
    }
}

/// Counts one answer in flight until dropped.
struct Answering(Arc<Answers>);

impl Answering {
    fn new(answers: &Arc<Answers>) -> Self {
        answers
            .count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self(Arc::clone(answers))
    }
}

impl Drop for Answering {
    fn drop(&mut self) {
        self.0
            .count
            .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        self.0.finished.notify_one();
    }
}

impl BridgedTurn {
    pub(super) fn new(turn: TurnIo, prompt: String, task_store: Option<PathBuf>) -> Self {
        Self {
            bridge: turn.bridge,
            input: turn.input,
            thread: turn.thread,
            run_id: turn.run_id,
            prompt: Some(prompt),
            steers: turn.steers,
            tasks: task_store.map(TaskRefresher::new),
            task_calls: HashSet::new(),
            new_block: false,
            new_thinking_block: false,
            inflight: std::collections::HashMap::new(),
            tool_bound: turn.tool_bound,
            answering: Arc::default(),
        }
    }

    pub(super) fn awaiting_ack(&self) -> bool {
        self.prompt.is_some()
    }

    /// The result is in and only unacknowledged steers hold stdin open.
    pub(super) fn waiting_for_steer_ack(&self) -> bool {
        with_ledger(&self.steers, |l| l.waiting_for_ack())
    }

    /// The result is in and commands the CLI acknowledged keep stdin open.
    pub(super) fn holding_open(&self) -> bool {
        with_ledger(&self.steers, |l| l.held_open())
    }

    /// Until when silence is work, not a stuck CLI: `None` when nothing
    /// runs. A hook waiting on a person has no end of its own (its finish
    /// wakes the loop, see [`Self::answers`]); a tool call counts until
    /// `tool_bound` after it was seen, so one whose result never comes
    /// cannot hold the turn open forever.
    pub(super) fn busy_until(&self) -> Option<tokio::time::Instant> {
        let now = std::time::Instant::now();
        if self
            .answering
            .count
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            return Some(tokio::time::Instant::now() + self.tool_bound);
        }
        self.inflight
            .values()
            .map(|seen| *seen + self.tool_bound)
            .filter(|until| *until > now)
            .max()
            .map(tokio::time::Instant::from_std)
    }

    /// Answers in flight, for the read loop to wait on their finish.
    pub(super) fn answers(&self) -> Arc<Answers> {
        Arc::clone(&self.answering)
    }

    /// A frame that arrived once input was closed for good: nothing may be
    /// asked or answered any more, but whether the CLI started a steer
    /// still counts, so a steer it read is not sent again.
    pub(super) fn observe_only(&self, line: &str) {
        if let Ok(frame) = serde_json::from_str::<Value>(line.trim()) {
            with_ledger(&self.steers, |l| l.observe(&frame));
        }
    }

    /// The CLI ignored a steer for [`super::steer::ACK_TIMEOUT`]: end its
    /// input so it can finish; the steer is reported unread.
    pub(super) async fn give_up_on_steers(&self) {
        with_ledger(&self.steers, SteerLedger::abandon);
        self.input.close(true);
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
            Some("assistant") if frame["parent_tool_use_id"].is_null() => self.on_tool_uses(&frame),
            // Tool results come back as the user's turn: they finish the rows.
            Some("user") if frame["parent_tool_use_id"].is_null() => self.on_tool_results(&frame),
            // A result ends its native turn: no tool call of it still runs.
            // Input ends once every command written is done (see `steer.rs`):
            // the owner closes it; the read loop already treats a settled
            // ledger as closed, and the owner refuses steers from then on.
            Some("result") => {
                self.inflight.clear();
                self.input.close(false);
            }
            Some("command_lifecycle") => self.input.close(false),
            _ => {}
        }
        Step::Continue
    }

    async fn on_ack(&mut self, response: &Value) -> Step {
        if response["request_id"].as_str() != Some(INIT_REQUEST_ID) {
            return Step::Continue;
        }
        if response["subtype"].as_str() != Some("success") {
            self.input.close(true);
            return Step::InitRefused;
        }
        // The owner writes it, then the steers held for it, in order.
        if let Some(prompt) = self.prompt.take() {
            self.input.prompt(&self.run_id, prompt);
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
                let call_id = input["tool_use_id"].as_str().map(str::to_string);
                let (bridge, input) = (Arc::clone(&self.bridge), self.input.clone());
                let (thread, run) = (self.thread.clone(), self.run_id.clone());
                // Claimed here, in stream order, before the answer is spawned.
                let claim = known.then(|| bridge.claim_call(&thread, &run).with_call_id(call_id));
                let answering = Answering::new(&self.answering);
                // Answered off the read loop: the CLI keeps streaming meanwhile.
                tokio::spawn(async move {
                    let _answering = answering;
                    let (mut decision, mut receipt) = if let Some(claim) = claim {
                        bridge
                            .decide_call_receipted(&thread, &run, &tool, &tool_input, reason, claim)
                            .await
                    } else {
                        (ToolDecision::Deny("Unknown hook.".into()), Receipt::none())
                    };
                    // A steer that gave up waiting sends its words itself:
                    // the denial then goes plain, or they would arrive twice.
                    if !receipt.claim() {
                        decision = ToolDecision::Deny(crate::bridge::PLAIN_DECLINE.into());
                    }
                    // The receipt says whether the answer (a steer's words as
                    // its deny reason, maybe) is on the CLI's stdin.
                    let written = input.frame(hook_answer(&request_id, &decision)).await;
                    if !written {
                        tracing::debug!("claude cli: hook answer not delivered");
                    }
                    receipt.delivered(written);
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
                self.input.frame_unawaited(answer);
            }
            _ => self
                .input
                .frame_unawaited(error(&request_id, "unsupported")),
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
                self.input
                    .frame_unawaited(hook_answer(request_id, &ToolDecision::Allow));
                true
            }
            interaction::CLAUDE_TODO_TOOL => {
                if let Some(plan) = interaction::claude_todo_plan(input) {
                    self.bridge.plan_updated(&self.thread, &self.run_id, plan);
                }
                self.input
                    .frame_unawaited(hook_answer(request_id, &ToolDecision::Allow));
                true
            }
            interaction::CLAUDE_QUESTION_TOOL => {
                let Some(form) = interaction::claude_question_form(input) else {
                    return false; // not a shape we can draw: the ordinary gate asks
                };
                let (bridge, input) = (Arc::clone(&self.bridge), self.input.clone());
                let (thread, run) = (self.thread.clone(), self.run_id.clone());
                let claim = bridge.claim_call(&thread, &run);
                let request_id = request_id.to_string();
                let tool = tool.to_string();
                let answering = Answering::new(&self.answering);
                tokio::spawn(async move {
                    let _answering = answering;
                    let (mut outcome, mut receipt) = bridge
                        .ask_receipted(&thread, &run, &tool, form.clone(), claim)
                        .await;
                    // A steer that gave up waiting sends its words itself.
                    if !receipt.claim() {
                        outcome = crate::bridge::QuestionOutcome::Declined;
                    }
                    let replied = matches!(outcome, crate::bridge::QuestionOutcome::Replied(_));
                    let reply = match outcome.answer() {
                        Some(answer) => interaction::claude_question_reply(&form, Some(answer)),
                        None => outcome.note(),
                    };
                    // Denied with the answer as the reason: the tool itself
                    // never runs, so the model reads exactly this text (a
                    // steer's words included, when one answered it).
                    let answer = hook_answer(&request_id, &ToolDecision::Deny(reply));
                    let written = input.frame(answer).await;
                    if !written {
                        tracing::debug!("claude cli: question answer not delivered");
                    }
                    receipt.delivered(replied && written);
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
        let (bridge, input) = (Arc::clone(&self.bridge), self.input.clone());
        let (thread, run) = (self.thread.clone(), self.run_id.clone());
        let claim = form.as_ref().map(|_| bridge.claim_call(&thread, &run));
        let answering = Answering::new(&self.answering);
        tokio::spawn(async move {
            let _answering = answering;
            let (reply, receipt) = match (form, claim) {
                (Some(form), Some(claim)) => {
                    let (outcome, receipt) = bridge
                        .ask_receipted(&thread, &run, "mcp_elicitation", form.clone(), claim)
                        .await;
                    (
                        interaction::elicitation_reply(&form, outcome.answer()),
                        receipt,
                    )
                }
                _ => (json!({"action": "decline"}), Receipt::none()),
            };
            // An elicitation reply carries no free text: a steer that
            // answered it sends its words itself, after this is written.
            if !input.frame(success(&request_id, reply)).await {
                tracing::debug!("claude cli: elicitation answer not delivered");
            }
            receipt.delivered(false);
        });
    }

    fn on_tool_uses(&mut self, frame: &Value) {
        let Some(blocks) = frame["message"]["content"].as_array() else {
            return;
        };
        for block in blocks.iter().filter(|b| b["type"] == "tool_use") {
            if let Some(id) = block["id"].as_str() {
                self.inflight
                    .insert(id.to_string(), std::time::Instant::now());
            }
        }
    }

    fn on_tool_results(&mut self, frame: &Value) {
        let Some(blocks) = frame["message"]["content"].as_array() else {
            return;
        };
        let mut tasks_changed = false;
        for block in blocks.iter().filter(|b| b["type"] == "tool_result") {
            if let Some(id) = block["tool_use_id"].as_str() {
                self.inflight.remove(id);
                let failed = block["is_error"].as_bool().unwrap_or(false);
                if self.task_calls.remove(id) {
                    tasks_changed |= !failed;
                    continue;
                }
                self.bridge
                    .tool_finished(&self.thread, &self.run_id, id, failed);
            }
        }
        if let (true, Some(tasks)) = (tasks_changed, &self.tasks) {
            let (bridge, thread, run) = (
                Arc::clone(&self.bridge),
                self.thread.clone(),
                self.run_id.clone(),
            );
            tasks.refresh(move |plan| bridge.plan_updated(&thread, &run, plan));
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
        turn_with_store(bridge, None)
    }

    fn turn_with_store(
        bridge: &Arc<CliBridge>,
        task_store: Option<PathBuf>,
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
        let steers = SharedSteers::default();
        let input = Input::spawn(
            child.stdin.take().unwrap(),
            Arc::clone(&steers),
            super::super::input::DEFAULT_WRITE_TIMEOUT,
        );
        let out = BufReader::new(child.stdout.take().unwrap()).lines();
        let thread = ThreadId::new("claude-forms").unwrap();
        bridge.turn_started(&thread, "run-c");
        let turn = BridgedTurn::new(
            TurnIo {
                bridge: Arc::clone(bridge),
                input,
                thread,
                run_id: "run-c".into(),
                steers,
                tool_bound: std::time::Duration::from_secs(60),
            },
            String::new(),
            task_store,
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

    /// A task-tool call let through by the hook, then its result: the turn
    /// re-reads the task store into the plan.
    async fn run_task_call(
        turn: &mut BridgedTurn,
        out: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
        request: &str,
        use_id: &str,
    ) {
        let hook = json!({
            "type": "control_request",
            "request_id": request,
            "request": {
                "subtype": "hook_callback",
                "callback_id": HOOK_CALLBACK_ID,
                "input": {"tool_name": "TaskUpdate", "tool_input": {"taskId": "1"}, "tool_use_id": use_id},
            },
        });
        turn.on_line(&hook.to_string()).await;
        let frame = next_frame(out).await;
        assert_eq!(
            frame["response"]["response"]["hookSpecificOutput"]["permissionDecision"],
            "allow"
        );
        let result = json!({
            "type": "user",
            "parent_tool_use_id": null,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": use_id, "content": "Updated task #1"}
            ]},
        });
        turn.on_line(&result.to_string()).await;
    }

    async fn wait_for_plan(bridge: &CliBridge, thread: &ThreadId) -> Vec<String> {
        for _ in 0..250 {
            if let Some((_, _, plan)) = bridge.plan_of(thread) {
                return plan.entries.iter().map(|e| e.content.clone()).collect();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("no plan published");
    }

    /// The task store is the CLI's: a linked entry (pointing outside the
    /// store), an oversize entry and a FIFO are all skipped, and the FIFO
    /// does not hang the refresh; a linked store folder is not read at all.
    #[tokio::test]
    async fn task_refresh_reads_only_bounded_regular_files_of_a_real_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("tasks").join("session-1");
        std::fs::create_dir_all(&store).unwrap();
        let task = |id: &str, subject: &str| {
            format!(r#"{{"id":"{id}","subject":"{subject}","status":"pending"}}"#)
        };
        std::fs::write(store.join("1.json"), task("1", "Plan")).unwrap();
        let outside = dir.path().join("secret.json");
        std::fs::write(&outside, task("2", "Linked")).unwrap();
        std::os::unix::fs::symlink(&outside, store.join("2.json")).unwrap();
        let padding = "x".repeat(usize::try_from(super::super::tasks::MAX_FILE_BYTES).unwrap());
        std::fs::write(
            store.join("3.json"),
            format!(r#"{{"id":"3","subject":"Huge","status":"pending","pad":"{padding}"}}"#),
        )
        .unwrap();
        nix::unistd::mkfifo(&store.join("4.json"), nix::sys::stat::Mode::S_IRWXU).unwrap();

        let bridge = CliBridge::new(std::time::Duration::from_secs(5));
        let (mut turn, mut out, mut child) = turn_with_store(&bridge, Some(store.clone()));
        run_task_call(&mut turn, &mut out, "h1", "toolu_t1").await;
        let plan = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_for_plan(&bridge, &turn.thread),
        )
        .await
        .expect("the FIFO must not hang the refresh");
        assert_eq!(plan, ["Plan"]);

        // Deleting the last task clears the shown plan.
        let (seq, _, _) = bridge.plan_of(&turn.thread).unwrap();
        std::fs::remove_file(store.join("1.json")).unwrap();
        run_task_call(&mut turn, &mut out, "h2", "toolu_t2").await;
        let mut cleared = false;
        for _ in 0..250 {
            if let Some((next, _, plan)) = bridge.plan_of(&turn.thread)
                && next > seq
            {
                assert!(plan.entries.is_empty(), "{:?}", plan.entries);
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(cleared, "the emptied store was never published");

        // A store folder that is itself a link is never followed.
        let linked = dir.path().join("tasks").join("session-2");
        std::os::unix::fs::symlink(&store, &linked).unwrap();
        assert!(super::super::tasks::read_plan(&linked).await.is_none());
        assert!(
            super::super::tasks::read_plan(&dir.path().join("missing"))
                .await
                .is_none()
        );
        #[expect(
            clippy::let_underscore_must_use,
            reason = "test teardown; the child may already have exited"
        )]
        let _ = child.kill().await;
    }

    /// Two task calls finish close together and the first re-read is the
    /// slower one: the plan ends on the second (newer) read, not the first.
    #[tokio::test]
    async fn a_slower_older_task_refresh_never_replaces_a_newer_plan() {
        use ironclaw_host_api::agent_plan::{AgentPlan, PlanEntry, PlanEntryStatus};
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reader: super::super::tasks::PlanReader = {
            let calls = Arc::clone(&calls);
            Arc::new(move |_store| {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async move {
                    let (delay, subject) = if call == 0 { (300, "old") } else { (0, "new") };
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    Some(AgentPlan::new(
                        [PlanEntry::new(
                            subject.to_string(),
                            PlanEntryStatus::Pending,
                        )],
                        None,
                        chrono::Utc::now(),
                    ))
                })
            })
        };
        let bridge = CliBridge::new(std::time::Duration::from_secs(5));
        let (turn, mut out, mut child) = turn_over_cat(&bridge);
        let mut turn = turn;
        turn.tasks = Some(super::super::tasks::TaskRefresher::with_reader(
            PathBuf::from("unused"),
            reader,
        ));
        run_task_call(&mut turn, &mut out, "h1", "toolu_a").await;
        // The first read has started (and sleeps) before the second call.
        for _ in 0..100 {
            if calls.load(std::sync::atomic::Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        run_task_call(&mut turn, &mut out, "h2", "toolu_b").await;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(wait_for_plan(&bridge, &turn.thread).await, ["new"]);
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
