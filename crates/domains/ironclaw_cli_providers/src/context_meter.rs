//! What ZLO puts in a CLI agent's context besides the transcript, as one
//! value per turn. Each runtime builds the text it sends from [`Notes`] and
//! records the same notes in its [`SentContexts`], which is what it reports
//! to ZLO's context meter: the meter shows what was sent, not an estimate
//! beside it.

use std::collections::HashMap;
use std::sync::Mutex;

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::context_report::ContextSample;
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

    fn add_to(&self, sample: &mut ContextSample) {
        for (category, note) in &self.0 {
            sample.add_text(*category, note);
        }
    }
}

/// What one thread's agent holds from ZLO after its latest turn.
#[derive(Debug, Clone, Default)]
struct Sent {
    /// Notes the session keeps (a system prompt, a first-turn preamble).
    session: Notes,
    /// Notes only the latest turn carried (requested skills, the goal).
    turn: Notes,
    /// Whether the latest turn's agent got ZLO's loopback MCP server.
    mcp_tools: bool,
}

/// Per thread, what a runtime last sent ([`Notes`] plus the MCP server).
#[derive(Debug, Default)]
pub(crate) struct SentContexts(Mutex<HashMap<ThreadId, Sent>>);

impl SentContexts {
    /// One turn's sends. `session` replaces the thread's kept notes when the
    /// turn sent them (`None`: the session still holds the earlier ones);
    /// `turn` always replaces the per-turn notes.
    pub(crate) fn record(
        &self,
        thread: &ThreadId,
        session: Option<Notes>,
        turn: Notes,
        mcp_tools: bool,
    ) {
        if let Ok(mut sent) = self.0.lock() {
            let entry = sent.entry(thread.clone()).or_default();
            if let Some(session) = session {
                entry.session = session;
            }
            entry.turn = turn;
            entry.mcp_tools = mcp_tools;
        }
    }

    /// Whether the thread's agent got the MCP server after all (known only
    /// once the agent answered its handshake).
    pub(crate) fn set_mcp_tools(&self, thread: &ThreadId, mcp_tools: bool) {
        if let Ok(mut sent) = self.0.lock()
            && let Some(entry) = sent.get_mut(thread)
        {
            entry.mcp_tools = mcp_tools;
        }
    }

    /// The meter's sample of what `thread` was sent. Before any turn of it
    /// ran in this process (a backend restart), `fallback` gives the notes
    /// and MCP flag a turn would send now, from the runtime's own builders.
    pub(crate) fn sample(
        &self,
        thread: &ThreadId,
        fallback: impl FnOnce() -> (Notes, bool),
    ) -> ContextSample {
        let recorded = self.0.lock().ok().and_then(|s| s.get(thread).cloned()); // silent-ok: a poisoned map only follows a panic elsewhere; the fallback is reported
        let (notes, mcp_tools) = match recorded {
            Some(sent) => (sent.session.then(sent.turn), sent.mcp_tools),
            None => fallback(),
        };
        sample_of(&notes, mcp_tools)
    }
}

/// `notes`, plus the orchestrator tools when the agent got the MCP server.
pub(crate) fn sample_of(notes: &Notes, mcp_tools: bool) -> ContextSample {
    let mut sample = ContextSample::default();
    notes.add_to(&mut sample);
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
    sample
}

#[cfg(test)]
mod tests {
    use ironclaw_product_contracts::orchestration::ContextToolOwner;

    use super::*;

    fn thread() -> ThreadId {
        ThreadId::new("t-1").unwrap()
    }

    #[test]
    fn the_sample_is_the_recorded_notes_and_the_mcp_tools_only_when_sent() {
        let sent = SentContexts::default();
        let session = Notes::default()
            .system_prompt(Some("You are careful. ".repeat(40)))
            .instructions(Some("delegate".into()));
        sent.record(&thread(), Some(session.clone()), Notes::default(), false);
        let without = sent.sample(&thread(), || unreachable!("recorded"));
        assert!(without.tools.is_empty(), "no MCP server, no tools counted");
        assert!(without.tokens(ContextCategory::SystemPrompt) > 100);

        // A later turn without session notes keeps the earlier ones.
        sent.record(
            &thread(),
            None,
            Notes::default().instructions(Some("goal".into())),
            true,
        );
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
        sent.set_mcp_tools(&thread(), false);
        assert!(sent.sample(&thread(), || unreachable!()).tools.is_empty());
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
