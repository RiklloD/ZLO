//! Live detail a CLI reports beside its reply and tool rows: the tail of a
//! running command's output, the turn's aggregated diff, and a finished
//! review. Everything here is live-only and bounded; the durable record
//! stays the CLI's transcript. Every body is redacted for display before
//! it leaves the bridge (commands print tokens, diffs and reviews can quote
//! them).

use std::time::{Duration, Instant};

use ironclaw_host_api::ids::ThreadId;

use super::{CliBridge, LiveEvent, ToolState, ToolView};

/// Characters of a command's output a row keeps: the newest end.
pub(super) const OUTPUT_TAIL_MAX_CHARS: usize = 4_000;
/// A turn diff past this many bytes is cut (the projection allows 128 KiB).
pub(super) const TURN_DIFF_MAX_BYTES: usize = 120 * 1024;
/// A review past this many bytes is cut.
const REVIEW_MAX_BYTES: usize = 64 * 1024;
/// Output and diff bodies are re-sent whole, so at most this often each.
const DETAIL_EMIT_EVERY: Duration = Duration::from_millis(250);

/// A bounded body that is broadcast whole, throttled.
#[derive(Debug, Default)]
pub(super) struct Detail {
    pub(super) body: String,
    pub(super) truncated: bool,
    sent_at: Option<Instant>,
    unsent: bool,
}

impl Detail {
    /// The body to send now: due, or `force`d, and changed since the last.
    fn take(&mut self, force: bool) -> Option<(String, bool)> {
        if !self.unsent {
            return None;
        }
        if !force
            && self
                .sent_at
                .is_some_and(|at| at.elapsed() < DETAIL_EMIT_EVERY)
        {
            return None;
        }
        self.unsent = false;
        self.sent_at = Some(Instant::now());
        Some((
            ironclaw_safety::sanitize_display_text(&self.body),
            self.truncated,
        ))
    }

    /// Append output, keeping the newest [`OUTPUT_TAIL_MAX_CHARS`] from a
    /// line start where one is near.
    fn push_tail(&mut self, delta: &str) {
        self.body.push_str(delta);
        self.unsent = true;
        let chars = self.body.chars().count();
        if chars <= OUTPUT_TAIL_MAX_CHARS {
            return;
        }
        let Some((cut, _)) = self.body.char_indices().nth(chars - OUTPUT_TAIL_MAX_CHARS) else {
            return;
        };
        let cut = self.body[cut..]
            .find('\n')
            .filter(|nl| *nl < 200)
            .map_or(cut, |nl| cut + nl + 1);
        self.body.replace_range(..cut, "");
        self.truncated = true;
    }

    /// Replace the body with `text`, cut at a line end within `max` bytes.
    fn replace_bounded(&mut self, text: &str, max: usize) {
        let (body, truncated) = bounded_prefix(text, max);
        if body == self.body && truncated == self.truncated {
            return;
        }
        self.body = body.to_string();
        self.truncated = truncated;
        self.unsent = true;
    }
}

/// The longest prefix of `text` within `max` bytes, ending at a line end
/// when there is one; and whether anything was cut.
pub(super) fn bounded_prefix(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut at = max;
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let head = &text[..at];
    let end = head.rfind('\n').map_or(at, |nl| nl + 1);
    (&text[..end], true)
}

