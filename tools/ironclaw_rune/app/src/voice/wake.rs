//! Wake phrase matching on a live transcript.
//!
//! Recognizers spell a made-up name many ways ("Hey ZLO", "hey zlo,",
//! "Hey Zelo"), so the phrase and the transcript are compared with
//! punctuation, case and spaces gone, allowing one wrong letter in phrases
//! of five letters or more. The last match wins: whatever follows it is the
//! command.

/// Letters and digits only, lowercased, per word.
fn words(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|w| {
            w.chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// Whether `phrase` has anything to listen for.
pub(crate) fn valid(phrase: &str) -> bool {
    words(phrase).concat().chars().count() >= 3
}

/// The word count of `transcript` up to the end of the last wake phrase,
/// or `None` when the phrase is not in it.
pub(crate) fn find(phrase: &str, transcript: &str) -> Option<usize> {
    let target = words(phrase).concat();
    let target_len = target.chars().count();
    if target_len < 3 {
        return None;
    }
    let tolerance = usize::from(target_len >= 5);
    let heard = words(transcript);
    let span = words(phrase).len();
    let mut found = None;
    for start in 0..heard.len() {
        // The recognizer may split or join words ("z lo", "heyzlo").
        for len in span.saturating_sub(1).max(1)..=span + 1 {
            let Some(window) = heard.get(start..start + len) else {
                break;
            };
            let joined = window.concat();
            if distance(&joined, &target) <= tolerance {
                found = Some(start + len);
            }
        }
    }
    found
}

/// The command spoken after the last wake phrase, as transcribed (with its
/// original spelling and punctuation).
pub(crate) fn command_after(phrase: &str, transcript: &str) -> Option<String> {
    let end = find(phrase, transcript)?;
    // `find` counts words that have letters; map back to raw words.
    let mut seen = 0;
    let mut out = Vec::new();
    for raw in transcript.split_whitespace() {
        if seen >= end {
            out.push(raw);
        } else if raw.chars().any(char::is_alphanumeric) {
            seen += 1;
        }
    }
    let command = out.join(" ");
    let command = command.trim_start_matches([',', '.', '!', '?', ':', ';', '-', ' ']);
    Some(command.to_string())
}

/// Levenshtein distance over chars.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            let next = (row[j + 1] + 1).min(row[j] + 1).min(prev + cost);
            prev = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_spellings_the_recognizer_produces() {
        for heard in [
            "Hey ZLO",
            "hey zlo,",
            "Hey, Zlo!",
            "hey zelo",
            "Heyzlo",
            "hey z lo",
        ] {
            assert!(find("Hey ZLO", heard).is_some(), "{heard}");
        }
    }

    #[test]
    fn ignores_other_words() {
        for heard in ["hey zero", "hello", "the slow fox", ""] {
            assert!(find("Hey ZLO", heard).is_none(), "{heard}");
        }
    }

    #[test]
    fn command_is_what_follows_the_last_phrase() {
        assert_eq!(
            command_after("Hey ZLO", "um hey ZLO, what's the time?").as_deref(),
            Some("what's the time?")
        );
        assert_eq!(command_after("Hey ZLO", "Hey ZLO").as_deref(), Some(""));
        assert_eq!(
            command_after("Hey ZLO", "hey zlo stop hey zlo open mail").as_deref(),
            Some("open mail")
        );
        assert_eq!(command_after("Hey ZLO", "nothing here"), None);
    }

    #[test]
    fn short_phrases_need_an_exact_match() {
        assert!(find("zlo", "zlo").is_some());
        assert!(find("zlo", "zla").is_none());
        assert!(!valid("hi"));
        assert!(valid("Hey ZLO"));
    }

    #[test]
    fn distance_counts_edits() {
        assert_eq!(distance("kitten", "sitting"), 3);
        assert_eq!(distance("", "abc"), 3);
        assert_eq!(distance("same", "same"), 0);
    }
}
