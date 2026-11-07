//! Worktree setup hooks (`.zlo/worktree.toml`) ask the person through the
//! same approval gate as any shell command a CLI agent runs: the gate opens
//! in the delegating thread's live turn, shows every command, is recorded in
//! `orch_cli_approvals`, and a "Full access" profile answers it by itself.
//! Without a live turn there is nobody to ask, and the hooks do not run.

use async_trait::async_trait;
use ironclaw_orchestrator::worktree::{
    HOOK_SCRIPT_MAX_CHARS, HookApproval, HookApprover, HookDecision,
};
use serde_json::json;

use crate::bridge::{CliBridge, GATE_BODY_MAX_CHARS, ToolDecision};

/// The tool name the gate shows for a worktree setup approval.
pub(crate) const WORKTREE_SETUP_TOOL: &str = "WorktreeSetup";

#[async_trait]
impl HookApprover for CliBridge {
    async fn approve(&self, request: &HookApproval) -> HookDecision {
        // The gate shows what it is asked about in full, or not at all.
        let length = request.script.chars().count();
        if length > HOOK_SCRIPT_MAX_CHARS.min(GATE_BODY_MAX_CHARS) {
            return HookDecision::Denied(format!(
                "The setup script is too long to show in full ({length} characters)."
            ));
        }
        let Some(run) = self.snapshot(&request.thread).map(|s| s.run_id) else {
            return HookDecision::Unavailable;
        };
        let call = uuid::Uuid::new_v4().to_string();
        let claim = self
            .claim_call(&request.thread, &run)
            .with_call_id(Some(call.clone()));
        let reason = format!(
            "The repository asks to run these setup commands in a new worktree \
             (content {}).",
            request.content_hash.chars().take(12).collect::<String>()
        );
        match self
            .decide_call(
                &request.thread,
                &run,
                WORKTREE_SETUP_TOOL,
                &json!({ "command": request.script }),
                Some(reason),
                claim,
            )
            .await
        {
            ToolDecision::Allow => {
                // The gate's job ends with the answer; the commands' output
                // goes to the worktree's setup log.
                self.tool_finished(&request.thread, &run, &call, false);
                HookDecision::Approved
            }
            ToolDecision::Deny(reason) => HookDecision::Denied(reason),
            // A rejection with feedback is still a denial: the hooks never run.
            ToolDecision::Rejected(feedback) => HookDecision::Denied(feedback.as_str().to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use ironclaw_host_api::ids::ThreadId;

    use super::*;

    fn request(thread: &ThreadId) -> HookApproval {
        HookApproval {
            thread: thread.clone(),
            repo_root: PathBuf::from("/repo"),
            script: "on_create:\n$ npm install".into(),
            content_hash: "abc123".into(),
        }
    }

    #[tokio::test]
    async fn setup_hooks_ask_through_the_live_turns_gate() {
        let bridge = CliBridge::new(Duration::from_secs(5));
        let thread = ThreadId::new("parent").unwrap();
        bridge.turn_started(&thread, "run-0");
        // Longer than the gate shows: refused without a gate.
        let mut long = request(&thread);
        long.script = format!("{}\n$ touch hidden", "$ true\n".repeat(400));
        assert!(matches!(
            bridge.approve(&long).await,
            HookDecision::Denied(_)
        ));
        assert!(bridge.snapshot(&thread).unwrap().gates.is_empty());
        bridge.turn_ended(&thread, "run-0", "completed");
        // No live turn: nobody to ask, nothing runs.
        assert_eq!(
            bridge.approve(&request(&thread)).await,
            HookDecision::Unavailable
        );

        bridge.turn_started(&thread, "run-1");
        for approved in [true, false] {
            let asking = {
                let bridge = Arc::clone(&bridge);
                let thread = thread.clone();
                tokio::spawn(async move { bridge.approve(&request(&thread)).await })
            };
            let gate = loop {
                if let Some(gate) = bridge
                    .snapshot(&thread)
                    .and_then(|s| s.gates.into_iter().next())
                {
                    break gate;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            assert_eq!(gate.tool_name, WORKTREE_SETUP_TOOL);
            assert!(gate.body.as_deref().unwrap_or("").contains("npm install"));
            let answer = if approved {
                crate::bridge::GateAnswer::Approve
            } else {
                crate::bridge::GateAnswer::Decline { feedback: None }
            };
            assert!(bridge.resolve_gate(&thread, &gate.gate_ref, answer));
            let decision = asking.await.unwrap();
            if approved {
                assert_eq!(decision, HookDecision::Approved);
            } else {
                assert!(matches!(decision, HookDecision::Denied(_)), "{decision:?}");
            }
        }
    }
}
