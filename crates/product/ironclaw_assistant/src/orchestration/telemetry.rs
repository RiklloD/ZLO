//! Per-turn telemetry for IronClaw's own agent loop: a diagnostic-sink
//! decorator that forwards every capture untouched and reports each finished
//! model call's context, speed and cache figures to the orchestrator, and
//! each assembled prompt's breakdown (what fills the context window).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_loop_host::HostManagedModelMessageRole;
use ironclaw_orchestrator::context_report::{ContextSample, IMAGE_TOKENS};
use ironclaw_product_contracts::inspector::PromptComponentKind;
use ironclaw_product_contracts::orchestration::ContextCategory;

use ironclaw_loop_host::{
    HostManagedModelCallDiagnosticCapture, HostManagedModelCallDiagnosticOutcome,
    HostManagedPromptDiagnosticCapture, HostManagedPromptDiagnosticSink,
    HostManagedToolInputDiagnosticCapture, HostManagedToolResultDiagnosticCapture,
    HostManagedToolStartedDiagnosticCapture,
};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::model::now_ms;
use ironclaw_orchestrator::runtime::TurnEventSink;
use ironclaw_orchestrator::telemetry::{TurnTelemetry, tokens_per_second};
use ironclaw_product_contracts::operator_llm::NativeModelRoute;

pub struct OrchestratorTelemetrySink {
    inner: Arc<dyn HostManagedPromptDiagnosticSink>,
    engine: Arc<Engine>,
    /// Each thread's context window, from its latest prompt (the loop's
    /// usage reports carry no window size).
    windows: Mutex<HashMap<ThreadId, u64>>,
}

impl OrchestratorTelemetrySink {
    pub fn new(inner: Arc<dyn HostManagedPromptDiagnosticSink>, engine: Arc<Engine>) -> Self {
        Self {
            inner,
            engine,
            windows: Mutex::new(HashMap::new()),
        }
    }

