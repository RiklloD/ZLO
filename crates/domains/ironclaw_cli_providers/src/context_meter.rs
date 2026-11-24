//! What ZLO puts in a CLI agent's context besides the transcript, as one
//! value per turn. Each runtime builds the text it sends from [`Notes`] and,
//! once the agent got that text, records their token counts in its
//! [`SentContexts`], which is what it reports to ZLO's context meter: the
//! meter shows what was sent, not an estimate beside it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::context_report::{ContextSample, estimate_tokens};
use ironclaw_product_contracts::orchestration::ContextCategory;

/// Notes in the order they are sent, each with its context-meter category.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Notes(Vec<(ContextCategory, String)>);

impl Notes {
    fn push(mut self, category: ContextCategory, note: Option<String>) -> Self {
        if let Some(note) = note {
            self.0.push((category, note));
        }
        self
    }

    /// Part of the agent's system prompt (the profile's SOUL.md).
    pub(crate) fn system_prompt(self, note: Option<String>) -> Self {
        self.push(ContextCategory::SystemPrompt, note)
    }

    /// An instruction note (delegation, visuals, skills, goal, ...).
    pub(crate) fn instructions(self, note: Option<String>) -> Self {
        self.push(ContextCategory::Instructions, note)
    }

    /// These notes, then `more`.
    pub(crate) fn then(mut self, more: Notes) -> Self {
        self.0.extend(more.0);
        self
    }

    /// The notes as sent: joined by blank lines; `None` when there are none.
    pub(crate) fn text(&self) -> Option<String> {
        let parts: Vec<&str> = self.0.iter().map(|(_, note)| note.as_str()).collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// Token counts per category: what a record keeps instead of the text.
    fn counts(&self) -> Counts {
        let mut counts = Counts::default();
        for (category, note) in &self.0 {
            counts.add(*category, estimate_tokens(note));
        }
        counts
    }
}

/// Estimated tokens per context-meter category, in first-seen order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Counts(Vec<(ContextCategory, u64)>);

impl Counts {
    fn add(&mut self, category: ContextCategory, tokens: u64) {
        match self.0.iter_mut().find(|(c, _)| *c == category) {
            Some((_, total)) => *total = total.saturating_add(tokens),
            None => self.0.push((category, tokens)),
        }
    }

    fn add_to(&self, sample: &mut ContextSample) {
        for (category, tokens) in &self.0 {
            sample.add(*category, *tokens);
        }
    }
}

/// Threads a runtime keeps a record for. The record is a cache: a thread
/// past the cap falls back to the runtime's own estimate.
pub(crate) const MAX_RECORDED_THREADS: usize = 512;

/// What one thread's agent holds from ZLO after its latest delivered turn,
/// as token counts (never the notes' text).
#[derive(Debug, Clone, Default)]
struct Sent {
    /// Notes the session keeps (a system prompt, a first-turn preamble).
    session: Counts,
    /// Notes only the latest turn carried (requested skills, the goal).
    turn: Counts,
    /// Whether the latest turn's agent got ZLO's loopback MCP server.
    mcp_tools: bool,
    /// When it was last written, for evicting the oldest past the cap.
    written: u64,
}

#[derive(Debug, Default)]
struct Records {
    threads: HashMap<ThreadId, Sent>,
    clock: u64,
}

/// Per thread, what a runtime last delivered ([`Notes`] counts plus the MCP
/// server), for at most [`MAX_RECORDED_THREADS`] threads.
#[derive(Debug, Default)]
pub(crate) struct SentContexts(Mutex<Records>);

impl SentContexts {
    /// One turn's sends, recorded only once [`Delivery::delivered`] says the
    /// agent got them: a launch that fails keeps the thread's earlier record.
    /// `session` replaces the thread's kept notes when the turn sends them
    /// (`None`: the session still holds the earlier ones); `turn` always
    /// replaces the per-turn notes.
    pub(crate) fn pending(
        self: &Arc<Self>,
        thread: &ThreadId,
        session: Option<&Notes>,
        turn: &Notes,
        mcp_tools: bool,
    ) -> Delivery {
        Delivery {
            sent: Arc::clone(self),
            thread: thread.clone(),
            session: session.map(Notes::counts),
            turn: turn.counts(),
            mcp_tools: AtomicBool::new(mcp_tools),
        }
    }

