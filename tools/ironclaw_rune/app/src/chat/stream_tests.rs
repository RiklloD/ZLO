//! The reserve while an answer streams: the prompt must not creep while the
//! answer below it grows.

use std::rc::Rc;

use rune::{App, AppSpec, Env, Runtime};
use zlo_client::transcript::Row;

use super::{Reserve, sync_items};
use crate::state::State;

fn no_init() -> State {
    unreachable!("tests build the state themselves")
}

/// The `chat-running` scene with its last prompt just sent: the reserve
/// held and the list pinned so that prompt sits at the top.
fn sent(s: &mut State) {
    super::fixtures::running(s);
    if let Some(c) = s.chat.as_mut() {
        c.reserve = Some(Reserve { held: true });
        c.pin_next = true;
        c.rise = None;
    }
}

/// Grow the streaming reply by `words` words, as a stream delta would.
fn stream(rt: &mut Runtime<State>, words: usize) {
    rt.update(move |s, tx| {
        let Some(c) = s.chat.as_mut() else { return };
        let mut rows = (*c.rows).clone();
        if let Some(Row::Assistant { text, .. }) = rows.iter_mut().rev().find(|r| {
            matches!(
                r,
                Row::Assistant {
                    streaming: true,
                    ..
                }
            )
        }) {
            for i in 0..words {
                text.push_str(if i % 9 == 8 {
                    " wraps the line.\n\n"
                } else {
                    " more"
                });
            }
        }
        c.rows = Rc::new(rows);
        c.rows_gen += 1;
        sync_items(s, "t1", tx);
    });
}

#[test]
fn the_sent_prompt_holds_its_place_while_the_answer_streams() {
    let app = App::new(AppSpec::new(
        "ZLO test",
        (1000., 760.),
        no_init,
        crate::view,
    ));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    sent(&mut s);
    let mut rt = Runtime::new(s, crate::view, (1000., 760.), Env::new(true, 1.));
    let dt = 1. / 60.;
    rt.frame(0.);
    rt.update(|s, tx| {
        sync_items(s, "t1", tx);
    });
    let mut t = 0.;
    // Let the send's climb and the reserve settle.
    for _ in 0..90 {
        t += dt;
        rt.frame(t);
    }
    let top = |rt: &Runtime<State>| rt.presented("row:msg:5").map(|(r, _)| r[1]);
    let Some(rest) = top(&rt) else {
        panic!("the prompt row is drawn");
    };
    let mut worst: f32 = 0.;
    for frame in 0..150 {
        if frame % 4 == 0 && frame < 110 {
            stream(&mut rt, 6);
        }
        t += dt;
        rt.frame(t);
        if let Some(y) = top(&rt) {
            worst = worst.max((y - rest).abs());
        }
    }
    eprintln!("prompt drift while streaming: {worst:.1} pt");
    assert!(
        worst <= 1.5,
        "the prompt crept {worst:.1} pt while its answer streamed"
    );
}

#[test]
fn an_answer_longer_than_the_room_is_followed_to_its_end() {
    let app = App::new(AppSpec::new(
        "ZLO test",
        (1000., 760.),
        no_init,
        crate::view,
    ));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    sent(&mut s);
    let mut rt = Runtime::new(s, crate::view, (1000., 760.), Env::new(true, 1.));
    let dt = 1. / 60.;
    rt.frame(0.);
    rt.update(|s, tx| {
        sync_items(s, "t1", tx);
    });
    let mut t = 0.;
    for frame in 0..600 {
        if frame % 4 == 0 && frame < 400 {
            stream(&mut rt, 12);
        }
        t += dt;
        rt.frame(t);
    }
    let Some((list, _)) = rt.presented("transcript:t1") else {
        panic!("the transcript is drawn");
    };
    let Some((reply, _)) = rt.presented("row:text:live") else {
        panic!("the reply is drawn");
    };
    let (list_end, reply_end) = (list[1] + list[3], reply[1] + reply[3]);
    eprintln!("list end {list_end:.1}, reply end {reply_end:.1}");
    // Under the reply: the working line and the last row's padding.
    assert!(
        reply_end <= list_end && list_end - reply_end < 120.,
        "the list follows the answer's end: reply ends at {reply_end:.1}, list at {list_end:.1}"
    );
}

