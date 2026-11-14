use async_trait::async_trait;
use ironclaw_loop_contracts::AgentLoopDriverHost;

use crate::planner::AgentLoopPlannerInternal;

use super::{
    AgentLoopExecutorError, AssistantReplyStage, BudgetStage, CapabilityStage, ExitStage,
    InputStage, ModelStage, PostCapabilityStage, PromptStage, ReplyAdmissionStage, StopStage,
};

#[derive(Clone, Copy)]
pub(crate) struct StageContext<'a> {
    pub(crate) planner: &'a dyn AgentLoopPlannerInternal,
    pub(crate) host: &'a (dyn AgentLoopDriverHost + Send + Sync),
}

/// One executor stage, invoked through a boxed (`async_trait`) future.
///
/// # Stack depth
///
/// The executor's poll frames sit in the middle of the deepest stack in the
/// product: process supervisor → turn executor → this pipeline → capability
/// stage → loop-host ports → `CapabilityHost` → host runtime → runtime lane.
/// Unoptimized builds do not share stack slots between temporaries, so an
/// `async fn` body reserves a slot sized to the callee future at every
/// `.await` site (typically two per site). With many call sites of 10–20 KB
/// helper futures, `CapabilityStage::process` polled in a 787 KB frame and
/// `DefaultExecutorPipeline::execute` in a 529 KB one, and a tool call
/// overflowed the 2 MiB default thread stack.
///
/// The fix is at the callee, not the call site: helpers awaited from many
/// sites return `Pin<Box<impl Future>>` (the body is built straight on the
/// heap, so each caller slot is one pointer). `Box::pin(helper(..))` at a call
/// site does *not* help — the unboxed future is still materialized in the
/// caller's frame first. The tool-dispatch journey runs on a 1.5 MiB thread in
/// `tests/integration/tool_call.rs` to keep this from regressing.
#[async_trait]
pub(crate) trait ExecutorStage<Input>: Send + Sync {
    type Output;

    async fn process(
        &self,
        ctx: StageContext<'_>,
        input: Input,
    ) -> Result<Self::Output, AgentLoopExecutorError>;
}

#[derive(Debug, Clone)]
pub(crate) struct DefaultExecutorPipeline {
    pub(crate) budget: BudgetStage,
    pub(crate) input: InputStage,
    pub(crate) prompt: PromptStage,
    pub(crate) model: ModelStage,
    pub(crate) reply_admission: ReplyAdmissionStage,
    pub(crate) assistant_reply: AssistantReplyStage,
    pub(crate) capabilities: CapabilityStage,
    pub(crate) post_capability: PostCapabilityStage,
    pub(crate) stop: StopStage,
    pub(crate) exit: ExitStage,
}

impl Default for DefaultExecutorPipeline {
    fn default() -> Self {
        Self {
            budget: BudgetStage,
            input: InputStage,
            prompt: PromptStage,
            model: ModelStage,
            reply_admission: ReplyAdmissionStage,
            assistant_reply: AssistantReplyStage,
            capabilities: CapabilityStage,
            post_capability: PostCapabilityStage::default(),
            stop: StopStage,
            exit: ExitStage,
        }
    }
}