impl CliBridge {
    /// A tool call the CLI reported itself (not through an approval):
    /// its row shows under the segment that led to it. A call the person
    /// just approved already has a row with no CLI id; the first matching
    /// one adopts `call_id` instead of drawing a second row.
    pub fn tool_observed(
        &self,
        thread: &ThreadId,
        run_id: &str,
        tool_name: &str,
        subject: Option<&str>,
        call_id: &str,
    ) {
        let (tool, unsent) = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            let Some(turn) = live.get_mut(thread).filter(|t| t.run_id == run_id) else {
                return;
            };
            if turn
                .tools
                .iter()
                .any(|(id, _)| id.as_deref() == Some(call_id))
            {
                return;
            }
            if let Some((id, _)) = turn.tools.iter_mut().find(|(id, t)| {
                id.is_none() && t.tool_name == tool_name && t.state == ToolState::Running
            }) {
                *id = Some(call_id.to_string());
                return;
            }
            let tool = ToolView {
                invocation: uuid::Uuid::new_v4().to_string(),
                run_id: run_id.to_string(),
                tool_name: tool_name.to_string(),
                subject: subject.map(|s| {
                    super::truncate_chars(
                        &ironclaw_safety::sanitize_display_text(s),
                        super::TOOL_SUBJECT_MAX_CHARS,
                    )
                }),
                state: ToolState::Running,
            };
            let unsent = turn.cut_segment();
            turn.tools.push((Some(call_id.to_string()), tool.clone()));
            (tool, unsent)
        };
        self.emit_text(thread, run_id, unsent);
        self.emit_tools(thread, vec![tool]);
    }

    /// Output the CLI streamed for call `call_id`. The row's tail is
    /// broadcast at most every [`DETAIL_EMIT_EVERY`]; [`Self::flush_details`]
    /// and the call's end send what is left.
    pub fn tool_output(&self, thread: &ThreadId, run_id: &str, call_id: &str, delta: &str) {
        let due = self.live.lock().ok().and_then(|mut live| {
            let turn = live.get_mut(thread).filter(|t| t.run_id == run_id)?;
            let invocation = turn
                .tools
                .iter()
                .find(|(id, _)| id.as_deref() == Some(call_id))
                .map(|(_, t)| t.invocation.clone())?;
            let detail = turn.outputs.entry(invocation.clone()).or_default();
            detail.push_tail(delta);
            detail.take(false).map(|body| (invocation, body))
        });
        if let Some((invocation, (tail, truncated))) = due {
            self.emit(LiveEvent::ToolOutput {
                thread: thread.clone(),
                run_id: run_id.to_string(),
                invocation,
                tail,
                truncated,
            });
        }
    }

    /// The turn's aggregated unified diff so far (whole, not a delta).
    pub fn turn_diff(&self, thread: &ThreadId, run_id: &str, diff: &str) {
        let due = self.live.lock().ok().and_then(|mut live| {
            let turn = live.get_mut(thread).filter(|t| t.run_id == run_id)?;
            turn.diff.replace_bounded(diff, TURN_DIFF_MAX_BYTES);
            turn.diff.take(false)
        });
        if let Some((diff, truncated)) = due {
            self.emit(LiveEvent::TurnDiff {
                thread: thread.clone(),
                run_id: run_id.to_string(),
                diff,
                truncated,
            });
        }
    }

    /// A finished review's findings text, bounded and display-redacted
    /// like every other live body. Drivers pass locations relative to the
    /// workspace (`src/a.rs:3-4`), which redaction keeps; an absolute path
    /// outside it is redacted with the rest.
    pub fn review(&self, thread: &ThreadId, run_id: &str, text: &str) {
        let (text, _) = bounded_prefix(text, REVIEW_MAX_BYTES);
        let text = ironclaw_safety::sanitize_display_text(text);
        let stored = self.live.lock().ok().is_some_and(|mut live| {
            live.get_mut(thread)
                .filter(|t| t.run_id == run_id)
                .map(|t| t.review = Some(text.clone()))
                .is_some()
        });
        if stored {
            self.emit(LiveEvent::Review {
                thread: thread.clone(),
                run_id: run_id.to_string(),
                text,
            });
        }
    }

    /// Send every output tail and the diff that a throttle held back.
    /// Drivers call it on a timer, so a quiet command's last lines show.
    pub fn flush_details(&self, thread: &ThreadId, run_id: &str) {
        let events = self
            .live
            .lock()
            .ok()
            .and_then(|mut live| {
                let turn = live.get_mut(thread).filter(|t| t.run_id == run_id)?;
                Some(turn.take_details(thread, true))
            })
            .unwrap_or_default(); // silent-ok: a poisoned map only follows a panic elsewhere
        for event in events {
            self.emit(event);
        }
    }

    /// The call `invocation` ended: its last output goes out now.
    pub(super) fn flush_output(&self, thread: &ThreadId, run_id: &str, invocation: &str) {
        let due = self.live.lock().ok().and_then(|mut live| {
            let turn = live.get_mut(thread).filter(|t| t.run_id == run_id)?;
            turn.outputs.get_mut(invocation)?.take(true)
        });
        if let Some((tail, truncated)) = due {
            self.emit(LiveEvent::ToolOutput {
                thread: thread.clone(),
                run_id: run_id.to_string(),
                invocation: invocation.to_string(),
                tail,
                truncated,
            });
        }
    }
}

