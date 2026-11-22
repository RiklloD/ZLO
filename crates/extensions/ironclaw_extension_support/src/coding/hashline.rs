//! Compact line-hash anchors ("hashlines") shared by `read_file` and
//! `apply_patch`.
//!
//! `read_file` with `hashline: true` prefixes every rendered line with
//! `LINE:HASH`, where HASH is four lowercase hex digits derived from that
//! line's exact text. `apply_patch` accepts edits anchored on those pairs and
//! re-verifies every anchored line against the file as it is now, so an edit
//! computed from a stale view is rejected (with fresh anchors) instead of
//! landing on the wrong lines. The hash is a freshness check on a line whose
//! position is already named, not a lookup key, so 16 bits is enough: a
//! changed line slips through only when the new text collides with the old.
//!
//! Both tools split decoded text the same way (`decode_text*` normalize line
//! endings to `\n`; a line is the text between `\n`s), so a hash rendered by
//! `read_file` is the hash `apply_patch` recomputes for the same line.

use std::fmt;

/// Four-hex-digit hash of one line's exact text (line terminator excluded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LineHash(u16);

impl LineHash {
    pub(super) fn of(line: &str) -> Self {
        // FNV-1a (32-bit), xor-folded to 16 bits: deterministic across
        // processes and releases, which a randomized std hasher is not.
        let mut hash: u32 = 0x811c_9dc5;
        for byte in line.as_bytes() {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
        Self(((hash >> 16) ^ (hash & 0xffff)) as u16)
    }

    fn parse(raw: &str) -> Option<Self> {
        if raw.len() != 4 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        u16::from_str_radix(raw, 16).ok().map(Self)
    }
}

impl fmt::Display for LineHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}", self.0)
    }
}

/// A `LINE:HASH` anchor naming one 1-based line and the hash it had when read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LineAnchor {
    pub(super) line: usize,
    pub(super) hash: LineHash,
}

impl LineAnchor {
    /// Parse `LINE:HASH` (surrounding whitespace tolerated, hex case-insensitive).
    pub(super) fn parse(raw: &str) -> Option<Self> {
        let (line, hash) = raw.trim().split_once(':')?;
        let line = line.trim().parse::<usize>().ok().filter(|line| *line > 0)?;
        let hash = LineHash::parse(hash.trim())?;
        Some(Self { line, hash })
    }
}

impl fmt::Display for LineAnchor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.hash)
    }
}

/// Replace the whole lines `start..=end` with `new_string`'s lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AnchoredEdit {
    pub(super) start: LineAnchor,
    pub(super) end: LineAnchor,
    pub(super) new_string: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AnchoredEditError {
    /// `end` names a line before `start`.
    Inverted { edit_index: usize },
    /// An anchor names a line past the end of the file.
    OutOfRange {
        edit_index: usize,
        anchor: LineAnchor,
        total_lines: usize,
    },
    /// The anchored line's current hash differs from the anchor's.
    Stale {
        edit_index: usize,
        anchor: LineAnchor,
        current: LineHash,
    },
    /// Two edits touch the same line.
    Overlap {
        previous_edit_index: usize,
        current_edit_index: usize,
    },
    /// The edits leave the file byte-for-byte unchanged.
    NoChange,
}

/// `content` split into lines, each with its byte span *including* its `\n`.
struct LineSpans<'a> {
    content: &'a str,
    spans: Vec<(usize, usize)>,
}

impl<'a> LineSpans<'a> {
    fn new(content: &'a str) -> Self {
        let mut spans = Vec::new();
        let mut start = 0usize;
        for segment in content.split_inclusive('\n') {
            spans.push((start, start + segment.len()));
            start += segment.len();
        }
        Self { content, spans }
    }

    fn len(&self) -> usize {
        self.spans.len()
    }

    /// Text of 1-based `line` without its terminator.
    fn text(&self, line: usize) -> Option<&'a str> {
        let (start, end) = *self.spans.get(line.checked_sub(1)?)?;
        let segment = self.content.get(start..end)?;
        Some(segment.strip_suffix('\n').unwrap_or(segment))
    }
}

