//! The voice pill's waveform: what samples the mic level, when, and what
//! it costs. Real audio cannot run headless, so a test writes the level
//! the audio tap would (`Engine::level`) and drives the app's poll.

use rune::{App, AppSpec, Env, Runtime, Tx};

use super::Composer;
use super::fixtures::with_status;
use super::level::{BARS, MicLevel};
use crate::state::State;

fn no_init() -> State {
    unreachable!("tests build the state themselves")
}

/// The open chat (`t1`), not a fixture, with motion allowed by the
/// preference whatever the test machine's own setting is.
fn chat_state(s: &mut State) {
    crate::chat::fixtures::chat(s);
    s.fixture = false;
    s.prefs.reduce_motion = "off".into();
}

fn dictating(s: &mut State) {
    chat_state(s);
    with_status(s, &["click", "words"], Some(Composer::Chat("t1".into())));
}

fn level(s: &State) -> MicLevel {
    s.voice
        .engine
        .as_ref()
        .map(|e| e.level.clone())
        .expect("a voice engine")
}

/// The waveform asks for its next sample.
fn sampling(s: &State) -> bool {
    s.voice
        .engine
        .as_ref()
        .is_some_and(|e| e.level_next.is_some())
}

fn wakes(tx: &Tx) -> bool {
    !format!("{tx:?}").contains("wakes: []")
}

fn runtime(fill: fn(&mut State)) -> Runtime<State> {
    let app = App::new(AppSpec::new(
        "ZLO test",
        (1280., 820.),
        no_init,
        crate::view,
    ));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    fill(&mut s);
    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.poll = Some(crate::poll);
    rt.frame(0.);
    rt
}

#[test]
fn idle_voice_samples_nothing_and_asks_for_no_wake() {
    let app = App::new(AppSpec::new("ZLO test", (400., 300.), no_init, crate::view));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    chat_state(&mut s);
    // No engine at all, then an idle one.
    let mut tx = Tx::default();
    assert!(!super::poll(&mut s, &mut tx));
    assert!(!wakes(&tx), "no engine: no wake");
    with_status(&mut s, &[], Some(Composer::Chat("t1".into())));
    level(&s).set(0.8);
    for _ in 0..3 {
        let mut tx = Tx::default();
        assert!(!super::poll(&mut s, &mut tx));
        assert!(!wakes(&tx), "idle voice asks for no wake: {tx:?}");
    }
    assert_eq!(s.voice.bars, None);
}

#[test]
fn a_live_mic_is_sampled_on_its_tick_and_never_while_still() {
    let app = App::new(AppSpec::new("ZLO test", (400., 300.), no_init, crate::view));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    dictating(&mut s);
    assert_eq!(super::status(&s), super::Status::Dictating);
    level(&s).set(0.9);
    let mut tx = Tx::default();
    assert!(
        super::poll(&mut s, &mut tx),
        "the first sample lands at once"
    );
    let bars = s.voice.bars.expect("bars while dictating");
    assert!(bars[BARS / 2] >= 7, "{bars:?}");
    assert!(
        format!("{tx:?}").contains("wakes: [6"),
        "and asks for the next sample ~60 ms on: {tx:?}"
    );
    // Before the tick nothing is sampled again.
    level(&s).set(0.);
    assert!(!super::poll(&mut s, &mut Tx::default()));
    assert_eq!(s.voice.bars, Some(bars));

    // The battery saver (a backgrounded window) does not still a live mic:
    // push-to-talk from another app is when the waveform matters most.
    s.look.battery.set_active(false);
    assert!(sampling(&s), "a live mic samples under the saver");
    s.look.battery.set_active(true);

    // Reduce motion holds the still pose and stops the samples.
    s.prefs.reduce_motion = "on".into();
    let mut tx = Tx::default();
    super::poll(&mut s, &mut tx);
    assert_eq!(s.voice.bars, None);
    assert!(!sampling(&s));
    s.prefs.reduce_motion = "off".into();

    // A fixture never samples (snaps stay deterministic).
    s.fixture = true;
    let mut tx = Tx::default();
    super::poll(&mut s, &mut tx);
    assert!(s.voice.bars.is_none() && !sampling(&s));
}

