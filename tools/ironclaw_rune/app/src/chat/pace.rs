//! Paced streaming, after MonoCode's `reveal.rs` `PacedText` (MIT, see
//! `THIRD_PARTY_NOTICES.md`). The gateway delivers a reply in bursts; the
//! reader sees it at a steady rate instead: at least 120 characters a
//! second, catching up on a backlog within about 120 ms, always ending on a
//! whole word. Markdown's word reveal then lights each word up as it lands,
//! so a burst reads as words arriving one after another.
//!
//! The answer and the thought being written are paced alike: a live
//! thought's words come in at the same rate instead of whenever the
//! gateway's next burst lands.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use zlo_client::transcript::Row;

/// Commit steps are at least this far apart: just under one 60 Hz frame,
/// so a step lands on every display frame the caller's tick asks for
/// (`chat::pace_tick`) and a frame that comes a little early is not skipped.
const STEP: Duration = Duration::from_millis(14);
/// A word still being typed is held back this long before it shows.
const HOLD: Duration = Duration::from_millis(90);
const FLOOR_CHARS_PER_SEC: f64 = 120.;
const CATCH_UP_SECS: f64 = 0.12;

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

/// Paced text for every assistant reply and thought that is being written
/// or still catching up. History never enters it, so a reopened thread
/// shows at once.
#[derive(Default)]
pub(super) struct Pacer {
    replies: HashMap<String, Paced>,
}

/// The text of a row the pacer reveals, and whether it is still being
/// written. A thought carries no flag of its own: the one being written is
/// the live run's last row.
fn paceable(row: &Row, tail_live: bool) -> Option<(&str, &str, bool)> {
    match row {
        Row::Assistant {
            key,
            text,
            streaming,
        } => Some((key, text, *streaming)),
        Row::Thinking { key, text } => Some((key, text, tail_live)),
        _ => None,
    }
}

/// `row` with its text cut to what the reader sees now.
fn with_text(row: &Row, shown: String) -> Row {
    match row {
        Row::Assistant { key, .. } => Row::Assistant {
            key: key.clone(),
            text: shown,
            streaming: true,
        },
        Row::Thinking { key, .. } => Row::Thinking {
            key: key.clone(),
            text: shown,
        },
        other => other.clone(),
    }
}

