//! The replies a steered turn still owes for the prompts it superseded.

use ironclaw_orchestrator::runtime::TurnOutcome;
use serde_json::Value;

use super::PromptFuture;
use crate::rpc::RpcError;

/// Replies still owed to prompts a steer superseded. Their ordinary stop
/// (`end_turn`, `cancelled`) means nothing; any other answer (a quota
/// error, a lost thinking signature, another failure) is kept, every one of
/// them, so the turn's outcome is decided over all of them (see [`pick`])
/// instead of being masked by the steer that followed or by an earlier,
/// less actionable failure. Arrival order does not matter.
#[derive(Default)]
pub(super) struct Superseded {
    pending: Vec<PromptFuture>,
    /// Every actionable reply that settled.
    pub(super) errors: Vec<Result<Value, RpcError>>,
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
            reason = "what settled is recorded in `errors`; pending replies are dropped"
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
        if !ordinary {
            self.errors.push(reply);
        }
    }
}

/// The superseded outcome that decides the turn: a usage limit first (the
/// latest reset wins, so the resume waits long enough), else the first.
pub(super) fn pick(outcomes: Vec<TurnOutcome>) -> Option<TurnOutcome> {
    let limit = outcomes
        .iter()
        .enumerate()
        .filter(|(_, o)| is_usage_limit(o))
        .max_by_key(|(_, o)| o.usage_limit_resets_at)
        .map(|(i, _)| i);
    let index = limit.unwrap_or(0);
    outcomes.into_iter().nth(index)
}

pub(super) fn is_usage_limit(outcome: &TurnOutcome) -> bool {
    outcome
        .error
        .as_deref()
        .is_some_and(|e| e.starts_with("usage_limit"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_orchestrator::runtime::TurnEndStatus;

    fn failed(error: &str, reset: Option<i64>) -> TurnOutcome {
        TurnOutcome {
            usage_limit_resets_at: reset,
            ..TurnOutcome::ended(TurnEndStatus::Failed, Some(error.into()))
        }
    }

    #[test]
    fn a_usage_limit_wins_over_any_order_and_the_latest_reset_wins() {
        let picked = pick(vec![
            failed("antigravity error", None),
            failed("usage_limit: a", Some(10)),
            failed("usage_limit: b", Some(30)),
            failed("other", None),
        ])
        .unwrap();
        assert_eq!(picked.usage_limit_resets_at, Some(30));
        let first = pick(vec![failed("one", None), failed("two", None)]).unwrap();
        assert_eq!(first.error.as_deref(), Some("one"));
        assert!(pick(Vec::new()).is_none());
    }
}