    fn sample(&self, capture: &HostManagedPromptDiagnosticCapture) {
        let sample = context_sample(capture);
        let thread_id = capture.context.thread_id.clone();
        if let Ok(mut windows) = self.windows.lock() {
            windows.insert(thread_id.clone(), capture.context_limit);
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let engine = Arc::clone(&self.engine);
        handle.spawn(async move {
            if let Err(error) = engine
                .record_context_sample(&thread_id, &sample, now_ms())
                .await
            {
                tracing::debug!(%error, "context sample not stored");
            }
        });
    }

    fn report(&self, capture: &HostManagedModelCallDiagnosticCapture) {
        let HostManagedModelCallDiagnosticCapture::Completed {
            diagnostic,
            duration_ms,
            outcome: HostManagedModelCallDiagnosticOutcome::Succeeded { usage: Some(usage) },
            ..
        } = capture
        else {
            return;
        };
        let (input, read, write) = (
            u64::from(usage.input_tokens),
            u64::from(usage.cache_read_input_tokens),
            u64::from(usage.cache_creation_input_tokens),
        );
        // Providers disagree on whether `input_tokens` already includes the
        // cache buckets (OpenAI-style) or excludes them (Anthropic). A read
        // larger than the input can only be the second shape.
        let fresh = if read > input { input } else { input - read };
        let telemetry = TurnTelemetry {
            // Routed calls carry `provider::model[::effort]`; show the model.
            model: diagnostic
                .effective_model
                .as_deref()
                .and_then(NativeModelRoute::plain_model)
                .or_else(|| NativeModelRoute::plain_model(&diagnostic.requested_model))
                .map(str::to_string),
            // Whole-call time (includes time to first token): the loop does
            // not see streamed frames.
            tps: tokens_per_second(u64::from(usage.output_tokens), *duration_ms),
            context_max: self
                .windows
                .lock()
                .ok()
                .and_then(|windows| windows.get(&diagnostic.context.thread_id).copied())
                .filter(|window| *window > 1),
            ..TurnTelemetry::from_prompt_tokens(fresh, read, write)
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let engine = Arc::clone(&self.engine);
        let thread_id = diagnostic.context.thread_id.clone();
        handle.spawn(async move {
            engine.turn_telemetry(&thread_id, telemetry, now_ms()).await;
        });
    }
}

impl HostManagedPromptDiagnosticSink for OrchestratorTelemetrySink {
    fn record_prompt(&self, capture: HostManagedPromptDiagnosticCapture) {
        self.sample(&capture);
        self.inner.record_prompt(capture);
    }

    fn record_model_call(&self, capture: HostManagedModelCallDiagnosticCapture) {
        self.report(&capture);
        self.inner.record_model_call(capture);
    }

    fn record_tool_input(&self, capture: HostManagedToolInputDiagnosticCapture) {
        self.inner.record_tool_input(capture);
    }

    fn record_tool_started(&self, capture: HostManagedToolStartedDiagnosticCapture) {
        self.inner.record_tool_started(capture);
    }

    fn record_tool_result(&self, capture: HostManagedToolResultDiagnosticCapture) {
        self.inner.record_tool_result(capture);
    }
}

/// The prompt's tokens by category, classified as the Inspector classifies
/// its components; a message's `<attachments>` block and image parts count
/// as attachments.
pub(crate) fn context_sample(capture: &HostManagedPromptDiagnosticCapture) -> ContextSample {
    let mut sample = ContextSample {
        window: (capture.context_limit > 1).then_some(capture.context_limit),
        turn_key: Some(capture.context.run_id.to_string()),
        ..ContextSample::default()
    };
    for (index, message) in capture.messages.iter().enumerate() {
        let kind = crate::inspector_store::prompt_component_kind(
            index,
            capture.identity_message_count,
            message,
        );
        // Summaries reach the model with the System role, so the ref decides
        // before the Inspector's kind does.
        let summary = message.content_ref.as_str().starts_with("msg:summary-");
        let category = match kind {
            _ if summary => ContextCategory::CompactionSummaries,
            PromptComponentKind::Identity | PromptComponentKind::System => {
                ContextCategory::SystemPrompt
            }
            PromptComponentKind::Instruction
            | PromptComponentKind::Skill
            | PromptComponentKind::Capability
            | PromptComponentKind::Other => ContextCategory::Instructions,
            PromptComponentKind::Conversation => match message.role {
                HostManagedModelMessageRole::User => ContextCategory::UserMessages,
                HostManagedModelMessageRole::Assistant => ContextCategory::AssistantMessages,
                HostManagedModelMessageRole::ToolResult => ContextCategory::ToolResults,
                HostManagedModelMessageRole::System => ContextCategory::SystemPrompt,
            },
        };
        let (text, attachments) = match message.content.rfind("\n\n<attachments>") {
            Some(at) => message.content.split_at(at),
            None => (message.content.as_str(), ""),
        };
        sample.add_text(category, text);
        sample.add_text(ContextCategory::Attachments, attachments);
        sample.add(
            ContextCategory::Attachments,
            u64::from(message.image_count).saturating_mul(IMAGE_TOKENS),
        );
    }
    for (capability_id, chars) in &capture.tool_definition_sizes {
        sample.add_tool(capability_id.as_str(), *chars);
    }
    sample
}

#[cfg(test)]
mod tests {
    use ironclaw_host_api::ids::{CapabilityId, TenantId};
    use ironclaw_host_api::turn::{
        LoopMessageRef, RunProfileId, RunProfileVersion, TurnId, TurnRunId, TurnScope,
    };
    use ironclaw_loop_contracts::{LoopRunContext, ResolvedRunProfile};
    use ironclaw_loop_host::HostManagedPromptDiagnosticMessage;
    use ironclaw_orchestrator::context_report::{estimate_tokens, reconcile};
    use ironclaw_product_contracts::orchestration::ContextToolOwner;

    use super::*;

    fn message(
        role: HostManagedModelMessageRole,
        content_ref: &str,
        content: &str,
        image_count: u32,
    ) -> HostManagedPromptDiagnosticMessage {
        HostManagedPromptDiagnosticMessage {
            role,
            content_ref: LoopMessageRef::new(content_ref).expect("ref"),
            content: content.to_string(),
            image_count,
        }
    }

    /// A prompt as the loop host assembles it: a compaction summary is a
    /// `MessageKind::Summary` row, which reaches the model with the System
    /// role and a `msg:summary-<id>` ref.
    fn capture() -> HostManagedPromptDiagnosticCapture {
        use HostManagedModelMessageRole::*;
        let context = LoopRunContext::new(
            TurnScope::new(
                TenantId::new("tenant").expect("tenant"),
                None,
                None,
                ThreadId::new("thread").expect("thread"),
            ),
            TurnId::new(),
            TurnRunId::new(),
            ResolvedRunProfile::legacy_compatibility(
                RunProfileId::interactive_default(),
                RunProfileVersion::new(1),
                true,
            ),
        );
        HostManagedPromptDiagnosticCapture {
            context,
            messages: vec![
                message(System, "msg:identity.soul", "You are ZLO.", 0),
                message(
                    System,
                    "msg:instruction.system.0.a",
                    "Read AGENTS.md first.",
                    0,
                ),
                message(
                    System,
                    "msg:snippet.skill.search.0.b",
                    "Search before answering.",
                    0,
                ),
                message(System, "msg:summary-1", "Earlier we planned the meter.", 0),
                message(
                    User,
                    "msg:user-1",
                    "look at this\n\n<attachments>\n- shot.png\n</attachments>",
                    2,
                ),
                message(Assistant, "msg:assistant-1", "Looking.", 0),
                message(ToolResult, "msg:tool-1", "file contents", 0),
            ],
            identity_message_count: 1,
            instruction_snippet_count: 2,
            active_skills: Vec::new(),
            capability_ids: Vec::new(),
            tool_definition_sizes: vec![
                (CapabilityId::new("builtin.shell").expect("cap"), 400),
                (CapabilityId::new("mcp-notion.search").expect("cap"), 4_000),
            ],
            requested_model: None,
            effective_model: None,
            context_limit: 200_000,
        }
    }

    #[test]
    fn native_prompts_split_into_every_category_the_report_shows() {
        let sample = context_sample(&capture());
        assert_eq!(sample.window, Some(200_000));
        assert_eq!(
            sample.tokens(ContextCategory::SystemPrompt),
            estimate_tokens("You are ZLO.")
        );
        assert_eq!(
            sample.tokens(ContextCategory::Instructions),
            estimate_tokens("Read AGENTS.md first.") + estimate_tokens("Search before answering.")
        );
        assert_eq!(
            sample.tokens(ContextCategory::CompactionSummaries),
            estimate_tokens("Earlier we planned the meter.")
        );
        assert_eq!(
            sample.tokens(ContextCategory::UserMessages),
            estimate_tokens("look at this")
        );
        assert_eq!(
            sample.tokens(ContextCategory::Attachments),
            estimate_tokens("\n\n<attachments>\n- shot.png\n</attachments>") + 2 * IMAGE_TOKENS
        );
        assert_eq!(
            sample.tokens(ContextCategory::AssistantMessages),
            estimate_tokens("Looking.")
        );
        assert_eq!(
            sample.tokens(ContextCategory::ToolResults),
            estimate_tokens("file contents")
        );
        assert_eq!(sample.tokens(ContextCategory::ToolDefinitions), 100 + 1_000);
        let fitted = reconcile(&sample, Some(9_000));
        assert_eq!(
            fitted.categories.iter().map(|c| c.tokens).sum::<u64>(),
            9_000
        );
        assert_eq!(
            fitted.tool_owners[0].owner,
            ContextToolOwner::Mcp("notion".into())
        );
    }

    #[tokio::test]
    async fn the_sink_stores_native_summaries_as_compaction_summaries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .expect("db"),
        );
        let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).expect("runtime"));
        let engine = Engine::open_libsql(runtime, "ironclaw")
            .await
            .expect("engine");
        let sink = OrchestratorTelemetrySink::new(
            Arc::new(crate::inspector_store::InMemoryDiagnosticStore::default()),
            Arc::clone(&engine),
        );
        let capture = capture();
        let thread = capture.context.thread_id.clone();
        sink.record_prompt(capture);
        let mut stored = None;
        for _ in 0..200 {
            stored = engine.store().context_sample(&thread).await.expect("read");
            if stored.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let stored = stored.expect("the sink stored the prompt's sample");
        assert_eq!(
            stored.tokens(ContextCategory::CompactionSummaries),
            estimate_tokens("Earlier we planned the meter.")
        );
        assert_eq!(
            stored.tokens(ContextCategory::SystemPrompt),
            estimate_tokens("You are ZLO.")
        );
        assert_eq!(stored.window, Some(200_000));
    }
}