    fn commit(&self, delivery: &Delivery) {
        let Ok(mut records) = self.0.lock() else {
            return; // silent-ok: a poisoned map only follows a panic elsewhere; the fallback is reported
        };
        records.clock = records.clock.saturating_add(1);
        let written = records.clock;
        if !records.threads.contains_key(&delivery.thread)
            && records.threads.len() >= MAX_RECORDED_THREADS
            && let Some(oldest) = records
                .threads
                .iter()
                .min_by_key(|(_, sent)| sent.written)
                .map(|(thread, _)| thread.clone())
        {
            records.threads.remove(&oldest);
        }
        let entry = records.threads.entry(delivery.thread.clone()).or_default();
        if let Some(session) = &delivery.session {
            entry.session = session.clone();
        }
        entry.turn = delivery.turn.clone();
        entry.mcp_tools = delivery.mcp_tools.load(Ordering::SeqCst);
        entry.written = written;
    }

    /// The meter's sample of what `thread` was sent. Without a record of it
    /// (no turn of it delivered in this process: a backend restart, a failed
    /// launch, or evicted past the cap), `fallback` gives the notes and MCP
    /// flag a turn would send now, from the runtime's own builders.
    pub(crate) fn sample(
        &self,
        thread: &ThreadId,
        fallback: impl FnOnce() -> (Notes, bool),
    ) -> ContextSample {
        let recorded = self
            .0
            .lock()
            .ok()
            .and_then(|r| r.threads.get(thread).cloned()); // silent-ok: a poisoned map only follows a panic elsewhere; the fallback is reported
        match recorded {
            Some(sent) => {
                let mut sample = ContextSample::default();
                sent.session.add_to(&mut sample);
                sent.turn.add_to(&mut sample);
                add_mcp_tools(&mut sample, sent.mcp_tools);
                sample
            }
            None => {
                let (notes, mcp_tools) = fallback();
                sample_of(&notes, mcp_tools)
            }
        }
    }
}

/// One turn's sends, waiting for the agent to get them. Committing is
/// idempotent; dropping it uncommitted keeps the thread's earlier record.
#[derive(Debug)]
pub(crate) struct Delivery {
    sent: Arc<SentContexts>,
    thread: ThreadId,
    session: Option<Counts>,
    turn: Counts,
    mcp_tools: AtomicBool,
}

impl Delivery {
    /// Whether the agent gets the MCP server after all (known only once it
    /// answered its handshake, which comes before the prompt).
    pub(crate) fn set_mcp_tools(&self, mcp_tools: bool) {
        self.mcp_tools.store(mcp_tools, Ordering::SeqCst);
    }