/// A short conversation from the timeline, as `load_timeline` sets it,
/// plus `extra` user messages after it.
fn page(extra: &[&str]) -> zlo_client::wire::TimelinePage {
    // Earlier turns: enough that the list scrolls, as in the recording.
    let mut messages: Vec<serde_json::Value> = (0..12)
        .map(|i| {
            let (kind, status) = if i % 2 == 0 {
                ("user", "accepted")
            } else {
                ("assistant", "finalized")
            };
            serde_json::json!({"message_id": format!("e{i}"), "sequence": i + 1,
                "kind": kind, "status": status, "turn_run_id": format!("e{}", i / 2),
                "content": format!("earlier message number {i}, long enough to take a line or two of the column")})
        })
        .collect();
    messages.extend([
        serde_json::json!({"message_id": "m1", "sequence": 21, "kind": "user",
            "status": "accepted", "turn_run_id": "r1", "content": "heho"}),
        serde_json::json!({"message_id": "m2", "sequence": 22, "kind": "assistant",
            "status": "finalized", "turn_run_id": "r1",
            "content": "Heho back. Want to start on something, or just saying hi?"}),
        serde_json::json!({"message_id": "m3", "sequence": 23, "kind": "user",
            "status": "accepted", "turn_run_id": "r2", "content": "im only testing stuff here"}),
        serde_json::json!({"message_id": "m4", "sequence": 24, "kind": "assistant",
            "status": "finalized", "turn_run_id": "r2",
            "content": "Fair enough, test away. If it helps, you could also try a chart or table, which render in the reply, or a small HTML page, which opens in the side panel."}),
    ]);
    for (i, text) in extra.iter().enumerate() {
        let (kind, id, status) = match text.strip_prefix("> ") {
            Some(_) => ("assistant", format!("a{i}"), "finalized"),
            None => ("user", format!("u{i}"), "accepted"),
        };
        let content = text.strip_prefix("> ").unwrap_or(text);
        messages.push(serde_json::json!({"message_id": id,
            "sequence": 25 + i, "kind": kind, "status": status,
            "turn_run_id": format!("x{}", i / 2), "content": content}));
    }
    serde_json::from_value(serde_json::json!({
        "thread": { "thread_id": "t1" }, "messages": messages
    }))
    .expect("timeline decodes")
}

/// A transcript-backed chat, as a real thread is (not fixture rows).
fn real_chat() -> State {
    let app = App::new(AppSpec::new(
        "ZLO test",
        (1000., 760.),
        no_init,
        crate::view,
    ));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    super::fixtures::running(&mut s);
    // The recording's prefs.
    s.prefs.ui_font_size = 17.;
    s.prefs.conversation_width = 800.;
    let c = s.chat.as_mut().expect("chat");
    c.fixture_run = None;
    c.composer.clear();
    c.transcript = Default::default();
    c.transcript.set_timeline(page(&[]));
    s
}

/// What `actions::send` does to the view, without the network.
fn send_like(s: &mut State, text: &str, tx: &mut rune::Tx) -> u64 {
    let c = s.chat.as_mut().expect("chat");
    let pending = c.transcript.push_pending_user(text, &[]);
    c.turn_heights.clear();
    c.reserve = Some(Reserve { held: true });
    c.pin_next = true;
    c.far_from_bottom = false;
    tx.motion(super::view::PROMPT_RISE_SPRING);
    super::refresh_rows(s, "t1", tx);
    pending
}

