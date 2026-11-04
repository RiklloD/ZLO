//! What a reply sounds like: Markdown turned into text worth hearing.
//!
//! Code blocks and tables are never read out; each becomes one short "see
//! the screen" line. Links read as their text, bare addresses as "a link",
//! and Markdown punctuation (emphasis, headings, bullets, quotes) goes. A
//! long reply stops at a sentence after [`MAX_CHARS`] and says the rest is
//! on screen, so a wall of text never ties up the speaker for minutes.

/// About a minute of speech.
pub(crate) const MAX_CHARS: usize = 1200;

const CODE_NOTE: &str = "See the screen for the code.";
const TABLE_NOTE: &str = "See the screen for the table.";
const REST_NOTE: &str = "The rest is on the screen.";

/// The spoken form of `markdown`; empty when nothing is worth saying.
pub(crate) fn speakable(markdown: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut paragraph = String::new();
    let mut fence: Option<&str> = None;
    let mut in_table = false;
    let flush = |paragraph: &mut String, parts: &mut Vec<String>| {
        let text = paragraph.trim();
        if !text.is_empty() {
            parts.push(sentence(text));
        }
        paragraph.clear();
    };
    let note = |parts: &mut Vec<String>, note: &str| {
        // Two code blocks in a row get one note.
        if parts.last().map(String::as_str) != Some(note) {
            parts.push(note.to_string());
        }
    };
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if let Some(open) = fence {
            if trimmed.starts_with(open) {
                fence = None;
            }
            continue;
        }
        if let Some(marker) = ["```", "~~~"].into_iter().find(|m| trimmed.starts_with(m)) {
            flush(&mut paragraph, &mut parts);
            note(&mut parts, CODE_NOTE);
            fence = Some(marker);
            continue;
        }
        if trimmed.starts_with('|') {
            if !in_table {
                flush(&mut paragraph, &mut parts);
                note(&mut parts, TABLE_NOTE);
                in_table = true;
            }
            continue;
        }
        in_table = false;
        if trimmed.is_empty() {
            flush(&mut paragraph, &mut parts);
            continue;
        }
        // Horizontal rules say nothing.
        if trimmed.len() >= 3 && trimmed.chars().all(|c| matches!(c, '-' | '*' | '_' | ' ')) {
            flush(&mut paragraph, &mut parts);
            continue;
        }
        let (block, text) = strip_block_marker(trimmed);
        if block {
            // Headings and list items each stand as their own sentence.
            flush(&mut paragraph, &mut parts);
            let spoken = inline(text);
            if !spoken.trim().is_empty() {
                parts.push(sentence(spoken.trim()));
            }
            continue;
        }
        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(&inline(text));
    }
    flush(&mut paragraph, &mut parts);
    // A reply that is nothing but notes still says them.
    cap(&parts.join(" "), MAX_CHARS)
}

/// Strip a heading, bullet, numbered-list or quote marker. True when the
/// line was a heading or list item (its own sentence).
fn strip_block_marker(line: &str) -> (bool, &str) {
    let mut text = line;
    while let Some(rest) = text.strip_prefix('>') {
        text = rest.trim_start();
    }
    if let Some(rest) = text.strip_prefix('#') {
        return (true, rest.trim_start_matches('#').trim_start());
    }
    for bullet in ["- [ ] ", "- [x] ", "- ", "* ", "+ "] {
        if let Some(rest) = text.strip_prefix(bullet) {
            return (true, rest);
        }
    }
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = text[digits..]
            .strip_prefix(". ")
            .or_else(|| text[digits..].strip_prefix(") "))
    {
        return (true, rest);
    }
    (false, text)
}

