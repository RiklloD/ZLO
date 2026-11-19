//! The replies a steered turn still owes for the prompts it superseded.

use ironclaw_orchestrator::runtime::TurnOutcome;
use serde_json::Value;

use super::PromptFuture;
use crate::rpc::RpcError;

/// Replies still owed to prompts a steer superseded. Their ordinary stop
/// (`end_turn`, `cancelled`) means nothing; any other answer (a quota
/// error, a lost thinking signature, another failure) is kept so it is
/// classified like the turn's own answer instead of being masked by the
/// steer that followed.
#[derive(Default)]
pub(super) struct Superseded {
    pending: Vec<PromptFuture>,
    /// The first actionable reply, in arrival order.
    pub(super) error: Option<Result<Value, RpcError>>,
}

impl Superseded {
    pub(super) fn push(&mut self, reply: PromptFuture) {
        self.pending.push(reply);
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Settles every reply that has arrived; ready when at least one did.
    fn poll_settled(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        let mut settled = false;
        let mut i = 0;
        while i < self.pending.len() {
            if let std::task::Poll::Ready(reply) = self.pending[i].as_mut().poll(cx) {
                drop(self.pending.swap_remove(i));
                self.note(reply);
                settled = true;
            } else {
                i += 1;
            }
        }
        if settled {
            std::task::Poll::Ready(())
        } else {
            std::task::Poll::Pending
        }
    }

    /// Wait until a superseded reply arrives.
    pub(super) async fn settle(&mut self) {
        std::future::poll_fn(|cx| self.poll_settled(cx)).await;
    }

    /// Take the replies that already arrived, without waiting. The agent
    /// answers prompts in order, so a superseded reply sent before the
    /// final one is already routed when the turn ends.
    pub(super) fn settle_ready(&mut self) {
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        #[expect(
            clippy::let_underscore_must_use,
            reason = "what settled is recorded in `error`; pending replies are dropped"
        )]
        let _ = self.poll_settled(&mut cx);
    }

    fn note(&mut self, reply: Result<Value, RpcError>) {
        let ordinary = reply.as_ref().is_ok_and(|r| {
            matches!(
                r["stopReason"].as_str(),
                Some("end_turn") | Some("cancelled")
            )
        });
        if !ordinary && self.error.is_none() {
            self.error = Some(reply);
        }
    }
}

pub(super) fn is_usage_limit(outcome: &TurnOutcome) -> bool {
    outcome
        .error
        .as_deref()
        .is_some_and(|e| e.starts_with("usage_limit"))
}
