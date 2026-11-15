//! `RebornScriptedReply` — terse façade for scripting one model turn in a
//! Reborn integration test; each reply maps 1:1 to a `TraceStep`. Raw
//! `TraceStep`/`LlmTrace`/`TraceResponse` construction is forbidden in new
//! Reborn integration tests (design §4.2) — use this.

// dead_code: `support_unit_tests.rs` mounts `reborn_support` without
// consuming this module, so symbols read unused there under `-D warnings`
// (matches sibling support modules).
#![allow(dead_code)]

use crate::support::trace_llm::{TraceResponse, TraceStep, TraceToolCall};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TOOL_CALL_ID: AtomicU64 = AtomicU64::new(1);

/// Provider-reported token usage for one scripted model call — the four
/// counts `ironclaw_llm`'s `CompletionResponse`/`ToolCompletionResponse`
/// carry (the provider contract has no reasoning-token count). Every reply
/// defaults to all-zero, so scripts that never call
/// [`RebornScriptedReply::with_usage`] report exactly what they always did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScriptedTokenUsage {
    /// Prompt (input) tokens.
    pub input: u32,
    /// Completion (output) tokens.
    pub output: u32,
    /// Input tokens served from the provider's prompt cache.
    pub cache_read: u32,
    /// Input tokens written into the provider's prompt cache.
    pub cache_creation: u32,
}

impl ScriptedTokenUsage {
    /// Plain prompt/completion usage with no prompt-cache tokens.
    pub fn new(input: u32, output: u32) -> Self {
        Self {
            input,
            output,
            ..Self::default()
        }
    }

    /// The same usage with prompt-cache read/creation counts.
    pub fn with_cache(mut self, cache_read: u32, cache_creation: u32) -> Self {
        self.cache_read = cache_read;
        self.cache_creation = cache_creation;
        self
    }
}

/// One scripted model turn.
pub struct RebornScriptedReply {
    step: TraceStep,
}

impl RebornScriptedReply {
    /// A plain assistant text reply.
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            step: TraceStep {
                request_hint: None,
                response: TraceResponse::Text {
                    content: content.into(),
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
                expected_tool_results: Vec::new(),
            },
        }
    }

    /// Scripts one model tool-call turn (CapabilityId, e.g. `"builtin.http"`).
    /// Applies the `'.' → "__"` encoding `ProviderToolName::new` requires (it
    /// rejects dots); `model_replay.rs`'s `trace_provider_tool_name` has an
    /// identical, independent encoder for the fixture-replay seam — keep both
    /// in sync if the mapping changes. NOT collision-safe (`.` vs `__` can
    /// collide) and not length-truncated — fine for single-capability tests
    /// only. The `id` auto-fills from a process-scoped counter, canonicalized
    /// per trace by `scripted_provider::scripted_trace_llm`.
    pub fn tool_call(capability_id: &str, arguments: serde_json::Value) -> Self {
        let name = capability_id.replace('.', "__");
        let id = format!("call-{}", NEXT_TOOL_CALL_ID.fetch_add(1, Ordering::Relaxed));
        Self {
            step: TraceStep {
                request_hint: None,
                response: TraceResponse::ToolCalls {
                    tool_calls: vec![TraceToolCall {
                        id,
                        name,
                        arguments,
                    }],
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
                expected_tool_results: Vec::new(),
            },
        }
    }

    /// Scripts one model turn carrying MULTIPLE tool calls (a "parallel"
    /// tool-calls turn from ONE model call). Each pair gets `tool_call`'s
    /// same encoding/id treatment. Still counts as exactly ONE script entry —
    /// the caller must follow with one more entry (the post-execution model
    /// call reacting to the tool results).
    pub fn tool_calls<'a>(calls: impl IntoIterator<Item = (&'a str, serde_json::Value)>) -> Self {
        let tool_calls = calls
            .into_iter()
            .map(|(capability_id, arguments)| TraceToolCall {
                id: format!("call-{}", NEXT_TOOL_CALL_ID.fetch_add(1, Ordering::Relaxed)),
                name: capability_id.replace('.', "__"),
                arguments,
            })
            .collect();
        Self {
            step: TraceStep {
                request_hint: None,
                response: TraceResponse::ToolCalls {
                    tool_calls,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                },
                expected_tool_results: Vec::new(),
            },
        }
    }

    /// Declare the token usage the scripted provider reports for this model
    /// call (it flows out of the vendor-SDK seam like a real provider's
    /// response usage, so every decorator above sees it).
    pub fn with_usage(mut self, usage: ScriptedTokenUsage) -> Self {
        match &mut self.step.response {
            TraceResponse::Text {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                ..
            }
            | TraceResponse::ToolCalls {
                input_tokens,
                output_tokens,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                ..
            } => {
                *input_tokens = usage.input;
                *output_tokens = usage.output;
                *cache_read_input_tokens = usage.cache_read;
                *cache_creation_input_tokens = usage.cache_creation;
            }
            // Never constructed by this façade: it only builds model replies.
            TraceResponse::UserInput { .. } => {}
        }
        self
    }

    /// Consume into the underlying replay step (crate-internal seam used by
    /// `scripted_provider::scripted_trace_llm`).
    pub(crate) fn into_step(self) -> TraceStep {
        self.step
    }
}
