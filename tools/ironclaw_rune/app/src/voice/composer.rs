//! Live dictation into a composer.
//!
//! Committed words go in as ordinary text at the caret; the volatile tail
//! goes in as the field's IME composition (`TextEdit::preedit`), so it shows
//! exactly like macOS's own dictation and input methods, and is replaced
//! in place as the recognizer revises it. Commit inserts the tail as typed
//! text; discard drops it.
//!
//! Rune has one state tree, so a target names its composer ([`Composer`]) and finds the
//! `TextEdit` again through the composer owners
//! (`crate::chat::voice_composer{,_mut}`) every time. The quick composer's
//! field lives in its own window; an edit there goes to its mirror in the main
//! state and, as the same edit, to the window (`workspace::quick::voice_edit`).

use rune::{Ime, TextEdit, Tx};

use super::transcript::{Change, Stabilizer};
use crate::state::State;

/// Which composer voice types into. The owners resolve it to their
/// `TextEdit`: area A the open chat's, area B the new-thread and quick
/// composers'.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Composer {
    /// The composer of the open thread with this id.
    Chat(String),
    /// The new-thread screen.
    Draft,
    /// The quick composer window.
    Quick,
}

/// A composer and the thread it writes to (`None` for the new-thread and
/// quick composers, which create one).
#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub composer: Composer,
    pub thread: Option<String>,
}

impl Target {
    pub fn new(composer: Composer, thread: Option<String>) -> Self {
        Self { composer, thread }
    }

    /// The composer still exists (a sent draft or a closed quick composer
    /// drops theirs).
    pub fn alive(&self, s: &State) -> bool {
        crate::chat::voice_composer(s, &self.composer).is_some()
    }

    /// The composer is drawn where the person can see it: the main window
    /// showing its chat (not a settings page over it), the right pane open
    /// on its side chat, the new-thread screen, or the quick composer's
    /// window. Window focus does not matter: push-to-talk is used from
    /// other apps. A composer kept alive under another page is not shown.
    pub fn shown(&self, s: &State) -> bool {
        use crate::state::Page;
        if !self.alive(s) {
            return false;
        }
        match &self.composer {
            Composer::Chat(id) => {
                s.page == Page::Chat
                    && s.chat.as_ref().is_some_and(|c| {
                        c.thread_id == *id || crate::chat::side::side_chat_shown(s)
                    })
            }
            Composer::Draft => s.page == Page::Draft,
            Composer::Quick => s.workspace.quick.is_open(),
        }
    }

    /// Edit the composer's field; `None` when it is gone. `f` may run
    /// twice (the quick composer's mirror and its window), so it is `Fn`.
    fn edit<R>(
        &self,
        s: &mut State,
        f: impl Fn(&mut TextEdit) -> R + Clone + Send + 'static,
    ) -> Option<R> {
        if self.composer == Composer::Quick {
            return crate::workspace::quick::voice_edit(s, f);
        }
        crate::chat::voice_composer_mut(s, &self.composer).map(f)
    }

    /// Send what the composer holds, the way Enter does.
    pub fn send(&self, s: &mut State, tx: &mut Tx) {
        crate::chat::voice_press_enter(s, &self.composer, tx);
    }
}

/// One utterance being typed into a [`Target`].
pub(crate) struct Live {
    pub target: Target,
    words: Stabilizer,
    /// The preedit tail now in the composer, as set.
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

    pub fn partial(&mut self, text: &str, s: &mut State) {
        let change = self.words.update(text);
        self.apply(change, s);
    }

    pub fn finish(&mut self, text: &str, s: &mut State) {
        let change = self.words.finish(text);
        self.apply(change, s);
    }

