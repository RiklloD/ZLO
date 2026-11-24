//! The `t3_*` orchestrator tools as synthetic loop capabilities. Each call
//! acts on the thread the run belongs to, as the run's acting user; the
//! engine answers with its JSON document (errors included), which becomes
//! the model-visible result.
//!
//! Every one is declared mutating: the engine registers the calling thread on
//! each call, and the family delegates, messages, forks, merges, queues,
//! schedules, and cancels threads. So the synthetic dispatch boundary passes
//! each call through the ordinary approval gate before this handler runs
//! (auto-approve on: it runs; off: the user is asked), and a run nobody can
//! approve for is never offered them.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::{
    ids::{InvocationId, UserId},
    resolution::Resolution,
};
use ironclaw_loop_contracts::{
    AgentLoopHostError, AgentLoopHostErrorKind, CapabilityProgress, resolution,
};
use ironclaw_loop_host::{
    CapabilityResultWrite, DurablePersistence, SyntheticApproval, SyntheticCapability,
    SyntheticCapabilityDescriptor, SyntheticCapabilityEffect, SyntheticCapabilityHandler,
    SyntheticCapabilityInvocation,
};
use ironclaw_orchestrator::engine::{CallContext, Engine};
use ironclaw_orchestrator::model::ThreadScope;
use ironclaw_orchestrator::tools::TOOLS;

fn orchestrator_capability_id(tool_name: &str) -> String {
    format!("builtin.{tool_name}")
}

/// Build the orchestrator tools (`TOOLS`, `t3_schedule` included) for one run's capability surface.
pub fn orchestrator_capabilities(
    engine: Arc<Engine>,
    fallback_user_id: UserId,
) -> Result<Vec<SyntheticCapability>, AgentLoopHostError> {
    TOOLS
        .iter()
        .map(|tool| {
            Ok(SyntheticCapability::new(
                SyntheticCapabilityDescriptor::new(
                    &orchestrator_capability_id(tool.name),
                    tool.name,
                    tool.description,
                    (tool.schema)(),
                    SyntheticCapabilityEffect::Mutating(SyntheticApproval::new(
                        APPROVAL_SUMMARY,
                        format!(
                            "Run the T3 orchestrator tool {} (it can start, message, or stop threads)",
                            tool.name
                        ),
                    )),
                )?,
                Arc::new(OrchestratorToolHandler {
                    name: tool.name,
                    engine: Arc::clone(&engine),
                    fallback_user_id: fallback_user_id.clone(),
                }),
            ))
        })
        .collect()
}

/// Fixed, interpolation-free approval gate text.
const APPROVAL_SUMMARY: &str = "this orchestrator action requires approval";

struct OrchestratorToolHandler {
    name: &'static str,
    engine: Arc<Engine>,
    fallback_user_id: UserId,
}

#[async_trait]
impl SyntheticCapabilityHandler for OrchestratorToolHandler {
    fn validate_provider_arguments(
        &self,
        arguments: &serde_json::Value,
    ) -> Result<(), AgentLoopHostError> {
        if arguments.is_object() || arguments.is_null() {
            Ok(())
        } else {
            Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::InvalidInvocation,
                "orchestrator tool arguments must be an object",
            ))
        }
    }

    async fn invoke(
        &self,
        invocation: SyntheticCapabilityInvocation,
    ) -> Result<Resolution, AgentLoopHostError> {
        let run = &invocation.run_context;
        let scope = ThreadScope {
            tenant_id: run.scope.tenant_id.clone(),
            user_id: run.acting_user_id(&self.fallback_user_id),
            agent_id: run.scope.agent_id.clone(),
            project_id: run.scope.project_id.clone(),
        };
        // silent-ok: an unknown thread is registered by call_tool; the route is only inherited.
        let route = self
            .engine
            .store()
            .thread(&run.thread_id)
            .await
            .ok()
            .flatten();
        let ctx = CallContext {
            thread_id: run.thread_id.clone(),
            scope,
            provider: route.as_ref().and_then(|t| t.provider.clone()),
            model: route.and_then(|t| t.model),
            // A subagent this call starts is drawn under this run's turn.
            run_id: Some(run.run_id.to_string()),
        };
        let output = self
            .engine
            .call_tool(self.name, &ctx, &invocation.input)
            .await;
        let summary = match output.get("ok") {
            Some(serde_json::Value::Bool(false)) => format!(
                "{} failed: {}",
                self.name,
                output["error"].as_str().unwrap_or("error")
            ),
            _ => format!("{} done", self.name),
        };
        let write = invocation
            .result_writer
            .write_capability_result(CapabilityResultWrite {
                run_context: &invocation.run_context,
                // A resumed call writes against the input it was approved for.
                input_ref: invocation
                    .request
                    .approval_resume
                    .as_ref()
                    .map_or(&invocation.request.input_ref, |resume| &resume.input_ref),
                invocation_id: InvocationId::new(),
                capability_id: &invocation.request.capability_id,
                output,
                display_preview: None,
                durable_persistence: DurablePersistence::Persist,
            })
            .await?;
        Ok(resolution::completed(
            write.result_ref,
            summary,
            CapabilityProgress::MadeProgress,
            false,
            write.byte_len,
            write.output_digest,
            write.model_observation,
        ))
    }
}
