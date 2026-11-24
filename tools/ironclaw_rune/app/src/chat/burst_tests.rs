//! A fast stream against a long thread: however many deltas land between
//! two frames, the UI thread rebuilds the rows and the view at most once
//! per frame.
//!
//! The bench (`stream_burst_bench`, ignored; run it with `--ignored
//! --nocapture`) prints UI-thread time per frame for a synthetic 2000
//! deltas/s burst at 120 Hz; the test pins the per-frame bound.

use std::cell::Cell;
use std::time::{Duration, Instant};

use rune::{Cx, El, Env, Runtime};
use zlo_client::StreamMessage;
use zlo_client::wire::{StreamFrame, TimelinePage};

use crate::state::State;

const RUN: &str = "6f1c1c0e-0000-4000-8000-0000000000ff";

thread_local! {
    static VIEWS: Cell<u64> = const { Cell::new(0) };
    static POLL_NS: Cell<u64> = const { Cell::new(0) };
    static VIEW_NS: Cell<u64> = const { Cell::new(0) };
}

fn timed_poll(s: &mut State, clock: f64, tx: &mut rune::Tx) -> bool {
    let at = Instant::now();
    let out = crate::poll(s, clock, tx);
    POLL_NS.with(|v| v.set(v.get() + at.elapsed().as_nanos() as u64));
    out
}

fn counted_view(s: &State, cx: &mut Cx<State>) -> El {
    VIEWS.with(|v| v.set(v.get() + 1));
    let at = Instant::now();
    let out = crate::view(s, cx);
    VIEW_NS.with(|v| v.set(v.get() + at.elapsed().as_nanos() as u64));
    out
}

/// A thread with `turns` settled turns, open in the window.
fn long_thread(turns: usize) -> State {
    let mut s = super::fixtures::test_state();
    s.page = crate::state::Page::Chat;
    s.conn = crate::state::Conn::Ready;
    let mut chat = super::ChatState::new("t1".into());
    let mut messages = Vec::new();
    for n in 0..turns {
        let run = format!("6f1c1c0e-0000-4000-8000-{n:012}");
        messages.push(serde_json::json!({
            "message_id": format!("u{n}"), "sequence": 2 * n, "kind": "user",
            "status": "accepted", "turn_run_id": run,
            "content": format!("Question {n}: how does the stream reach the view?"),
        }));
        messages.push(serde_json::json!({
            "message_id": format!("a{n}"), "sequence": 2 * n + 1, "kind": "assistant",
            "status": "finalized", "turn_run_id": run,
            "content": "A settled answer with **some markdown**, a list:\n\n- one\n- two\n\n\
                        and a paragraph long enough to wrap over a couple of lines in the \
                        transcript column of a normal window.",
        }));
    }
    let page: TimelinePage = serde_json::from_value(serde_json::json!({
        "thread": { "thread_id": "t1" }, "messages": messages
    }))
    .expect("timeline decodes");
    chat.transcript.set_timeline(page);
    s.chat = Some(chat);
    s
}

fn delta(body: &str) -> StreamMessage {
    let frame: StreamFrame = serde_json::from_value(serde_json::json!({
        "cursor": "c", "type": "projection_update",
        "state": { "thread_id": "t1", "items": [
            { "text": { "id": format!("text:{RUN}:0"), "run_id": RUN,
                        "body": body, "narration": false } }
        ]}
    }))
    .expect("frame decodes");
    StreamMessage::Frame(Box::new(frame))
}

struct Burst {
    frames: u32,
    rows_rebuilt: u64,
    views: u64,
    ui: Vec<Duration>,
}

