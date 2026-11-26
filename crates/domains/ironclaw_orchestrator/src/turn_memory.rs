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
use crate::runtime::{RecalledMemory, TurnOrigin};

/// Longest a recall may hold up a turn's start.
const RECALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The host's memory for CLI turns.
#[async_trait]
pub trait TurnMemory: Send + Sync {
    /// Memory for a turn about to start on `thread` with the person's
    /// `text`; `None` when there is nothing to add.
    async fn recall(&self, thread: &ThreadRecord, text: &str) -> Option<RecalledMemory>;

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

/// The person's own words in a turn's text, when the turn is theirs: a
/// typed or queued message, without a context handoff another thread's
/// transcript was prepended as. The engine's own text (delegation briefs,
/// result deliveries, handoffs, auto-continue) is neither recalled for nor
/// remembered.
fn own_words(text: &str, origin: TurnOrigin) -> Option<&str> {
    if !matches!(origin, TurnOrigin::UserPrompt | TurnOrigin::QueueDrain) {
        return None;
    }
    let words = crate::handoff::strip_context(text).unwrap_or(text);
    // `@thread` / `@file` digests ZLO expanded into the message are another
    // thread's or file's content, not the person's words.
    let words = ironclaw_product_contracts::mention::strip_mention_context(words).trim();
    (!words.is_empty()).then_some(words)
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

    /// Bounded by [`RECALL_TIMEOUT`]: a slow memory never holds a turn.
    pub(crate) async fn recall(
        &self,
        thread: &ThreadRecord,
        text: &str,
        origin: TurnOrigin,
    ) -> Option<RecalledMemory> {
        let words = own_words(text, origin)?;
        let source = self.source.get()?;
        match tokio::time::timeout(RECALL_TIMEOUT, source.recall(thread, words)).await {
            Ok(memory) => memory,
            Err(_elapsed) => {
                tracing::debug!(thread = %thread.thread_id, "memory recall timed out; turn runs without it");
                None // silent-ok: recall is additive context, the turn still runs
            }
        }
    }

    /// `run_id` started with `text`: capture it when it completes.
    pub(crate) fn started(
        &self,
        thread: &ThreadId,
        run_id: Option<&str>,
        text: &str,
        origin: TurnOrigin,
    ) {
        if self.source.get().is_none() {
            return;
        }
        let Some(words) = own_words(text, origin) else {
            return;
        };
        if let Ok(mut asked) = self.asked.lock() {
            asked.insert(
                thread.clone(),
                (run_id.map(str::to_string), words.to_string()),
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
        async fn recall(&self, _: &ThreadRecord, _: &str) -> Option<RecalledMemory> {
            None
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
        hooks.started(&thread, Some("r4"), "brief", TurnOrigin::DelegatedBrief);
        assert!(
            hooks.take(&thread, Some("r4")).is_none(),
            "a brief is the engine's text"
        );
        let handed =
            "Context handoff (a). their transcript\n\n[End of context handoff]\n\n---\n\nmy words";
        hooks.started(&thread, Some("r5"), handed, TurnOrigin::UserPrompt);
        assert_eq!(hooks.take(&thread, Some("r5")).as_deref(), Some("my words"));
        let mentioned = format!(
            "compare with @a{}a's transcript{}",
            ironclaw_product_contracts::mention::MENTION_CONTEXT_OPEN,
            ironclaw_product_contracts::mention::MENTION_CONTEXT_CLOSE
        );
        hooks.started(&thread, Some("r6"), &mentioned, TurnOrigin::UserPrompt);
        assert_eq!(
            hooks.take(&thread, Some("r6")).as_deref(),
            Some("compare with @a")
        );
    }
}