/// The pill alone, as a composer places it, with only the voice poll:
/// what the waveform costs, apart from the rest of the window.
fn pill_view(s: &State, _: &mut rune::Cx<State>) -> rune::El {
    let k = crate::design::kit(&s.palette);
    let mut layer = Vec::new();
    rune::col().child(super::ui::composer_controls(
        s,
        &k,
        &mut layer,
        &Composer::Chat("t1".into()),
        None,
    ))
}

fn voice_poll(s: &mut State, _: f64, tx: &mut Tx) -> bool {
    super::poll(s, tx)
}

fn pill_runtime(fill: fn(&mut State)) -> Runtime<State> {
    let app = App::new(AppSpec::new("ZLO test", (400., 300.), no_init, pill_view));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    fill(&mut s);
    let mut rt = Runtime::new(s, pill_view, (400., 300.), Env::new(true, 1.));
    rt.poll = Some(voice_poll);
    rt.frame(0.);
    rt
}

struct Drawn {
    /// Frames drawn because the previous one was still animating.
    continuous: usize,
    /// Times the middle bar's drawn height changed.
    changes: usize,
    /// Level samples the poll took.
    samples: u32,
}

/// Run the host's frame loop for `secs` (a frame per wake, or the next
/// one while animating) with the mic level following `voice` as the tap
/// would write it.
fn run(rt: &mut Runtime<State>, secs: f64, voice: impl Fn(f64) -> f32) -> Drawn {
    let mic = level(&rt.state);
    let bar = |rt: &Runtime<State>| rt.presented("voice-bar-2").map(|(r, _)| r[3]);
    let mut out = Drawn {
        continuous: 0,
        changes: 0,
        samples: 0,
    };
    let mut clock = 1. / 120.;
    let mut animating = rt.frame(1. / 120.);
    let mut last = bar(rt);
    let end = clock + secs;
    while clock < end {
        mic.set(voice(clock));
        let dt = if animating {
            out.continuous += 1;
            1. / 120.
        } else {
            match rt.wake_deadline() {
                Some(due) => (due - clock).max(1. / 120.),
                None => break,
            }
        };
        clock += dt;
        animating = rt.frame(dt);
        let now = bar(rt);
        out.changes += usize::from(now != last);
        last = now;
    }
    out.samples = rt.state.voice.engine.as_ref().map_or(0, |e| e.level_tick);
    out
}

#[test]
fn the_waveform_steps_at_the_sample_rate_not_the_display_rate() {
    let speech = |t: f64| (0.5 + 0.5 * (t * 9.).sin() as f32).clamp(0., 1.);
    let mut rt = pill_runtime(dictating);
    assert!(
        rt.presented("voice-bar-2").is_some(),
        "the pill's bars are drawn"
    );
    let d = run(&mut rt, 3., speech);
    eprintln!(
        "waveform over 3 s: {} samples, {} bar changes, {} display-rate frames",
        d.samples, d.changes, d.continuous
    );
    assert_eq!(
        d.continuous, 0,
        "the bars never spring at the display's rate"
    );
    assert!(
        (44..=52).contains(&d.samples),
        "~16.7 samples a second: {}",
        d.samples
    );
    assert!(
        d.changes >= 24 && d.changes <= d.samples as usize,
        "the bars follow the voice, one step per sample at most: {}",
        d.changes
    );

    // Under Reduce motion the pill holds its pose and samples nothing.
    let mut rt = pill_runtime(|s| {
        dictating(s);
        s.prefs.reduce_motion = "on".into();
    });
    let d = run(&mut rt, 3., speech);
    assert_eq!(
        (d.samples, d.changes, d.continuous),
        (0, 0, 0),
        "a still pose"
    );
}

#[test]
fn the_chat_composer_draws_the_waveform_in_place_of_the_dot() {
    let mut rt = runtime(dictating);
    rt.frame(1. / 60.);
    let (wave, _) = rt
        .presented("voice-wave")
        .expect("the waveform in the composer");
    let (pill, _) = rt.presented("voice-pill").expect("the pill");
    assert!(
        (wave[3] - 12.).abs() < 0.5 && wave[2] < 13.,
        "a 12 pt glyph: {wave:?}"
    );
    assert!(
        (pill[3] - 26.).abs() < 0.5,
        "the pill keeps its height: {pill:?}"
    );
    for i in 0..BARS {
        assert!(rt.presented(&format!("voice-bar-{i}")).is_some());
    }
}