impl super::LiveTurn {
    /// Unsent detail bodies as events (`force` ignores the throttle).
    pub(super) fn take_details(&mut self, thread: &ThreadId, force: bool) -> Vec<LiveEvent> {
        let mut events = Vec::new();
        for (invocation, detail) in &mut self.outputs {
            if let Some((tail, truncated)) = detail.take(force) {
                events.push(LiveEvent::ToolOutput {
                    thread: thread.clone(),
                    run_id: self.run_id.clone(),
                    invocation: invocation.clone(),
                    tail,
                    truncated,
                });
            }
        }
        if let Some((diff, truncated)) = self.diff.take(force) {
            events.push(LiveEvent::TurnDiff {
                thread: thread.clone(),
                run_id: self.run_id.clone(),
                diff,
                truncated,
            });
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_tail_keeps_the_newest_end_from_a_line_start() {
        let mut d = Detail::default();
        for i in 0..2_000 {
            d.push_tail(&format!("line {i}\n"));
        }
        assert!(d.truncated);
        assert!(d.body.chars().count() <= OUTPUT_TAIL_MAX_CHARS);
        assert!(d.body.starts_with("line "), "{}", &d.body[..20]);
        assert!(d.body.ends_with("line 1999\n"));
    }

    #[test]
    fn bounded_prefix_cuts_at_a_line_end_and_a_char_boundary() {
        assert_eq!(bounded_prefix("abc", 10), ("abc", false));
        assert_eq!(bounded_prefix("ab\ncd\nef", 7), ("ab\ncd\n", true));
        // `é` is two bytes: a cut inside it backs off to the boundary.
        assert_eq!(bounded_prefix("éé", 3), ("é", true));
    }

    /// A finding quoting a hardcoded key reaches the stream redacted, its
    /// workspace-relative location intact.
    #[tokio::test]
    async fn a_review_is_redacted_but_keeps_relative_locations() {
        let bridge = CliBridge::new(super::super::DEFAULT_APPROVAL_TIMEOUT);
        let thread = ThreadId::new("t").unwrap();
        bridge.turn_started(&thread, "r");
        let mut rx = bridge.subscribe();
        bridge.review(
            &thread,
            "r",
            "Review comment:\n\n- [P1] Hardcoded key — src/a.rs:3-4\n  \
             let key = \"sk-proj-abcdefghijklmnopqrstuvwxyz0123456789\";",
        );
        let LiveEvent::Review { text, .. } = rx.recv().await.unwrap() else {
            panic!("not a review")
        };
        assert!(!text.contains("sk-proj-abcdefghijklmnop"), "{text}");
        assert!(text.contains("src/a.rs:3-4"), "{text}");
        assert_eq!(bridge.snapshot(&thread).unwrap().review, Some(text));
    }

    #[test]
    fn detail_bodies_are_redacted_before_they_leave() {
        let mut d = Detail::default();
        d.push_tail("export OPENAI_API_KEY=sk-proj-abcdefghijklmnopqrstuvwxyz0123456789\n");
        let (body, _) = d.take(true).unwrap();
        assert!(!body.contains("sk-proj-abcdefghijklmnop"), "{body}");
        assert!(d.take(true).is_none(), "nothing new to send");
    }
}