/// Apply `edits` (all verified against `content` first; nothing is applied
/// unless every anchor is fresh and no two edits overlap).
pub(super) fn apply_anchored_edits(
    content: &str,
    edits: &[AnchoredEdit],
) -> Result<String, AnchoredEditError> {
    let lines = LineSpans::new(content);
    let mut ordered: Vec<(usize, &AnchoredEdit)> = edits.iter().enumerate().collect();
    for (edit_index, edit) in &ordered {
        if edit.end.line < edit.start.line {
            return Err(AnchoredEditError::Inverted {
                edit_index: *edit_index,
            });
        }
        for anchor in [edit.start, edit.end] {
            let Some(text) = lines.text(anchor.line) else {
                return Err(AnchoredEditError::OutOfRange {
                    edit_index: *edit_index,
                    anchor,
                    total_lines: lines.len(),
                });
            };
            let current = LineHash::of(text);
            if current != anchor.hash {
                return Err(AnchoredEditError::Stale {
                    edit_index: *edit_index,
                    anchor,
                    current,
                });
            }
        }
    }
    ordered.sort_by_key(|(_, edit)| edit.start.line);
    for pair in ordered.windows(2) {
        let [
            (previous_edit_index, previous),
            (current_edit_index, current),
        ] = pair
        else {
            continue;
        };
        if current.start.line <= previous.end.line {
            return Err(AnchoredEditError::Overlap {
                previous_edit_index: (*previous_edit_index).min(*current_edit_index),
                current_edit_index: (*previous_edit_index).max(*current_edit_index),
            });
        }
    }

    let mut output = String::with_capacity(content.len());
    let mut cursor = 0usize;
    for (_, edit) in &ordered {
        // Both anchors were range-checked above, so these indexes exist; the
        // fallbacks keep the function total without a panic path.
        let range_start = lines
            .spans
            .get(edit.start.line - 1)
            .map_or(content.len(), |span| span.0);
        let range_end = lines
            .spans
            .get(edit.end.line - 1)
            .map_or(content.len(), |span| span.1);
        output.push_str(content.get(cursor..range_start).unwrap_or_default());
        let replaced_had_terminator = content
            .get(range_start..range_end)
            .is_some_and(|replaced| replaced.ends_with('\n'));
        let mut replacement = edit.new_string.replace("\r\n", "\n").replace('\r', "\n");
        // The replacement takes the replaced range's terminator state, so a
        // replaced middle line stays a line and a replaced final line keeps
        // the file's (lack of) trailing newline.
        if !replacement.is_empty() {
            if replaced_had_terminator && !replacement.ends_with('\n') {
                replacement.push('\n');
            } else if !replaced_had_terminator && replacement.ends_with('\n') {
                replacement.pop();
            }
        }
        output.push_str(&replacement);
        cursor = range_end;
    }
    output.push_str(content.get(cursor..).unwrap_or_default());
    if output == content {
        return Err(AnchoredEditError::NoChange);
    }
    Ok(output)
}