#[test]
fn finishing_settles_the_bars_to_rest_and_stops_sampling() {
    let app = App::new(AppSpec::new("ZLO test", (400., 300.), no_init, crate::view));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    chat_state(&mut s);
    // Push-to-talk held, words heard, released: the recognizer finalizes.
    with_status(
        &mut s,
        &["hold", "words", "release"],
        Some(Composer::Chat("t1".into())),
    );
    let finishing = s
        .voice
        .engine
        .as_ref()
        .is_some_and(|e| matches!(e.coord.mode(), super::coordinator::Mode::Finishing { .. }));
    assert!(finishing, "the fixture reached Finishing");
    level(&s).set(0.9);
    let mut heights = Vec::new();
    for _ in 0..20 {
        // Step past each tick.
        if let Some(e) = s.voice.engine.as_mut() {
            e.level_next = Some(0.);
        }
        super::poll(&mut s, &mut Tx::default());
        heights.push(s.voice.bars.expect("bars")[BARS / 2]);
        if !sampling(&s) {
            break;
        }
    }
    assert!(
        heights.windows(2).all(|w| w[1] <= w[0]),
        "the bars only fall: {heights:?}"
    );
    assert_eq!(heights.last(), Some(&0), "and rest flat: {heights:?}");
    assert!(
        heights.len() <= 12,
        "within about half a second: {heights:?}"
    );
}

/// One poll on a forced-due tick: whether it sampled and whether it asked
/// for the next sample.
fn tick(s: &mut State) -> (bool, bool) {
    if let Some(e) = s.voice.engine.as_mut()
        && e.level_next.is_some()
    {
        e.level_next = Some(0.);
    }
    let mut tx = Tx::default();
    super::poll(s, &mut tx);
    (s.voice.bars.is_some(), sampling(s))
}

#[test]
fn a_composer_off_screen_samples_nothing() {
    use crate::state::Page;
    let app = App::new(AppSpec::new("ZLO test", (400., 300.), no_init, crate::view));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    dictating(&mut s);
    level(&s).set(0.7);
    assert_eq!(tick(&mut s), (true, true), "dictating in the open chat");

    // Settings over the chat: the chat and its voice session are kept,
    // but nothing is on screen to animate.
    s.page = Page::Settings;
    let mut tx = Tx::default();
    super::poll(&mut s, &mut tx);
    assert_eq!(s.voice.bars, None);
    assert!(!sampling(&s), "no sample asked for under Settings");
    assert!(
        !format!("{tx:?}").contains("60ms"),
        "and no 60 ms wake: {tx:?}"
    );
    assert_eq!(
        super::status(&s),
        super::Status::Dictating,
        "still dictating"
    );

    // Back to the chat: sampling resumes.
    s.page = Page::Chat;
    assert_eq!(tick(&mut s), (true, true), "the chat is back on screen");

    // The new-thread screen over the chat hides it too.
    s.page = Page::Draft;
    assert_eq!(tick(&mut s), (false, false));
}

#[test]
fn a_side_chat_samples_only_while_its_pane_shows_it() {
    let app = App::new(AppSpec::new("ZLO test", (400., 300.), no_init, crate::view));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    chat_state(&mut s);
    crate::chat::fixtures::with_side_chat(&mut s, "t-side");
    s.prefs.side_open = true;
    s.prefs.side_tab = "side-chat".into();
    with_status(
        &mut s,
        &["click", "words"],
        Some(Composer::Chat("t-side".into())),
    );
    level(&s).set(0.7);
    assert_eq!(tick(&mut s), (true, true), "the pane shows the side chat");

    s.prefs.side_open = false;
    assert_eq!(tick(&mut s), (false, false), "pane closed: nothing samples");

    s.prefs.side_open = true;
    s.prefs.side_tab = "activity".into();
    assert_eq!(tick(&mut s), (false, false), "another tab: nothing samples");

    s.prefs.side_tab = "side-chat".into();
    assert_eq!(tick(&mut s), (true, true), "back on the side chat");
}

#[test]
fn a_closed_quick_composer_samples_nothing() {
    let app = App::new(AppSpec::new("ZLO test", (400., 300.), no_init, crate::view));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    chat_state(&mut s);
    with_status(&mut s, &["click", "words"], Some(Composer::Quick));
    level(&s).set(0.7);
    assert!(!s.workspace.quick.is_open());
    assert_eq!(tick(&mut s), (false, false));
}