    /// The prompt carrying these sends is with the agent: record them.
    pub(crate) fn delivered(&self) {
        self.sent.commit(self);
    }
}

/// `notes`, plus the orchestrator tools when the agent got the MCP server.
pub(crate) fn sample_of(notes: &Notes, mcp_tools: bool) -> ContextSample {
    let mut sample = ContextSample::default();
    notes.counts().add_to(&mut sample);
    add_mcp_tools(&mut sample, mcp_tools);
    sample
}

/// The orchestrator tools, when the agent got the MCP server.
fn add_mcp_tools(sample: &mut ContextSample, mcp_tools: bool) {
    if mcp_tools {
        for tool in &ironclaw_orchestrator::tools::TOOLS {
            let chars =
                tool.name.len() + tool.description.len() + (tool.schema)().to_string().len();
            sample.add_tool(
                &format!("mcp-{}.{}", crate::bridge::MCP_SERVER_NAME, tool.name),
                u64::try_from(chars).unwrap_or(u64::MAX),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use ironclaw_product_contracts::orchestration::ContextToolOwner;

    use super::*;

    fn thread() -> ThreadId {
        ThreadId::new("t-1").unwrap()
    }

    fn record(
        sent: &Arc<SentContexts>,
        thread: &ThreadId,
        session: Option<&Notes>,
        turn: &Notes,
        mcp: bool,
    ) {
        sent.pending(thread, session, turn, mcp).delivered();
    }

    #[test]
    fn the_sample_is_the_recorded_notes_and_the_mcp_tools_only_when_sent() {
        let sent = Arc::new(SentContexts::default());
        let session = Notes::default()
            .system_prompt(Some("You are careful. ".repeat(40)))
            .instructions(Some("delegate".into()));
        record(&sent, &thread(), Some(&session), &Notes::default(), false);
        let without = sent.sample(&thread(), || unreachable!("recorded"));
        assert!(without.tools.is_empty(), "no MCP server, no tools counted");
        assert!(without.tokens(ContextCategory::SystemPrompt) > 100);
        assert_eq!(without, sample_of(&session, false), "counts match the text");

        // A later turn without session notes keeps the earlier ones.
        let goal = Notes::default().instructions(Some("goal".into()));
        record(&sent, &thread(), None, &goal, true);
        let with = sent.sample(&thread(), || unreachable!("recorded"));
        assert_eq!(
            with.tokens(ContextCategory::SystemPrompt),
            without.tokens(ContextCategory::SystemPrompt)
        );
        assert_eq!(with.tools.len(), 1);
        assert_eq!(
            with.tools[0].owner,
            ContextToolOwner::Mcp("ironclaw".into())
        );
        assert_eq!(
            with.tools[0].tools as usize,
            ironclaw_orchestrator::tools::TOOLS.len()
        );
        assert_eq!(with, sample_of(&session.clone().then(goal.clone()), true));

        // The handshake turned the MCP server down before the prompt went.
        let delivery = sent.pending(&thread(), None, &goal, true);
        delivery.set_mcp_tools(false);
        delivery.delivered();
        assert!(sent.sample(&thread(), || unreachable!()).tools.is_empty());
    }

    #[test]
    fn an_undelivered_turn_keeps_the_earlier_record_or_none() {
        let sent = Arc::new(SentContexts::default());
        let first = Notes::default().instructions(Some("first goal".into()));
        record(&sent, &thread(), Some(&first), &Notes::default(), false);
        let before = sent.sample(&thread(), || unreachable!("recorded"));
        // The launch failed: its delivery is dropped uncommitted.
        let long = Notes::default().instructions(Some("x".repeat(4_000)));
        drop(sent.pending(&thread(), Some(&long), &long, true));
        assert_eq!(sent.sample(&thread(), || unreachable!("recorded")), before);

        let other = ThreadId::new("t-2").unwrap();
        drop(sent.pending(&other, Some(&long), &long, true));
        let fallback = sent.sample(&other, || (Notes::default(), false));
        assert_eq!(
            fallback,
            ContextSample::default(),
            "the fallback, not the failed send"
        );
    }

    #[test]
    fn a_record_keeps_counts_not_the_notes_text() {
        let sent = Arc::new(SentContexts::default());
        let secret = "SOUL-TEXT-THAT-MUST-NOT-STAY-IN-MEMORY";
        let notes = Notes::default()
            .system_prompt(Some(secret.repeat(20)))
            .instructions(Some(format!("goal: {secret}")));
        let delivery = sent.pending(&thread(), Some(&notes), &notes, false);
        assert!(!format!("{delivery:?}").contains(secret), "{delivery:?}");
        delivery.delivered();
        let kept = format!("{:?}", sent.0.lock().unwrap());
        assert!(!kept.contains(secret), "{kept}");
        assert!(
            sent.sample(&thread(), || unreachable!("recorded"))
                .tokens(ContextCategory::SystemPrompt)
                > 100
        );
    }

    #[test]
    fn the_record_holds_at_most_the_cap_and_evicts_the_oldest_write() {
        let sent = Arc::new(SentContexts::default());
        let id = |n: usize| ThreadId::new(format!("t-{n}")).unwrap();
        let notes = Notes::default().instructions(Some("note".into()));
        for n in 0..MAX_RECORDED_THREADS {
            record(&sent, &id(n), Some(&notes), &Notes::default(), false);
        }
        // Thread 0 is written again, so thread 1 is now the oldest.
        record(&sent, &id(0), None, &Notes::default(), false);
        record(
            &sent,
            &id(MAX_RECORDED_THREADS),
            Some(&notes),
            &Notes::default(),
            false,
        );
        assert_eq!(sent.0.lock().unwrap().threads.len(), MAX_RECORDED_THREADS);
        let fallback = || (Notes::default(), false);
        assert_eq!(
            sent.sample(&id(1), fallback),
            ContextSample::default(),
            "evicted"
        );
        for n in [0, 2, MAX_RECORDED_THREADS] {
            assert!(sent.sample(&id(n), || unreachable!("kept")).total() > 0);
        }
    }

    #[test]
    fn text_joins_the_notes_in_send_order() {
        let notes = Notes::default()
            .system_prompt(Some("a".into()))
            .instructions(None)
            .then(Notes::default().instructions(Some("b".into())));
        assert_eq!(notes.text().as_deref(), Some("a\n\nb"));
        assert_eq!(Notes::default().text(), None);
    }
}