/// Fresh `LINE:HASH` anchors for the lines around `line` (two either side),
/// for a stale-anchor diagnostic. Line text is not included: the summary
/// channel is a single bounded line, and the anchors are what the model needs
/// to retry.
pub(super) fn fresh_anchors_around(content: &str, line: usize) -> String {
    let lines = LineSpans::new(content);
    let first = line.saturating_sub(2).max(1);
    let last = line.saturating_add(2).min(lines.len());
    (first..=last)
        .filter_map(|number| {
            lines.text(number).map(|text| {
                LineAnchor {
                    line: number,
                    hash: LineHash::of(text),
                }
                .to_string()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchor(content: &str, line: usize) -> LineAnchor {
        let text = content.split('\n').nth(line - 1).expect("line exists");
        LineAnchor {
            line,
            hash: LineHash::of(text),
        }
    }

    fn edit(content: &str, start: usize, end: usize, new_string: &str) -> AnchoredEdit {
        AnchoredEdit {
            start: anchor(content, start),
            end: anchor(content, end),
            new_string: new_string.to_string(),
        }
    }

    #[test]
    fn line_hash_is_stable_four_hex_digits() {
        // Pinned values: the hash is a wire contract between a read in one
        // process and an edit in another, so it must never drift.
        assert_eq!(LineHash::of("").to_string(), "1cd9");
        assert_eq!(LineHash::of("fn main() {").to_string().len(), 4);
        assert_eq!(LineHash::of("alpha"), LineHash::of("alpha"));
        assert_ne!(LineHash::of("alpha"), LineHash::of("alpha "));
    }

    #[test]
    fn anchors_parse_and_round_trip() {
        let parsed = LineAnchor::parse(" 42:A3F0 ").expect("anchor parses");
        assert_eq!(parsed.line, 42);
        assert_eq!(parsed.to_string(), "42:a3f0");
        for bad in [
            "", "42", ":a3f0", "0:a3f0", "42:a3f", "42:a3f0f", "42:zzzz", "x:a3f0",
        ] {
            assert!(LineAnchor::parse(bad).is_none(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn anchored_edits_replace_insert_and_delete_whole_lines() {
        let content = "one\ntwo\nthree\nfour\n";
        let replaced = apply_anchored_edits(content, &[edit(content, 2, 3, "TWO\nTHREE")])
            .expect("fresh anchors apply");
        assert_eq!(replaced, "one\nTWO\nTHREE\nfour\n");

        let deleted =
            apply_anchored_edits(content, &[edit(content, 2, 2, "")]).expect("deletion applies");
        assert_eq!(deleted, "one\nthree\nfour\n");

        let expanded = apply_anchored_edits(content, &[edit(content, 1, 1, "zero\none")])
            .expect("expansion applies");
        assert_eq!(expanded, "zero\none\ntwo\nthree\nfour\n");

        let multi = apply_anchored_edits(
            content,
            &[edit(content, 4, 4, "FOUR"), edit(content, 1, 1, "ONE")],
        )
        .expect("disjoint edits apply in any order");
        assert_eq!(multi, "ONE\ntwo\nthree\nFOUR\n");
    }

    #[test]
    fn anchored_edit_keeps_the_final_line_terminator_state() {
        let content = "a\nb";
        assert_eq!(
            apply_anchored_edits(content, &[edit(content, 2, 2, "B\n")]).expect("applies"),
            "a\nB"
        );
        let content = "a\nb\n";
        assert_eq!(
            apply_anchored_edits(content, &[edit(content, 2, 2, "B")]).expect("applies"),
            "a\nB\n"
        );
    }

    #[test]
    fn anchored_edits_reject_stale_out_of_range_inverted_overlapping_and_no_op() {
        let content = "one\ntwo\nthree\n";
        let mut stale = edit(content, 2, 2, "TWO");
        stale.start.hash = LineHash::of("not two");
        stale.end = stale.start;
        assert!(matches!(
            apply_anchored_edits(content, &[stale]),
            Err(AnchoredEditError::Stale { edit_index: 0, anchor, .. }) if anchor.line == 2
        ));

        let mut beyond = edit(content, 3, 3, "x");
        beyond.end.line = 9;
        assert!(matches!(
            apply_anchored_edits(content, &[beyond]),
            Err(AnchoredEditError::OutOfRange { total_lines: 3, .. })
        ));

        let inverted = AnchoredEdit {
            start: anchor(content, 3),
            end: anchor(content, 1),
            new_string: "x".to_string(),
        };
        assert_eq!(
            apply_anchored_edits(content, &[inverted]),
            Err(AnchoredEditError::Inverted { edit_index: 0 })
        );

        assert_eq!(
            apply_anchored_edits(
                content,
                &[edit(content, 1, 2, "x"), edit(content, 2, 3, "y")]
            ),
            Err(AnchoredEditError::Overlap {
                previous_edit_index: 0,
                current_edit_index: 1
            })
        );

        assert_eq!(
            apply_anchored_edits(content, &[edit(content, 2, 2, "two")]),
            Err(AnchoredEditError::NoChange)
        );
    }

    #[test]
    fn a_stale_edit_never_partially_applies() {
        let content = "one\ntwo\nthree\n";
        let good = edit(content, 1, 1, "ONE");
        let mut stale = edit(content, 3, 3, "THREE");
        stale.start.hash = LineHash::of("changed");
        stale.end = stale.start;
        assert!(apply_anchored_edits(content, &[good, stale]).is_err());
    }

    #[test]
    fn fresh_anchors_cover_the_neighbourhood() {
        let content = "a\nb\nc\nd\ne\n";
        let anchors = fresh_anchors_around(content, 1);
        assert_eq!(anchors.split(' ').count(), 3);
        assert!(anchors.starts_with("1:"));
        assert_eq!(fresh_anchors_around(content, 3).split(' ').count(), 5);
    }
}