/// `per_frame` deltas land as separate updates (the UI drained between
/// each), then one frame runs; `frames` times.
fn burst(turns: usize, frames: u32, per_frame: u32) -> Burst {
    let mut rt = Runtime::new(
        long_thread(turns),
        counted_view,
        (1000., 760.),
        Env::new(true, 1.),
    );
    rt.poll = Some(timed_poll);
    rt.update(|s, tx| {
        super::refresh_rows(s, "t1", tx);
    });
    // `Runtime::frame` takes the frame's step, not the clock: a growing
    // "step" is a coarse headless catch-up that polls (and may rebuild the
    // view) at every timer deadline inside it, which is not one frame.
    let dt = 1. / 120.;
    for _ in 0..10 {
        rt.frame(dt);
    }
    let generation = rt.state.chat.as_ref().map(|c| c.stream_gen).expect("chat");
    let rows_before = rt.state.chat.as_ref().map(|c| c.rows_gen).expect("chat");
    let views_before = VIEWS.with(Cell::get);
    // Decoding runs on the network runtime in the app: not timed here.
    let mut body = String::new();
    let mut deltas = (0..frames * per_frame)
        .map(|_| {
            body.push_str(" word");
            delta(&body)
        })
        .collect::<Vec<_>>()
        .into_iter();
    let mut ui = Vec::new();
    let mut updates = Duration::ZERO;
    for _ in 0..frames {
        let start = Instant::now();
        for batch in deltas.by_ref().take(per_frame as usize) {
            let batch = vec![batch];
            rt.update_if(move |s, tx| super::on_stream(s, "t1", generation, batch, tx));
        }
        updates += start.elapsed();
        rt.frame(dt);
        ui.push(start.elapsed());
    }
    eprintln!(
        "updates total {updates:.2?} poll {:.2?} view {:.2?}",
        Duration::from_nanos(POLL_NS.with(|v| v.replace(0))),
        Duration::from_nanos(VIEW_NS.with(|v| v.replace(0)))
    );
    let rows_rebuilt = rt.state.chat.as_ref().map(|c| c.rows_gen).expect("chat") - rows_before;
    Burst {
        frames,
        rows_rebuilt,
        views: VIEWS.with(Cell::get) - views_before,
        ui,
    }
}

#[test]
fn a_burst_of_deltas_rebuilds_rows_at_most_once_per_frame() {
    let b = burst(40, 30, 17);
    assert!(
        b.rows_rebuilt <= u64::from(b.frames),
        "{} row rebuilds over {} frames",
        b.rows_rebuilt,
        b.frames
    );
    assert!(
        b.views <= u64::from(b.frames),
        "{} views over {} frames",
        b.views,
        b.frames
    );
    // The stream still shows: the reply's rows were rebuilt.
    assert!(b.rows_rebuilt > 0);
}