    fn apply(&mut self, change: Change, s: &mut State) {
        let lead = match self.lead {
            Some(lead) => lead,
            None if change.append.is_empty() && change.volatile.is_empty() => return,
            None => {
                // Space off text already before the caret.
                let lead = self.target.edit(s, |edit| {
                    let caret = edit.range().0.min(edit.text.len());
                    let before = edit.text.get(..caret).unwrap_or("");
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
        let tail = volatile.clone();
        let shown = self.shown.clone();
        let done = self.target.edit(s, move |edit| {
            // Drop the old tail and insert the newly committed words at the
            // caret; then show the new tail as the preedit.
            show_change(edit, &shown, &append, &tail);
        });
        if done.is_some() {
            self.shown = volatile;
        }
    }

    /// Keep the volatile tail as typed text.
    pub fn commit(&mut self, s: &mut State) {
        if self.shown.is_empty() {
            return;
        }
        let _ = self.target.edit(s, |edit| {
            // What the field still shows (typing may have ended the preedit)
            // becomes typed text.
            edit.finish_composition();
        });
        self.shown.clear();
    }

    /// Remove the volatile tail.
    pub fn discard(&mut self, s: &mut State) {
        if self.shown.is_empty() {
            return;
        }
        let shown = self.shown.clone();
        let _ = self.target.edit(s, move |edit| {
            take_back_finished(edit, &shown);
            edit.cancel_composition();
        });
        self.shown.clear();
    }
}

/// Drops the old volatile tail, inserts `append` at the caret and shows
/// `volatile` as the field's preedit (an input-method composition, so it is
/// drawn but not yet text).
fn show_change(edit: &mut TextEdit, shown: &str, append: &str, volatile: &str) {
    take_back_finished(edit, shown);
    edit.cancel_composition();
    if !append.is_empty() {
        edit.insert(append);
    }
    if !volatile.is_empty() {
        let end = volatile.len();
        edit.ime(&Ime::Preedit(
            volatile.to_owned(),
            Some((end, end)),
            Vec::new(),
        ));
    }
}

/// When the field finished the tail's composition on its own (Rune writes a
/// composition into the text when the window or field loses focus), the
/// words shown as volatile are now ordinary text just before the caret, and
/// the recognizer will send them again. Take that copy back out so it is not
/// typed twice. Text the user has typed since is left alone.
fn take_back_finished(edit: &mut TextEdit, shown: &str) {
    if shown.is_empty() || edit.is_composing() {
        return;
    }
    let (start, end) = edit.range();
    if start != end {
        return;
    }
    if let Some(from) = end.checked_sub(shown.len())
        && edit.text.get(from..end) == Some(shown)
    {
        edit.set_selection(from, end);
        edit.insert("");
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

    /// Simulate the composer edits on a real `TextEdit`: committed text is
    /// inserted at the caret, the tail is the preedit.
    fn type_out(before: &str, partials: &[&str], final_text: &str) -> String {
        let lead = if before.chars().last().is_some_and(|c| !c.is_whitespace()) {
            " "
        } else {
            ""
        };
        let mut edit = TextEdit::new(before);
        edit.set_caret(before.len());
        let mut words = Stabilizer::default();
        let mut changes: Vec<Change> = partials.iter().map(|p| words.update(p)).collect();
        changes.push(words.finish(final_text));
        let mut shown = String::new();
        for change in changes {
            let (append, volatile) = led(lead, &change);
            show_change(&mut edit, &shown, &append, &volatile);
            shown = volatile;
        }
        edit.finish_composition();
        edit.text.clone()
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
    fn a_tail_finished_by_focus_loss_is_not_typed_twice() {
        let mut edit = TextEdit::new("Hi");
        edit.set_caret(2);
        let mut words = Stabilizer::default();
        let (append, volatile) = led(" ", &words.update("hello"));
        show_change(&mut edit, "", &append, &volatile);
        // The window loses focus: Rune writes the composition into the text.
        edit.finish_composition();
        assert_eq!(edit.text, format!("Hi{volatile}"));
        let (append2, volatile2) = led(" ", &words.finish("hello"));
        show_change(&mut edit, &volatile, &append2, &volatile2);
        edit.finish_composition();
        assert_eq!(edit.text, "Hi hello");
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