/// The bug in the 2026-11-17 recording: after a send, the prompt never
/// climbed to the top, and when its server copy replaced the optimistic
/// bubble the whole conversation jumped. Every row must move only on the
/// send's climb, then hold still through the ack.
#[test]
fn a_send_climbs_once_and_the_ack_moves_nothing() {
    let s = real_chat();
    let mut rt = Runtime::new(s, crate::view, (1352., 878.), Env::new(true, 2.));
    let dt = 1. / 60.;
    let mut t = 0.;
    rt.frame(t);
    rt.update(|s, tx| {
        super::refresh_rows(s, "t1", tx);
    });
    for _ in 0..30 {
        t += dt;
        rt.frame(t);
    }
    let mut first = 0;
    rt.update(|s, tx| {
        first = send_like(s, "yo testing", tx);
    });
    for _ in 0..90 {
        t += dt;
        rt.frame(t);
    }
    let key = format!("row:pending:{first}");
    let (list, _) = rt.presented("transcript:t1").expect("list drawn");
    let (row, _) = rt.presented(&key).expect("prompt drawn");
    eprintln!("list top {:.1}, prompt top {:.1}", list[1], row[1]);
    assert!(
        row[1] - list[1] < 60.,
        "the sent prompt climbs to the top of the list: list {:.1}, prompt {:.1}",
        list[1],
        row[1]
    );
    // The climb's window is wall-clock time; let it pass.
    std::thread::sleep(super::view::CLIMB);
    let rest = row[1];
    let other = rt.presented("row:msg:m4").map(|(r, _)| r[1]);
    // The gateway accepts, the timeline has the server copy.
    rt.update(move |s, tx| {
        let c = s.chat.as_mut().expect("chat");
        c.transcript.ack_pending_user(first);
        c.transcript.set_timeline(page(&["yo testing"]));
        super::refresh_rows(s, "t1", tx);
    });
    let mut worst: f32 = 0.;
    let mut copies = 0;
    for _ in 0..90 {
        t += dt;
        rt.frame(t);
        if let Some((r, _)) = rt.presented(&key) {
            worst = worst.max((r[1] - rest).abs());
        }
        if rt.presented("row:msg:u0").is_some() {
            copies += 1;
        }
        if let (Some(before), Some((r, _))) = (other, rt.presented("row:msg:m4")) {
            worst = worst.max((r[1] - before).abs());
        }
    }
    eprintln!("drift after the ack: {worst:.1} pt");
    assert_eq!(copies, 0, "the server copy reuses the bubble's element");
    assert!(worst <= 1.5, "the ack moved the conversation {worst:.1} pt");
}

/// A whole turn after the send, as in the recording: the run starts (the
/// working line), the answer lands, the run ends; then a second send. The
/// prompt climbs to the top once per send and holds there; the transcript
/// never jumps on a frame.
#[test]
fn a_whole_turn_holds_the_prompt_and_never_jumps() {
    let s = real_chat();
    let mut rt = Runtime::new(s, crate::view, (1352., 878.), Env::new(true, 2.));
    let dt = 1. / 60.;
    let mut t = 0.;
    rt.frame(t);
    rt.update(|s, tx| {
        super::refresh_rows(s, "t1", tx);
    });
    for _ in 0..30 {
        t += dt;
        rt.frame(t);
    }
    let run = |rt: &mut Runtime<State>, t: &mut f64, frames: usize, key: &str| -> (f32, f32) {
        // (where the prompt ends, the largest one-frame step of any frame
        // after the first 40, by which the climb has landed).
        let mut last: Option<f32> = None;
        let mut step: f32 = 0.;
        for f in 0..frames {
            *t += dt;
            rt.frame(*t);
            let y = rt.presented(key).map(|(r, _)| r[1]);
            if let (Some(a), Some(b)) = (last, y)
                && f > 40
            {
                step = step.max((a - b).abs());
            }
            last = y.or(last);
        }
        (last.unwrap_or(f32::NAN), step)
    };
    let mut first = 0;
    rt.update(|s, tx| first = send_like(s, "yo testing", tx));
    let key = format!("row:pending:{first}");
    let (landed, _) = run(&mut rt, &mut t, 60, &key);
    let top = rt.presented("transcript:t1").expect("list").0[1];
    assert!(
        landed - top < 60.,
        "the prompt climbs to the top: {landed} vs {top}"
    );
    std::thread::sleep(super::view::CLIMB);
    let mut worst: f32 = 0.;
    let mut steps = Vec::new();
    // Accepted; the run starts.
    rt.update(move |s, tx| {
        let c = s.chat.as_mut().expect("chat");
        c.transcript.ack_pending_user(first);
        c.transcript.set_timeline(page(&["yo testing"]));
        c.fixture_run = Some("x0".into());
        super::refresh_rows(s, "t1", tx);
    });
    let (y, step) = run(&mut rt, &mut t, 60, &key);
    steps.push(("ack + run starts", y, step));
    // The answer lands and the run ends.
    rt.update(move |s, tx| {
        let c = s.chat.as_mut().expect("chat");
        c.transcript.set_timeline(page(&[
            "yo testing",
            "> Got it, I'm getting your messages.",
        ]));
        c.fixture_run = None;
        super::refresh_rows(s, "t1", tx);
    });
    let (y, step) = run(&mut rt, &mut t, 60, &key);
    steps.push(("answer + run ends", y, step));
    for (what, y, step) in &steps {
        eprintln!("{what}: prompt at {y:.1} (landed {landed:.1}), worst frame step {step:.1}");
        worst = worst.max((y - landed).abs()).max(*step);
    }
    assert!(
        worst <= 1.5,
        "the turn moved its prompt {worst:.1} pt: {steps:?}"
    );

    // The second send climbs again, from the held reserve of the first.
    let mut second = 0;
    rt.update(|s, tx| second = send_like(s, "nice thats cool", tx));
    let key2 = format!("row:pending:{second}");
    let (landed2, _) = run(&mut rt, &mut t, 60, &key2);
    eprintln!("second prompt lands at {landed2:.1}, list top {top:.1}");
    assert!(
        landed2 - top < 60.,
        "the second prompt climbs to the top: {landed2} vs {top}"
    );
}

