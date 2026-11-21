//! Frame-time benchmark scenes (`bench-*` in `scenes.rs`), driven by
//! `bench/run.py`. Each scene is the window in a fixed state; the script in
//! `bench/scripts/NAME.json` supplies the person's input (taps, keys, wheel,
//! trackpad) and [`poll`] supplies what the gateway would (a reply streaming
//! in bursts, a switched thread's timeline landing, runs starting and
//! ending), on the frame clock, so every run sees the same frames. No
//! gateway is needed. Prefs are reset to their defaults so a person's
//! settings (reduced motion, font size, docks) never change the numbers.

use std::collections::HashMap;
use std::rc::Rc;

use rune::Tx;
use zlo_client::inbox::{InboxState, InboxStatus};
use zlo_client::transcript::{Row, ToolStatus};

use crate::chat::{self, ChatState};
use crate::state::{Conn, Page, State};
use crate::workspace::fixtures as ws;

/// What a bench scene feeds in on its own, keyed by the frame clock.
pub struct Bench {
    feed: Feed,
    /// The frame clock at the scene's first poll; feed times count from it.
    start: Option<f64>,
    /// How far the feed has got (its own meaning per feed).
    step: usize,
}

enum Feed {
    /// Input only: the script does everything.
    None,
    /// A reply streaming into the open chat in bursts, then the run ends.
    Stream {
        thread: String,
        bursts: Vec<(f64, String)>,
        end: f64,
    },
    /// A thread opened from the sidebar gets its timeline (these rows) the
    /// frame after it opens, as a load from a fast gateway would.
    Timelines(HashMap<String, Vec<Row>>),
    /// Runs start and end: at each time, a thread's status changes.
    Shelf(Vec<(f64, &'static str, Option<InboxStatus>)>),
}

/// The app's poll calls this before every frame; it is a no-op outside the
/// bench scenes.
pub fn poll(s: &mut State, clock: f64, tx: &mut Tx) -> bool {
    let Some(b) = s.bench.as_mut() else {
        return false;
    };
    let start = *b.start.get_or_insert(clock);
    let t = clock - start;
    let mut next: Option<f64> = None;
    let mut stream: Option<(String, String, bool)> = None;
    let mut statuses = Vec::new();
    let mut changed = false;
    match &mut b.feed {
        Feed::None => {}
        Feed::Stream {
            thread,
            bursts,
            end,
        } => {
            let mut grow = String::new();
            while let Some((at, text)) = bursts.get(b.step) {
                if *at > t {
                    next = Some(*at);
                    break;
                }
                grow.push_str(text);
                b.step += 1;
            }
            // One step past the last burst: the run has ended.
            let mut finish = false;
            if b.step == bursts.len() {
                if t >= *end {
                    finish = true;
                    b.step += 1;
                } else {
                    next = Some(*end);
                }
            }
            if !grow.is_empty() || finish {
                stream = Some((thread.clone(), grow, finish));
            }
        }
        Feed::Timelines(timelines) => {
            if let Some(c) = s.chat.as_mut()
                && c.rows.is_empty()
                && let Some(rows) = timelines.get(&c.thread_id)
            {
                chat::fixtures::fill(c, rows.clone(), false);
                changed = true;
            }
        }
        Feed::Shelf(events) => {
            while let Some(&(at, id, status)) = events.get(b.step) {
                if at > t {
                    next = Some(at);
                    break;
                }
                statuses.push((id, status));
                b.step += 1;
            }
        }
    }
    if let Some((thread, grow, finish)) = stream {
        stream_into(s, &thread, &grow, finish, tx);
        changed = true;
    }
    for (id, status) in statuses {
        ws::bench_set_status(s, id, status);
        changed = true;
    }
    if let Some(at) = next {
        tx.wake_after(std::time::Duration::from_secs_f64((at - t).max(0.)));
    }
    changed
}

/// Grow the streaming reply of `thread` by `text`, as a stream delta would
/// (rows rebuilt, then folded through the pacer); `finish` ends the run.
fn stream_into(s: &mut State, thread: &str, text: &str, finish: bool, tx: &mut Tx) {
    let Some(c) = chat::chat_for(s, thread) else {
        return;
    };
    let mut rows = (*c.rows).clone();
    if let Some(Row::Assistant {
        text: reply,
        streaming,
        ..
    }) = rows.iter_mut().rev().find(|r| {
        matches!(
            r,
            Row::Assistant {
                streaming: true,
                ..
            }
        )
    }) {
        reply.push_str(text);
        if finish {
            *streaming = false;
        }
    }
    if finish {
        c.fixture_run = None;
    }
    c.rows = Rc::new(rows);
    c.rows_gen += 1;
    chat::sync_items(s, thread, tx);
}

fn user(key: &str, text: &str) -> Row {
    Row::User {
        key: key.into(),
        text: text.into(),
        pending: false,
        attachments: Vec::new(),
    }
}

fn reply(key: &str, text: &str, streaming: bool) -> Row {
    Row::Assistant {
        key: key.into(),
        text: text.into(),
        streaming,
    }
}

fn tool(key: &str, title: &str, detail: &str, status: ToolStatus) -> Row {
    Row::Tool {
        key: key.into(),
        title: title.into(),
        detail: Some(detail.into()),
        status,
        reason: None,
    }
}

/// The reply the stream scene types out: headings, a list, inline code, a
/// table and a code block, about 2.6k characters.
const STREAMED: &str = include_str!("bench_reply.md");

/// `turns` question/answer pairs keyed `{prefix}`; every fourth answer has a
/// code block, every seventh a table, as a real coding thread does.
fn conversation(prefix: &str, turns: usize) -> Vec<Row> {
    let mut rows = Vec::with_capacity(turns * 2);
    for i in 0..turns {
        rows.push(user(
            &format!("msg:{prefix}u{i}"),
            &format!("Question {i}: how does the transcript keep its place while it grows?"),
        ));
        let mut text = format!(
            "Answer {i}. The list is a **growth stack** in a scroll viewport:\n\n\
             - only the rows near the viewport are built\n\
             - finished rows are kept by `memo`\n\n\
             It follows its end while a reply streams, and `scroll_into_view` \
             still reaches row {i} from anywhere."
        );
        if i % 4 == 0 {
            text.push_str(
                "\n\n```rust\nfn fold(rows: &[Row]) -> Vec<Item> {\n    \
                 let mut out = Vec::with_capacity(rows.len());\n    \
                 for row in rows {\n        out.push(Item::Row(row.clone()));\n    }\n    \
                 out\n}\n```",
            );
        }
        if i % 7 == 0 {
            text.push_str(
                "\n\n| crate | change | risk |\n|---|---|---|\n\
                 | ironclaw_rune | transcript | low |\n| zlo_client | none | — |",
            );
        }
        rows.push(reply(&format!("msg:{prefix}a{i}"), &text, false));
    }
    rows
}

/// The window every bench scene starts from: default prefs, the sidebar
/// open with `rows`' threads (plus a few more), `open` in the chat.
fn shell(s: &mut State, feed: Feed, open: &str, rows: Vec<Row>, live: bool) {
    s.prefs = Default::default();
    s.palette = crate::design::resolve(&s.prefs, s.system_dark);
    ws::bench_shell(s);
    s.conn = Conn::Ready;
    s.page = Page::Chat;
    let mut c = ChatState::new(open.into());
    chat::fixtures::fill(&mut c, rows, live);
    s.chat = Some(c);
    s.bench = Some(Box::new(Bench {
        feed,
        start: None,
        step: 0,
    }));
}

/// Token bursts over `STREAMED`: every 45–75 ms, 6–48 characters, from a
/// fixed seed.
fn bursts(from: f64) -> (Vec<(f64, String)>, f64) {
    let mut seed: u32 = 0x5eed;
    let mut next = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        seed >> 8
    };
    let chars: Vec<char> = STREAMED.chars().collect();
    let (mut at, mut i, mut out) = (from, 0, Vec::new());
    while i < chars.len() {
        let n = 6 + (next() % 43) as usize;
        let end = (i + n).min(chars.len());
        out.push((at, chars[i..end].iter().collect()));
        i = end;
        at += 0.045 + f64::from(next() % 31) / 1000.;
    }
    (out, at + 0.3)
}

/// (1) A long reply streams in token bursts (Markdown and a code block)
/// under a 40-turn history, then the run ends.
pub fn stream(s: &mut State) {
    let mut rows = conversation("h", 40);
    rows.push(user(
        "msg:live-q",
        "Write up how the pacer and the fold fit together.",
    ));
    rows.push(reply("text:live", "", true));
    let (bursts, end) = bursts(0.25);
    shell(
        s,
        Feed::Stream {
            thread: "t1".into(),
            bursts,
            end,
        },
        "t1",
        rows,
        true,
    );
}

/// (2) Two long threads; the script switches between them from the sidebar.
pub fn switch(s: &mut State) {
    let t1 = conversation("a", 150);
    let t2 = conversation("b", 150);
    let timelines = HashMap::from([("t1".to_string(), t1.clone()), ("t2".to_string(), t2)]);
    shell(s, Feed::Timelines(timelines), "t1", t1, false);
}

/// (3) The sidebar slides shut and open (⌘B) over a 40-turn chat.
pub fn sidebar(s: &mut State) {
    shell(s, Feed::None, "t1", conversation("h", 40), false);
}

/// (4) A 500-message thread (250 turns): wheel notches, then trackpad swipes.
pub fn scroll(s: &mut State) {
    shell(s, Feed::None, "t1", conversation("h", 250), false);
}

/// (5) The right pane opens and closes (⌥⌘B) over a 40-turn chat.
pub fn dock(s: &mut State) {
    shell(s, Feed::None, "t1", conversation("h", 40), false);
    s.prefs.side_open = false;
}

/// (6) Typing into the composer of a 40-turn chat.
pub fn typing(s: &mut State) {
    shell(s, Feed::None, "t1", conversation("h", 40), false);
}

/// (7) Runs start and end in two threads: their cards leave the list for the
/// Working shelf and come back.
pub fn shelf(s: &mut State) {
    use InboxStatus::{Done, Working};
    let events = vec![
        (0.3, "t3", Some(Working)),
        (0.9, "t4", Some(Working)),
        (1.5, "t5", Some(Working)),
        (2.1, "t3", Some(Done)),
        (2.7, "t4", Some(Done)),
        (3.3, "t5", Some(Done)),
    ];
    shell(s, Feed::Shelf(events), "t1", conversation("h", 40), false);
}

/// (8) A finished turn with 1000 tool calls, its activity group collapsed;
/// the script opens it and scrolls through it.
pub fn tools(s: &mut State) {
    let mut rows = conversation("h", 6);
    rows.push(user("msg:tools-q", "Audit every crate's error handling."));
    let titles = [
        ("ironclaw.shell", "rg -n \"unwrap()\" crates/"),
        (
            "ironclaw.read_file",
            "crates/kernel/ironclaw_turns/src/lib.rs",
        ),
        (
            "ironclaw.edit_file",
            "crates/domains/ironclaw_llm/src/error.rs",
        ),
        ("ironclaw.shell", "cargo check -p ironclaw_llm"),
    ];
    for i in 0..1000 {
        let (title, detail) = titles[i % titles.len()];
        let status = if i % 97 == 13 {
            ToolStatus::Failed
        } else {
            ToolStatus::Completed
        };
        rows.push(tool(
            &format!("tool:b{i}"),
            title,
            &format!("{detail} #{i}"),
            status,
        ));
    }
    rows.push(reply(
        "msg:tools-a",
        "Done: 1000 calls, **11 failures**, all in tests.",
        false,
    ));
    shell(s, Feed::None, "t1", rows, false);
}

/// The inbox rows bench scenes list: the two long threads and enough others
/// to fill the sidebar.
pub(crate) fn inbox_rows() -> Vec<zlo_client::inbox::InboxRow> {
    let titles = [
        ("t1", "Port the frontend to Rune"),
        ("t2", "Fix the flaky integration test"),
        ("t3", "Run the integration suite"),
        ("t4", "Summarise yesterday's PRs"),
        ("t5", "Draft the release notes"),
        ("t6", "Approve the deploy plan"),
        ("t7", "Reply to the design review"),
        ("t8", "Profile the streaming path"),
        ("t9", "Upgrade wgpu"),
        ("t10", "Write the migration guide"),
    ];
    titles
        .iter()
        .enumerate()
        .map(|(i, (id, title))| {
            let mut r = ws::row(id, title, "default", InboxState::Inbox, 5 + 20 * i as i64);
            r.status = Some(InboxStatus::Done);
            r
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use rune::{App, AppSpec, Env, Runtime};
    use zlo_client::transcript::Row;

    use crate::state::State;

    fn no_init() -> State {
        unreachable!("tests build the state themselves")
    }

    /// The stream feed types the whole reply out on the frame clock and
    /// ends the run, through the chat's own fold.
    #[test]
    fn the_stream_feed_delivers_the_whole_reply_then_ends_the_run() {
        let app = App::new(AppSpec::new(
            "ZLO test",
            (1000., 760.),
            no_init,
            crate::view,
        ));
        let mut s = State::new(crate::jobs::Rt::new(app.handle()));
        super::stream(&mut s);
        let mut rt = Runtime::new(s, crate::view, (1000., 760.), Env::new(true, 1.));
        let mut t = 0.;
        while t < 10. {
            rt.update(move |s, tx| {
                super::poll(s, t, tx);
            });
            t += 1. / 60.;
            rt.frame(t);
        }
        rt.update(|s, _| {
            let Some(c) = s.chat.as_ref() else {
                panic!("the chat is open");
            };
            let reply = c.rows.iter().rev().find_map(|r| match r {
                Row::Assistant {
                    text, streaming, ..
                } => Some((text.clone(), *streaming)),
                _ => None,
            });
            assert_eq!(reply, Some((super::STREAMED.to_string(), false)));
            assert!(c.fixture_run.is_none(), "the run ended");
        });
    }

    /// The shelf feed moves a card onto the Working shelf and back.
    #[test]
    fn the_shelf_feed_starts_and_ends_runs() {
        use zlo_client::inbox::InboxState;
        let app = App::new(AppSpec::new(
            "ZLO test",
            (1000., 760.),
            no_init,
            crate::view,
        ));
        let mut s = State::new(crate::jobs::Rt::new(app.handle()));
        super::shelf(&mut s);
        let mut rt = Runtime::new(s, crate::view, (1000., 760.), Env::new(true, 1.));
        let state = |rt: &mut Runtime<State>, t: f64| {
            let mut out = None;
            rt.update(|s, tx| {
                super::poll(s, t, tx);
                out = s.workspace.inbox.row("t3").map(|r| r.state);
            });
            out
        };
        assert_eq!(state(&mut rt, 0.), Some(InboxState::Inbox));
        assert_eq!(state(&mut rt, 1.), Some(InboxState::Working));
        assert_eq!(state(&mut rt, 2.5), Some(InboxState::Inbox));
    }
}
