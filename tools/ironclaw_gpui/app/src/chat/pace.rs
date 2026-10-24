//! Paced streaming, after MonoCode's `reveal.rs` `PacedText` (MIT, see
//! `THIRD_PARTY_NOTICES.md`). The gateway delivers a reply in bursts; the
//! reader sees it at a steady rate instead: at least 90 characters a second,
//! catching up on a backlog within about 220 ms, always ending on a whole
//! word. TextView's stream fade then lights each word up as it lands, so a
//! burst reads as words arriving one after another.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use ironclaw_gpui_client::transcript::Row;

/// Commit steps are at least this far apart, matching the 30 Hz clock.
const STEP: Duration = Duration::from_millis(30);
/// A word still being typed is held back this long before it shows.
const HOLD: Duration = Duration::from_millis(150);
const FLOOR_CHARS_PER_SEC: f64 = 90.;
const CATCH_UP_SECS: f64 = 0.22;

struct Paced {
    target: String,
    shown: usize,
    position: f64,
    last: Instant,
    burst: Instant,
}

impl Paced {
    fn new(now: Instant) -> Self {
        Self {
            target: String::new(),
            shown: 0,
            position: 0.,
            last: now,
            burst: now,
        }
    }

    fn update(&mut self, text: &str, now: Instant) {
        if self.target == text {
            return;
        }
        if !text.starts_with(&self.target) {
            // Replaced, not extended: show it whole.
            self.target = text.to_string();
            self.shown = text.len();
            self.position = text.len() as f64;
        } else {
            self.target.push_str(&text[self.target.len()..]);
        }
        self.burst = now;
    }

    fn behind(&self) -> bool {
        self.shown < self.target.len()
    }

    fn step(&mut self, now: Instant, streaming: bool) {
        if !self.behind() || now.duration_since(self.last) < STEP {
            return;
        }
        let dt = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        let len = self.target.len() as f64;
        let backlog = len - self.position;
        let advance = (FLOOR_CHARS_PER_SEC * dt).max(backlog * (1. - (-dt / CATCH_UP_SECS).exp()));
        self.position = (self.position + advance).min(len);
        let mut at = (self.position.ceil() as usize).min(self.target.len());
        while at < self.target.len() && !self.target.is_char_boundary(at) {
            at += 1;
        }
        // End on a word boundary; a half-typed last word waits for its
        // space unless the stream paused or ended.
        let end = self.target[at..]
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, _)| at + i)
            .unwrap_or_else(|| {
                if !streaming || now.duration_since(self.burst) >= HOLD {
                    self.target.len()
                } else {
                    self.target
                        .char_indices()
                        .rev()
                        .find(|(_, c)| c.is_whitespace())
                        .map_or(0, |(i, c)| i + c.len_utf8())
                }
            });
        self.shown = self.shown.max(end);
    }
}

/// Paced text for every assistant reply that is streaming or still
/// catching up. History never enters it, so a reopened thread shows at once.
#[derive(Default)]
pub(super) struct Pacer {
    replies: HashMap<String, Paced>,
}

impl Pacer {
    /// Rows as the reader should see them now, and whether any reply is
    /// still behind (the caller keeps frames coming while it is).
    pub(super) fn apply<'a>(
        &mut self,
        rows: &'a [Row],
        now: Instant,
        reduced: bool,
    ) -> (Cow<'a, [Row]>, bool) {
        if reduced {
            self.replies.clear();
            return (Cow::Borrowed(rows), false);
        }
        let mut out: Option<Vec<Row>> = None;
        let mut behind = false;
        let mut seen = Vec::new();
        for (ix, row) in rows.iter().enumerate() {
            let Row::Assistant {
                key,
                text,
                streaming,
            } = row
            else {
                continue;
            };
            if !*streaming && !self.replies.contains_key(key) {
                continue;
            }
            let paced = self
                .replies
                .entry(key.clone())
                .or_insert_with(|| Paced::new(now));
            paced.update(text, now);
            paced.step(now, *streaming);
            seen.push(key.clone());
            if !*streaming && !paced.behind() {
                continue;
            }
            behind |= paced.behind();
            let shown = paced.target[..paced.shown].to_string();
            out.get_or_insert_with(|| rows.to_vec())[ix] = Row::Assistant {
                key: key.clone(),
                text: shown,
                streaming: true,
            };
        }
        self.replies.retain(|key, paced| {
            seen.contains(key) && (paced.behind() || rows.iter().any(|r| is_streaming(r, key)))
        });
        (out.map_or(Cow::Borrowed(rows), Cow::Owned), behind)
    }
}

fn is_streaming(row: &Row, key: &str) -> bool {
    matches!(row, Row::Assistant { key: k, streaming: true, .. } if k == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(text: &str, streaming: bool) -> Vec<Row> {
        vec![Row::Assistant {
            key: "a".into(),
            text: text.into(),
            streaming,
        }]
    }

    fn shown(rows: &[Row]) -> &str {
        match &rows[0] {
            Row::Assistant { text, .. } => text,
            _ => "",
        }
    }

    #[test]
    fn a_burst_is_revealed_word_by_word_then_settles() {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        let rows = reply("one two three four five six seven eight nine ten", true);
        let (first, behind) = pacer.apply(&rows, t0, false);
        assert!(behind);
        assert_eq!(shown(&first), "");
        let (mid, _) = pacer.apply(&rows, t0 + Duration::from_millis(60), false);
        let mid = shown(&mid).to_string();
        assert!(!mid.is_empty() && mid.len() < 48, "{mid:?}");
        let full = "one two three four five six seven eight nine ten";
        assert!(full[mid.len()..].starts_with(' '), "ends on a word: {mid:?}");
        let done = reply("one two three four five six seven eight nine ten", false);
        let mut t = t0 + Duration::from_millis(60);
        for _ in 0..100 {
            t += Duration::from_millis(33);
            if !pacer.apply(&done, t, false).1 {
                break;
            }
        }
        let (last, behind) = pacer.apply(&done, t + Duration::from_millis(33), false);
        assert!(!behind);
        assert!(matches!(last, Cow::Borrowed(_)), "a settled reply renders as is");
    }

    #[test]
    fn history_is_never_paced() {
        let mut pacer = Pacer::default();
        let rows = reply("an old answer", false);
        let (out, behind) = pacer.apply(&rows, Instant::now(), false);
        assert!(!behind);
        assert_eq!(shown(&out), "an old answer");
    }
}
