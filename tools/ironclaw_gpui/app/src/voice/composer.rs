//! Live dictation into a composer.
//!
//! Committed words go in as ordinary text; the volatile tail goes in as
//! marked text through the input's IME path (`replace_and_mark_text_in_range`),
//! so it shows underlined exactly like macOS's own dictation and input
//! methods, and is replaced in place as the recognizer revises it. Commit
//! unmarks the tail (it stays as typed text); discard removes it.

use gpui_kit::{AnyWindowHandle, EntityInputHandler as _};

use super::transcript::{Change, Stabilizer};
use crate::ui::*;

/// A composer, the window it lives in, and the thread it writes to
/// (`None` for the new-thread and quick composers, which create one).
#[derive(Clone)]
pub(crate) struct Target {
    pub input: WeakEntity<TextareaState>,
    pub window: AnyWindowHandle,
    pub thread: Option<String>,
}

impl Target {
    pub fn new(
        input: &Entity<TextareaState>,
        window: AnyWindowHandle,
        thread: Option<String>,
    ) -> Self {
        Self {
            input: input.downgrade(),
            window,
            thread,
        }
    }

    /// The composer still exists (a sent draft or a closed quick composer
    /// drops theirs).
    pub fn alive(&self) -> bool {
        self.input.upgrade().is_some()
    }

    pub fn is(&self, input: &Entity<TextareaState>) -> bool {
        self.input.entity_id() == input.entity_id()
    }

    /// Edit the composer with its window at hand; `None` when either is
    /// gone.
    fn edit<R>(
        &self,
        cx: &mut App,
        f: impl FnOnce(&mut TextareaState, &mut Window, &mut Context<TextareaState>) -> R,
    ) -> Option<R> {
        let input = self.input.upgrade()?;
        self.window
            .update(cx, |_, window, cx| {
                input.update(cx, |state, cx| f(state, window, cx))
            })
            .ok()
    }

    /// Send what the composer holds, the way Enter does: every composer
    /// (chat, new thread, quick) sends on this event.
    pub fn send(&self, cx: &mut App) {
        if let Some(input) = self.input.upgrade() {
            input.update(cx, |_, cx| {
                cx.emit(InputEvent::PressEnter {
                    secondary: false,
                    shift: false,
                })
            });
        }
    }
}

/// One utterance being typed into a [`Target`].
pub(crate) struct Live {
    pub target: Target,
    words: Stabilizer,
    /// The marked tail now in the composer, as inserted.
    shown: String,
    /// The space between earlier composer text and this utterance, fixed
    /// at its first insertion (`None` before it).
    lead: Option<&'static str>,
}

impl Live {
    pub fn new(target: Target) -> Self {
        Self {
            target,
            words: Stabilizer::default(),
            shown: String::new(),
            lead: None,
        }
    }

    pub fn partial(&mut self, text: &str, cx: &mut App) {
        let change = self.words.update(text);
        self.apply(change, cx);
    }

    pub fn finish(&mut self, text: &str, cx: &mut App) {
        let change = self.words.finish(text);
        self.apply(change, cx);
    }

    fn apply(&mut self, change: Change, cx: &mut App) {
        let lead = match self.lead {
            Some(lead) => lead,
            None if change.append.is_empty() && change.volatile.is_empty() => return,
            None => {
                // Space off text already before the cursor.
                let lead = self.target.edit(cx, |state, _, _| {
                    let value = state.value();
                    let before = value.get(..state.cursor().min(value.len())).unwrap_or("");
                    if before.chars().last().is_some_and(|c| !c.is_whitespace()) {
                        " "
                    } else {
                        ""
                    }
                });
                let Some(lead) = lead else { return };
                self.lead = Some(lead);
                lead
            }
        };
        let (append, volatile) = led(lead, &change);
        if append.is_empty() && volatile == self.shown {
            return;
        }
        let had_marked = !self.shown.is_empty();
        let done = self.target.edit(cx, |state, window, cx| {
            // Replace the old marked tail (or insert at the cursor) with the
            // newly committed words; then mark the new tail.
            if had_marked || !append.is_empty() {
                state.replace_text_in_range(None, &append, window, cx);
            }
            if !volatile.is_empty() {
                state.replace_and_mark_text_in_range(None, &volatile, None, window, cx);
            }
        });
        if done.is_some() {
            self.shown = volatile;
        }
    }

    /// Keep the volatile tail as typed text.
    pub fn commit(&mut self, cx: &mut App) {
        if self.shown.is_empty() {
            return;
        }
        let _ = self
            .target
            .edit(cx, |state, window, cx| state.unmark_text(window, cx));
        self.shown.clear();
    }

    /// Remove the volatile tail.
    pub fn discard(&mut self, cx: &mut App) {
        if self.shown.is_empty() {
            return;
        }
        let _ = self.target.edit(cx, |state, window, cx| {
            if state.marked_text_range(window, cx).is_some() {
                state.replace_text_in_range(None, "", window, cx);
            }
        });
        self.shown.clear();
    }
}

/// The change's pieces with `lead` on the one that starts the utterance
/// (the stabilizer leaves only that one without a leading space).
fn led(lead: &str, change: &Change) -> (String, String) {
    let lead_on = |piece: &str| {
        if piece.is_empty() || piece.starts_with(' ') {
            piece.to_string()
        } else {
            format!("{lead}{piece}")
        }
    };
    (lead_on(&change.append), lead_on(&change.volatile))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Simulate the composer edits: committed text replaces the marked
    /// tail, then the new tail is marked.
    fn type_out(before: &str, partials: &[&str], final_text: &str) -> String {
        let lead = if before.chars().last().is_some_and(|c| !c.is_whitespace()) {
            " "
        } else {
            ""
        };
        let mut words = Stabilizer::default();
        let (mut text, mut marked) = (before.to_string(), String::new());
        let mut changes: Vec<Change> = partials.iter().map(|p| words.update(p)).collect();
        changes.push(words.finish(final_text));
        for change in changes {
            let (append, volatile) = led(lead, &change);
            if !marked.is_empty() || !append.is_empty() {
                marked.clear();
                text.push_str(&append);
            }
            marked = volatile;
        }
        text + &marked
    }

    #[test]
    fn utterances_space_off_existing_text_once() {
        assert_eq!(
            type_out("Hello", &["world", "world there"], "world there"),
            "Hello world there"
        );
        assert_eq!(type_out("Hello ", &["world"], "world"), "Hello world");
        assert_eq!(
            type_out("", &["one", "one two"], "one two three"),
            "one two three"
        );
    }

    #[test]
    fn a_volatile_first_word_keeps_its_space_when_committed() {
        // The first piece shown is volatile; committing it must not drop
        // the space before it.
        assert_eq!(
            type_out(
                "Hi",
                &["there", "there you", "there you go"],
                "there you go"
            ),
            "Hi there you go"
        );
    }
}
