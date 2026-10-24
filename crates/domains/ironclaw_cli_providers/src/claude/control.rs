//! The stream-json control side of one bridged turn: the PreToolUse hook
//! registration, its answers, the prompt that follows the CLI's ack, live
//! reply text, and closing stdin once the result is in.
//!
//! Every hook callback is answered with an explicit allow or deny. Never an
//! error response: the CLI treats a failed hook as "no opinion" and goes on
//! with its own permission rules, which auto-approve reads.

use std::sync::Arc;

use ironclaw_host_api::ids::ThreadId;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::Mutex;

use crate::bridge::{CliBridge, ToolDecision};

const INIT_REQUEST_ID: &str = "ironclaw-init";
const HOOK_CALLBACK_ID: &str = "ironclaw-approval";

pub(super) type SharedStdin = Arc<Mutex<Option<ChildStdin>>>;

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
    json!({
        "type": "user",
        "uuid": run_id,
        "session_id": "",
        "parent_tool_use_id": null,
        "message": {"role": "user", "content": text},
    })
    .to_string()
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
        let _ = pipe.shutdown().await; // silent-ok: the CLI may already be gone
    }
}

fn hook_answer(request_id: &str, decision: &ToolDecision) -> String {
    let (verdict, reason) = match decision {
        ToolDecision::Allow => ("allow", "Approved in IronClaw."),
        ToolDecision::Deny(reason) => ("deny", reason.as_str()),
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

pub(super) struct BridgedTurn {
    pub bridge: Arc<CliBridge>,
    pub stdin: SharedStdin,
    pub thread: ThreadId,
    pub run_id: String,
    /// Sent once the CLI acknowledged the hook registration.
    pub prompt: Option<String>,
    /// The next text delta opens a new text block.
    new_block: bool,
}

impl BridgedTurn {
    pub fn new(
        bridge: Arc<CliBridge>,
        stdin: SharedStdin,
        thread: ThreadId,
        run_id: String,
        prompt: String,
    ) -> Self {
        Self {
            bridge,
            stdin,
            thread,
            run_id,
            prompt: Some(prompt),
            new_block: false,
        }
    }

    pub fn awaiting_ack(&self) -> bool {
        self.prompt.is_some()
    }

    pub async fn on_line(&mut self, line: &str) -> Step {
        let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
            return Step::Continue;
        };
        match frame.get("type").and_then(Value::as_str) {
            Some("control_response") => return self.on_ack(&frame["response"]).await,
            Some("control_request") => self.on_request(&frame).await,
            Some("stream_event") if frame["parent_tool_use_id"].is_null() => {
                self.on_stream_event(&frame["event"])
            }
            Some("result") => close(&self.stdin).await,
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
        if let Some(prompt) = self.prompt.take()
            && let Err(error) = write_line(&self.stdin, &prompt).await
        {
            tracing::debug!(%error, "claude cli: prompt write failed");
        }
        Step::Continue
    }

    async fn on_request(&self, frame: &Value) {
        let Some(request_id) = frame["request_id"].as_str().map(str::to_string) else {
            return;
        };
        let request = &frame["request"];
        match request["subtype"].as_str() {
            Some("hook_callback") => {
                let input = &request["input"];
                let tool = input["tool_name"].as_str().unwrap_or("tool").to_string();
                let tool_input = input["tool_input"].clone();
                let known = request["callback_id"].as_str() == Some(HOOK_CALLBACK_ID);
                let (bridge, stdin) = (Arc::clone(&self.bridge), Arc::clone(&self.stdin));
                let (thread, run) = (self.thread.clone(), self.run_id.clone());
                // Answered off the read loop: the CLI keeps streaming meanwhile.
                tokio::spawn(async move {
                    let decision = if known {
                        bridge
                            .request_approval(&thread, &run, &tool, &tool_input)
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
            // Only reached if a call slips past the hook; nobody prompts here.
            Some("can_use_tool") => {
                let answer = success(
                    &request_id,
                    json!({"behavior": "deny", "message": "Tool calls are approved through IronClaw."}),
                );
                let _ = write_line(&self.stdin, &answer).await; // silent-ok: the CLI is gone
            }
            _ => {
                let _ = write_line(&self.stdin, &error(&request_id, "unsupported")).await; // silent-ok: the CLI is gone
            }
        }
    }

    fn on_stream_event(&mut self, event: &Value) {
        match event["type"].as_str() {
            Some("content_block_start") if event["content_block"]["type"] == "text" => {
                self.new_block = true;
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
