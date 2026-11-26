//! Long-term memory for turns that do not run on IronClaw's own loop (CLI
//! agents): recalled context goes into the turn ([`TurnInput::memory`]),
//! and the completed exchange is handed back for capture. The loop's own
//! turns recall and capture inside the loop; the host's [`TurnMemory`]
//! skips them.
//!
//! Additive like the profile context: a recall that fails or finds nothing
//! leaves the turn as it was, and capture runs off the engine's path.
//!
//! [`TurnInput::memory`]: crate::runtime::TurnInput::memory

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;

use crate::model::ThreadRecord;
use crate::runtime::TurnOrigin;

/// The host's memory for CLI turns.
#[async_trait]
pub trait TurnMemory: Send + Sync {
    /// Context for a turn about to start on `thread` with `text`, ready to
    /// put in front of the agent; `None` when there is nothing to add.
    async fn recall(&self, thread: &ThreadRecord, text: &str) -> Option<String>;

    /// A completed turn: what was asked and what the agent answered.
    async fn capture(&self, thread: &ThreadRecord, asked: &str, answer: &str);
}

/// The engine's side: the plugged-in [`TurnMemory`] and, per thread, the
/// text of the run that will be captured once it completes.
#[derive(Default)]
pub(crate) struct TurnMemoryHooks {
    source: OnceLock<Arc<dyn TurnMemory>>,
    asked: Mutex<HashMap<ThreadId, (Option<String>, String)>>,
}

/// Turns whose text is the person's (or a delegating agent's brief): the
/// engine's own scaffolding (result deliveries, handoffs, auto-continue) is
/// neither recalled for nor remembered.
fn remembered(origin: TurnOrigin) -> bool {
    matches!(
        origin,
        TurnOrigin::UserPrompt | TurnOrigin::QueueDrain | TurnOrigin::DelegatedBrief
    )
}

impl TurnMemoryHooks {
    /// First call wins.
    pub(crate) fn set(&self, source: Arc<dyn TurnMemory>) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "first call wins by contract; a later wiring is ignored"
        )]
        let _ = self.source.set(source);
    }

    pub(crate) async fn recall(
        &self,
        thread: &ThreadRecord,
        text: &str,
        origin: TurnOrigin,
    ) -> Option<String> {
        if !remembered(origin) {
            return None;
        }
        self.source.get()?.recall(thread, text).await
    }

    /// `run_id` started with `text`: capture it when it completes.
    pub(crate) fn started(
        &self,
        thread: &ThreadId,
        run_id: Option<&str>,
        text: &str,
        origin: TurnOrigin,
    ) {
        if self.source.get().is_none() || !remembered(origin) {
            return;
        }
        if let Ok(mut asked) = self.asked.lock() {
            asked.insert(
                thread.clone(),
                (run_id.map(str::to_string), text.to_string()),
            );
        }
    }

    /// The text `run_id` was started with, once; `None` for another run or
    /// a run that is not remembered.
    pub(crate) fn take(&self, thread: &ThreadId, run_id: Option<&str>) -> Option<String> {
        let mut asked = self.asked.lock().ok()?;
        match asked.get(thread) {
            Some((started, _)) if started.as_deref() == run_id => {
                asked.remove(thread).map(|(_, text)| text)
            }
            _ => None,
        }
    }

    /// Hand a completed exchange to the host, off the caller's path.
    pub(crate) fn capture(&self, thread: ThreadRecord, asked: String, answer: String) {
        let Some(source) = self.source.get().cloned() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            source.capture(&thread, &asked, &answer).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nothing;

    #[async_trait]
    impl TurnMemory for Nothing {
        async fn recall(&self, _: &ThreadRecord, _: &str) -> Option<String> {
            Some("recalled".into())
        }
        async fn capture(&self, _: &ThreadRecord, _: &str, _: &str) {}
    }

    #[test]
    fn only_the_started_run_is_taken_and_only_once() {
        let hooks = TurnMemoryHooks::default();
        let thread = ThreadId::new("t1").unwrap();
        hooks.started(&thread, Some("r1"), "hi", TurnOrigin::UserPrompt);
        assert!(
            hooks.take(&thread, Some("r1")).is_none(),
            "nothing without a source"
        );
        hooks.set(Arc::new(Nothing));
        hooks.started(&thread, Some("r1"), "hi", TurnOrigin::UserPrompt);
        assert!(hooks.take(&thread, Some("r2")).is_none(), "another run");
        assert_eq!(hooks.take(&thread, Some("r1")).as_deref(), Some("hi"));
        assert!(hooks.take(&thread, Some("r1")).is_none(), "once");
        hooks.started(&thread, Some("r3"), "result", TurnOrigin::ResultDelivery);
        assert!(
            hooks.take(&thread, Some("r3")).is_none(),
            "scaffolding is not remembered"
        );
    }
}