/// The live timing: the run starts and the gateway acks a few frames into
/// the send's climb. The climb still lands the prompt at the top.
#[test]
fn updates_during_the_climb_do_not_stop_it() {
    for delay in [1usize, 3, 6, 12, 20] {
        let s = real_chat();
        let mut rt = Runtime::new(s, crate::view, (1352., 878.), Env::new(true, 2.));
        let dt = 1. / 60.;
        let mut t = 0.;
        rt.frame(t);
        rt.update(|s, tx| {
            super::refresh_rows(s, "t1", tx);
        });
        for _ in 0..30 {
            t += dt;
            rt.frame(t);
        }
        let mut first = 0;
        rt.update(|s, tx| first = send_like(s, "yo testing", tx));
        for _ in 0..delay {
            t += dt;
            rt.frame(t);
        }
        rt.update(|s, tx| {
            let c = s.chat.as_mut().expect("chat");
            c.fixture_run = Some("x0".into());
            super::refresh_rows(s, "t1", tx);
        });
        for _ in 0..3 {
            t += dt;
            rt.frame(t);
        }
        rt.update(move |s, tx| {
            let c = s.chat.as_mut().expect("chat");
            c.transcript.ack_pending_user(first);
            c.transcript.set_timeline(page(&["yo testing"]));
            super::refresh_rows(s, "t1", tx);
        });
        for _ in 0..90 {
            t += dt;
            rt.frame(t);
        }
        let top = rt.presented("transcript:t1").expect("list").0[1];
        let y = rt
            .presented(&format!("row:pending:{first}"))
            .expect("prompt")
            .0[1];
        eprintln!("delay {delay}: prompt {y:.1}, list top {top:.1}");
        assert!(
            y - top < 60.,
            "updates {delay} frames in stopped the climb at {y}"
        );
    }
}

/// An answer that lands whole after the climb and outgrows the room is
/// followed to its end (holding the prompt across that commit must not
/// leave the list off its end).
#[test]
fn a_late_answer_longer_than_the_room_is_followed() {
    let s = real_chat();
    let mut rt = Runtime::new(s, crate::view, (1352., 878.), Env::new(true, 2.));
    let dt = 1. / 60.;
    let mut t = 0.;
    rt.frame(t);
    rt.update(|s, tx| {
        super::refresh_rows(s, "t1", tx);
    });
    for _ in 0..30 {
        t += dt;
        rt.frame(t);
    }
    let mut first = 0;
    rt.update(|s, tx| first = send_like(s, "write a lot", tx));
    for _ in 0..60 {
        t += dt;
        rt.frame(t);
    }
    std::thread::sleep(super::view::CLIMB);
    let long = format!(
        "> {}",
        "A long paragraph of the answer that wraps. ".repeat(160)
    );
    rt.update(move |s, tx| {
        let c = s.chat.as_mut().expect("chat");
        c.transcript.ack_pending_user(first);
        c.transcript.set_timeline(page(&["write a lot", &long]));
        super::refresh_rows(s, "t1", tx);
    });
    for _ in 0..240 {
        t += dt;
        rt.frame(t);
    }
    let (list, _) = rt.presented("transcript:t1").expect("list");
    let (reply, _) = rt
        .presented("row:msg:a1")
        .expect("the reply's end is drawn");
    let (list_end, reply_end) = (list[1] + list[3], reply[1] + reply[3]);
    eprintln!("list end {list_end:.1}, reply end {reply_end:.1}");
    assert!(
        reply_end <= list_end + 1. && list_end - reply_end < 120.,
        "the list follows the answer's end: reply ends at {reply_end:.1}, list at {list_end:.1}"
    );
}