#[test]
#[ignore = "bench: run with --ignored --nocapture"]
fn stream_burst_bench() {
    // 2000 deltas/s at 120 Hz: about 17 per frame, for one second.
    // `BURST_REPEAT=n` loops the long-thread burst n more times (to profile).
    let repeat: usize = std::env::var("BURST_REPEAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let cases = [(50, 0), (50, 17), (300, 0), (300, 17)]
        .into_iter()
        .chain(std::iter::repeat_n((300, 17), repeat));
    for (turns, per_frame) in cases {
        let b = burst(turns, 120, per_frame);
        let total: Duration = b.ui.iter().sum();
        let mut sorted = b.ui.clone();
        sorted.sort();
        let p50 = sorted[sorted.len() / 2];
        let p95 = sorted[sorted.len() * 95 / 100];
        let max = sorted[sorted.len() - 1];
        eprintln!(
            "turns={turns} deltas/frame={per_frame} frames={} rows_rebuilt={} views={} ui/frame mean={:.2?} p50={p50:.2?} p95={p95:.2?} max={max:.2?}",
            b.frames,
            b.rows_rebuilt,
            b.views,
            total / b.frames,
        );
    }
}

/// Input queued for the frame goes first: the stream's fold waits for the
/// next frame (bounded), so a key or click never waits behind a rebuild of
/// the whole thread.
#[test]
fn a_queued_input_puts_the_stream_fold_off_by_a_frame() {
    let mut rt = Runtime::new(
        long_thread(40),
        counted_view,
        (1000., 760.),
        Env::new(true, 1.),
    );
    rt.update(|s, tx| {
        super::refresh_rows(s, "t1", tx);
    });
    rt.frame(0.);
    let generation = rt.state.chat.as_ref().map(|c| c.stream_gen).expect("chat");
    let rows = |rt: &Runtime<State>| rt.state.chat.as_ref().map(|c| c.rows_gen).expect("chat");
    let before = rows(&rt);
    rt.update_if(move |s, tx| super::on_stream(s, "t1", generation, vec![delta("Hello")], tx));
    assert_eq!(rows(&rt), before, "the burst alone rebuilds nothing");

    rt.update_if(|s, tx| super::poll_chat(s, "t1", true, tx));
    assert_eq!(rows(&rt), before, "input waiting: the fold waits");
    assert!(
        rt.state
            .chat
            .as_ref()
            .is_some_and(|c| c.rows_stale.is_some()),
        "still owed to the next frame"
    );

    rt.update_if(|s, tx| super::poll_chat(s, "t1", false, tx));
    assert_eq!(rows(&rt), before + 1, "the next frame folds it");

    // Input every frame (a long scroll) delays the fold by a bounded time.
    rt.update_if(move |s, tx| {
        super::on_stream(s, "t1", generation, vec![delta("Hello there")], tx)
    });
    rt.update(|s, _| {
        if let Some(c) = s.chat.as_mut() {
            c.rows_stale = Some(Instant::now() - super::INPUT_YIELD_MAX);
        }
    });
    rt.update_if(|s, tx| super::poll_chat(s, "t1", true, tx));
    assert_eq!(
        rows(&rt),
        before + 2,
        "waited long enough: folded under input"
    );
}

#[test]
fn work_yields_to_input_only_while_it_is_fresh() {
    let now = Instant::now();
    assert!(super::yields_to_input(true, now, now));
    assert!(!super::yields_to_input(false, now, now));
    assert!(!super::yields_to_input(
        true,
        now - super::INPUT_YIELD_MAX,
        now
    ));
}

/// While the pacer holds a half-typed word back, its steps reveal nothing:
/// such a step must not report a change (which rebuilds the whole view on
/// every frame of the hold).
#[test]
fn a_pacer_step_that_reveals_nothing_is_not_a_change() {
    let mut rt = Runtime::new(
        long_thread(4),
        counted_view,
        (1000., 760.),
        Env::new(true, 1.),
    );
    rt.update(|s, tx| {
        super::refresh_rows(s, "t1", tx);
    });
    rt.frame(0.);
    let generation = rt.state.chat.as_ref().map(|c| c.stream_gen).expect("chat");
    // "partial" has no space after it yet: it is held for the hold window.
    rt.update_if(move |s, tx| super::on_stream(s, "t1", generation, vec![delta("ok partial")], tx));
    rt.update_if(|s, tx| super::poll_chat(s, "t1", false, tx));
    let items = |rt: &Runtime<State>| {
        rt.state
            .chat
            .as_ref()
            .map(|c| std::rc::Rc::as_ptr(&c.items))
            .expect("chat")
    };
    let pacing = |rt: &Runtime<State>| rt.state.chat.as_ref().is_some_and(|c| c.pacing);
    let (mut idle_steps, mut idle_changes) = (0, 0);
    let until = Instant::now() + Duration::from_millis(600);
    while pacing(&rt) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(17));
        let before = items(&rt);
        let changed = rt.update_if(|s, tx| super::poll_chat(s, "t1", false, tx));
        if items(&rt) == before {
            idle_steps += 1;
            idle_changes += usize::from(changed);
        }
    }
    eprintln!("hold: {idle_steps} steps revealed nothing, {idle_changes} reported a change");
    assert!(
        idle_steps >= 1,
        "the hold window produced no idle step to check"
    );
    // (A working-line seconds repaint may coincide with one of them.)
    assert!(
        idle_changes < idle_steps,
        "{idle_changes} of {idle_steps} pacer steps that revealed nothing reported a change"
    );
}

#[test]
fn the_pacer_follows_reduce_motion_like_the_rest_of_the_app() {
    let system_on = || true;
    let system_off = || false;
    // "System" (the default) follows macOS's Reduce motion.
    assert!(super::pacer_off("system", false, system_on));
    assert!(!super::pacer_off("system", false, system_off));
    // "On" always, "Off" never — whatever macOS says.
    assert!(super::pacer_off("on", false, system_off));
    assert!(!super::pacer_off("off", false, system_on));
    // Nobody watches a background window type.
    assert!(super::pacer_off("off", true, system_off));
    // The idle battery saver only slows the tick.
    assert!(super::pace_tick(true) > super::pace_tick(false));
}
