//! The reserve while an answer streams (GPUI held the newest prompt at the
//! top of the list for the whole turn): the prompt must not creep while the
//! answer below it grows.

use std::rc::Rc;

use ironclaw_gpui_client::transcript::Row;
use rune::{App, AppSpec, Env, Runtime};

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
    rt.update(|s, tx| sync_items(s, "t1", tx));
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
    rt.update(|s, tx| sync_items(s, "t1", tx));
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