/// Inline Markdown to plain words.
fn inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '`' => {
                let end = chars[i + 1..].iter().position(|&c| c == '`');
                match end {
                    Some(len) => {
                        let code: String = chars[i + 1..i + 1 + len].iter().collect();
                        // Short identifiers read fine; long snippets don't.
                        if code.chars().count() <= 24 {
                            out.push_str(&code);
                        } else {
                            out.push_str("some code");
                        }
                        i += len + 2;
                    }
                    None => i += 1,
                }
            }
            '!' if chars.get(i + 1) == Some(&'[') => i += 1,
            '[' => match link(&chars[i..]) {
                Some((label, used)) => {
                    out.push_str(&label);
                    i += used;
                }
                None => {
                    out.push(c);
                    i += 1;
                }
            },
            '*' | '~' => i += 1,
            '_' if chars.get(i + 1) == Some(&'_') => i += 2,
            '<' if chars[i..].starts_with(&['<', 'h', 't', 't', 'p']) => {
                let len = chars[i..].iter().position(|&c| c == '>').unwrap_or(0);
                out.push_str("a link");
                i += len + 1;
            }
            _ if starts_with_url(&chars[i..]) => {
                let len = chars[i..]
                    .iter()
                    .position(|c| c.is_whitespace())
                    .unwrap_or(chars.len() - i);
                out.push_str("a link");
                // Keep a sentence's closing punctuation.
                if let Some(&last) = chars[i..i + len].last()
                    && matches!(last, '.' | ',' | ';' | ':' | '!' | '?')
                {
                    out.push(last);
                }
                i += len;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// `[label](target)` at the start of `chars`: the label and chars used.
fn link(chars: &[char]) -> Option<(String, usize)> {
    let close = chars.iter().position(|&c| c == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = chars[close + 2..].iter().position(|&c| c == ')')?;
    let label: String = chars[1..close].iter().collect();
    let label = inline(&label);
    let label = if label.trim().is_empty() || starts_with_url(&label.chars().collect::<Vec<_>>()) {
        "a link".to_string()
    } else {
        label
    };
    Some((label, close + 2 + end + 1))
}

fn starts_with_url(chars: &[char]) -> bool {
    let head: String = chars.iter().take(8).collect();
    head.starts_with("https://") || head.starts_with("http://")
}

/// End `text` with sentence punctuation, so voices pause between parts.
fn sentence(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.chars().last() {
        Some('.' | '!' | '?' | ':' | ';') => text,
        Some(_) => format!("{text}."),
        None => text,
    }
}

/// At most `max` chars, cut after the last full sentence that fits.
fn cap(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    let cut = head
        .rfind(['.', '!', '?'])
        .map(|ix| ix + 1)
        .unwrap_or_else(|| head.rfind(' ').unwrap_or(head.len()));
    format!("{} {REST_NOTE}", head[..cut].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_blocks_become_one_note() {
        let md = "Run this:\n\n```rust\nfn main() {}\n```\n```sh\ncargo run\n```\nDone";
        assert_eq!(
            speakable(md),
            "Run this: See the screen for the code. Done."
        );
    }

    #[test]
    fn tilde_fences_and_unclosed_fences_hide_code() {
        assert_eq!(speakable("~~~\nsecret code\n~~~"), CODE_NOTE);
        assert_eq!(speakable("Hi\n```\nlet x = 1;"), format!("Hi. {CODE_NOTE}"));
    }

    #[test]
    fn tables_are_summarized() {
        let md = "Results:\n| a | b |\n|---|---|\n| 1 | 2 |\nThat's all.";
        assert_eq!(
            speakable(md),
            "Results: See the screen for the table. That's all."
        );
    }

    #[test]
    fn markdown_punctuation_is_dropped() {
        let md = "# Summary\n\n**Bold** and *italic* and ~~gone~~ words.\n- first item\n- second\n1. one\n> quoted";
        assert_eq!(
            speakable(md),
            "Summary. Bold and italic and gone words. first item. second. one. quoted."
        );
    }

    #[test]
    fn links_read_as_their_text() {
        assert_eq!(
            speakable("See [the docs](https://x.test/a) or https://y.test/b."),
            "See the docs or a link."
        );
        assert_eq!(speakable("![chart](c.png) here"), "chart here.");
    }

    #[test]
    fn short_inline_code_is_read_long_is_not() {
        assert_eq!(speakable("Use `cargo fmt` now"), "Use cargo fmt now.");
        assert_eq!(
            speakable("Run `cargo clippy --all --benches --tests -- -D warnings` first"),
            "Run some code first."
        );
    }

    #[test]
    fn snake_case_survives() {
        assert_eq!(speakable("the read_aloud flag"), "the read_aloud flag.");
    }

    #[test]
    fn long_replies_stop_at_a_sentence() {
        let md = "This is one sentence. ".repeat(100);
        let spoken = speakable(&md);
        assert!(spoken.chars().count() <= MAX_CHARS + REST_NOTE.len() + 1);
        assert!(spoken.ends_with(REST_NOTE));
        assert!(spoken.contains("sentence. The rest"));
    }

    #[test]
    fn nothing_to_say_is_empty() {
        assert_eq!(speakable(""), "");
        assert_eq!(speakable("\n\n---\n"), "");
    }
}