impl Pacer {
    /// Rows as the reader should see them now, and whether any text is
    /// still behind (the caller keeps frames coming while it is). `live`:
    /// a run is in flight, so its last row may be a thought being written.
    pub(super) fn apply<'a>(
        &mut self,
        rows: &'a [Row],
        live: bool,
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
        let mut writing = Vec::new();
        let last = rows.len().saturating_sub(1);
        for (ix, row) in rows.iter().enumerate() {
            let Some((key, text, streaming)) = paceable(row, live && ix == last) else {
                continue;
            };
            if !streaming && !self.replies.contains_key(key) {
                continue;
            }
            let paced = self
                .replies
                .entry(key.to_string())
                .or_insert_with(|| Paced::new(now));
            paced.update(text, now);
            paced.step(now, streaming);
            seen.push(key.to_string());
            if streaming {
                writing.push(key.to_string());
            }
            if !streaming && !paced.behind() {
                continue;
            }
            behind |= paced.behind();
            let shown = paced.target[..paced.shown].to_string();
            let out = out.get_or_insert_with(|| rows.to_vec());
            out[ix] = with_text(row, shown);
        }
        self.replies
            .retain(|key, paced| seen.contains(key) && (paced.behind() || writing.contains(key)));
        (out.map_or(Cow::Borrowed(rows), Cow::Owned), behind)
    }
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
        let (first, behind) = pacer.apply(&rows, true, t0, false);
        assert!(behind);
        assert_eq!(shown(&first), "");
        let (mid, _) = pacer.apply(&rows, true, t0 + Duration::from_millis(60), false);
        let mid = shown(&mid).to_string();
        assert!(!mid.is_empty() && mid.len() < 48, "{mid:?}");
        let full = "one two three four five six seven eight nine ten";
        assert!(
            full[mid.len()..].starts_with(' '),
            "ends on a word: {mid:?}"
        );
        let done = reply("one two three four five six seven eight nine ten", false);
        let mut t = t0 + Duration::from_millis(60);
        for _ in 0..100 {
            t += Duration::from_millis(33);
            if !pacer.apply(&done, false, t, false).1 {
                break;
            }
        }
        let (last, behind) = pacer.apply(&done, false, t + Duration::from_millis(33), false);
        assert!(!behind);
        assert!(
            matches!(last, Cow::Borrowed(_)),
            "a settled reply renders as is"
        );
    }

    #[test]
    fn history_is_never_paced() {
        let mut pacer = Pacer::default();
        let rows = reply("an old answer", false);
        let (out, behind) = pacer.apply(&rows, false, Instant::now(), false);
        assert!(!behind);
        assert_eq!(shown(&out), "an old answer");
    }

    fn thought(text: &str) -> Row {
        Row::Thinking {
            key: "t".into(),
            text: text.into(),
        }
    }

    fn text_of(row: &Row) -> &str {
        match row {
            Row::Assistant { text, .. } | Row::Thinking { text, .. } => text,
            _ => "",
        }
    }

    /// Tokens of `chunk` bytes every `every`, stepped once per 60 Hz frame
    /// for `secs`: the worst time any byte waited between arriving and
    /// showing, for the row `pick` takes out of each frame's rows.
    fn worst_wait(
        rows_for: impl Fn(&str) -> Vec<Row>,
        live: bool,
        pick: usize,
        every: Duration,
        secs: f64,
    ) -> Duration {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        let frame = Duration::from_micros(16_667);
        let mut target = String::new();
        let mut arrived: Vec<Instant> = Vec::new();
        let mut next_token = t0;
        let mut worst = Duration::ZERO;
        let mut shown_len = 0usize;
        let mut t = t0;
        while t.duration_since(t0).as_secs_f64() < secs {
            while next_token <= t {
                let word = format!("w{} ", arrived.len() % 10);
                for _ in 0..word.len() {
                    arrived.push(next_token);
                }
                target.push_str(&word);
                next_token += every;
            }
            let rows = rows_for(&target);
            let (out, _) = pacer.apply(&rows, live, t, false);
            let now_len = text_of(&out[pick]).len();
            for at in &arrived[shown_len..now_len] {
                worst = worst.max(t.duration_since(*at));
            }
            shown_len = shown_len.max(now_len);
            t += frame;
        }
        worst
    }

    #[test]
    fn a_steady_stream_shows_each_word_within_a_few_frames() {
        // ~100 tokens a second in 33 ms bursts (what the gateway now sends).
        let worst = worst_wait(
            |text| reply(text, true),
            true,
            0,
            Duration::from_millis(10),
            2.0,
        );
        eprintln!("steady stream: worst arrival-to-shown wait {worst:?}");
        assert!(
            worst <= Duration::from_millis(200),
            "a word waited {worst:?} between arriving and showing"
        );
    }

    #[test]
    fn the_thought_being_written_is_paced_like_the_answer() {
        let mut pacer = Pacer::default();
        let t0 = Instant::now();
        let rows = vec![thought("one two three four five six seven eight nine ten")];
        let (first, behind) = pacer.apply(&rows, true, t0, false);
        assert!(behind, "a live thought's burst is revealed, not dropped in");
        assert_eq!(text_of(&first[0]), "");
        let (mid, _) = pacer.apply(&rows, true, t0 + Duration::from_millis(40), false);
        let mid = text_of(&mid[0]).to_string();
        assert!(!mid.is_empty() && mid.len() < 48, "{mid:?}");
        // The thought ends (a tool row follows): it finishes its reveal,
        // then renders as is.
        let mut after = rows.clone();
        after.push(Row::System {
            key: "s".into(),
            text: "next".into(),
        });
        let mut t = t0 + Duration::from_millis(40);
        for _ in 0..100 {
            t += Duration::from_millis(16);
            if !pacer.apply(&after, true, t, false).1 {
                break;
            }
        }
        let (last, behind) = pacer.apply(&after, true, t + Duration::from_millis(16), false);
        assert!(!behind);
        assert!(matches!(last, Cow::Borrowed(_)));
    }

    #[test]
    fn a_finished_thought_is_never_revealed_again() {
        let mut pacer = Pacer::default();
        let rows = vec![
            thought("an old thought"),
            Row::System {
                key: "s".into(),
                text: "after".into(),
            },
        ];
        let (out, behind) = pacer.apply(&rows, true, Instant::now(), false);
        assert!(!behind);
        assert_eq!(text_of(&out[0]), "an old thought");
        // Nor when no run is live, even as the last row.
        let done = [thought("done")];
        let (out, behind) = pacer.apply(&done, false, Instant::now(), false);
        assert!(!behind);
        assert_eq!(text_of(&out[0]), "done");
    }
}
