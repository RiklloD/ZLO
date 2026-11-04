//! Live transcripts: which words are settled and which may still change.
//!
//! Apple's recognizer re-sends the whole utterance on every partial result
//! and may rewrite its last words as it hears more. A word counts as
//! committed once two partials in a row agree on it and something follows
//! it; the rest is volatile. Committed text only ever grows, so the
//! composer can append it as plain text and keep just the volatile tail as
//! marked (underlined) text, the way macOS dictation shows it.

/// What changed in the composer for one recognizer result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Change {
    /// Newly committed text to append (with its leading space).
    pub append: String,
    /// The whole volatile tail now (with its leading space).
    pub volatile: String,
}

#[derive(Default)]
pub(crate) struct Stabilizer {
    committed: Vec<String>,
    last: Vec<String>,
}

impl Stabilizer {
    /// Feed the recognizer's latest full partial transcript.
    pub fn update(&mut self, partial: &str) -> Change {
        let words = split(partial);
        // Settled: agreed by both partials, and not the newest word.
        let agreed = self
            .last
            .iter()
            .zip(&words)
            .take_while(|(a, b)| a == b)
            .count()
            .min(words.len().saturating_sub(1));
        let before = self.committed.len();
        let mut append: &[String] = &[];
        if agreed > before && words.starts_with(&self.committed) {
            append = &words[before..agreed];
        }
        self.committed.extend(append.iter().cloned());
        let volatile = words.get(self.committed.len()..).unwrap_or_default();
        let change = change(before, append, volatile);
        self.last = words;
        change
    }

    /// The recognizer's final transcript: everything left is committed.
    pub fn finish(&mut self, final_text: &str) -> Change {
        let words = split(final_text);
        let before = self.committed.len();
        let rest = words.get(before..).unwrap_or_default();
        let change = change(before, rest, &[]);
        self.committed.extend(rest.iter().cloned());
        self.last = words;
        change
    }
}

/// The composer edit for `append` after `before` committed words, with
/// `volatile` following it.
fn change(before: usize, append: &[String], volatile: &[String]) -> Change {
    let lead = |words_before: usize| if words_before > 0 { " " } else { "" };
    let mut out = Change::default();
    if !append.is_empty() {
        out.append = format!("{}{}", lead(before), append.join(" "));
    }
    if !volatile.is_empty() {
        out.volatile = format!("{}{}", lead(before + append.len()), volatile.join(" "));
    }
    out
}

fn split(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(s: &mut Stabilizer, partials: &[&str]) -> Vec<Change> {
        partials.iter().map(|p| s.update(p)).collect()
    }

    /// Replaying the changes the way the composer does gives the transcript.
    fn replay(changes: &[Change]) -> String {
        let mut committed = String::new();
        let mut volatile = String::new();
        for c in changes {
            committed.push_str(&c.append);
            volatile = c.volatile.clone();
        }
        committed + &volatile
    }

    #[test]
    fn words_settle_once_two_partials_agree() {
        let mut s = Stabilizer::default();
        let c = feed(&mut s, &["Hello", "Hello there", "Hello there how"]);
        assert_eq!(
            c[0],
            Change {
                append: "".into(),
                volatile: "Hello".into()
            }
        );
        // "Hello" agreed by both and followed by "there".
        assert_eq!(
            c[1],
            Change {
                append: "Hello".into(),
                volatile: " there".into()
            }
        );
        assert_eq!(
            c[2],
            Change {
                append: " there".into(),
                volatile: " how".into()
            }
        );
    }

    #[test]
    fn the_newest_word_stays_volatile_even_when_repeated() {
        let mut s = Stabilizer::default();
        let c = feed(&mut s, &["Hel", "Hel"]);
        assert_eq!(c[1].append, "");
        assert_eq!(c[1].volatile, "Hel");
    }

    #[test]
    fn revisions_rewrite_only_the_volatile_tail() {
        let mut s = Stabilizer::default();
        let c = feed(
            &mut s,
            &[
                "write a",
                "write a test",
                "write a tester for",
                "write a tester for me",
            ],
        );
        // "test" was never agreed, so its rewrite to "tester" costs nothing.
        assert_eq!(replay(&c), "write a tester for me");
        assert_eq!(c[1].append, "write a");
        assert_eq!(c[2].append, "");
        assert_eq!(c[2].volatile, " tester for");
        assert_eq!(c[3].append, " tester for");
    }

    #[test]
    fn committed_text_never_shrinks() {
        let mut s = Stabilizer::default();
        let mut c = feed(&mut s, &["one two three", "one two three four"]);
        // The recognizer rewrites a committed word: committed stays, and the
        // tail is whatever follows the committed count.
        c.push(s.update("won two three four five"));
        assert!(c.iter().all(|c| !c.append.contains("won")));
        assert_eq!(replay(&c), "one two three four five");
    }

    #[test]
    fn finish_commits_the_rest() {
        let mut s = Stabilizer::default();
        let mut c = feed(&mut s, &["send it", "send it now"]);
        c.push(s.finish("send it now please"));
        assert_eq!(c[2].append, " now please");
        assert_eq!(c[2].volatile, "");
        assert_eq!(replay(&c), "send it now please");
    }

    #[test]
    fn finish_without_partials_commits_everything() {
        let mut s = Stabilizer::default();
        let c = s.finish("just this");
        assert_eq!(c.append, "just this");
        assert_eq!(c.volatile, "");
    }

    #[test]
    fn empty_partials_hear_nothing() {
        let mut s = Stabilizer::default();
        assert_eq!(s.update("  "), Change::default());
    }
}
