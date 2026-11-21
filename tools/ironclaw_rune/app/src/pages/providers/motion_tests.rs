//! Providers motion that has to keep playing: cards and tiles keep their
//! identity (provider id) and glide when the list re-sorts, the tabs
//! crossfade, the copy glyph confirms, and nothing draws at rest. The
//! runtime helpers here are shared with the other settings pages' motion
//! tests (Extensions, Profiles, Operator).

use rune::{App, AppSpec, Env, Runtime, Tx};

use super::Tab;
use crate::state::State;

fn no_init() -> State {
    unreachable!("tests build the state themselves")
}

/// The whole app at 1280×820 on `fill`'s fixture, one frame in.
pub(crate) fn runtime(fill: fn(&mut State)) -> Runtime<State> {
    runtime_sized(fill, (1280., 820.))
}

/// [`runtime`] in a window of `size` (a tall one shows a long page whole:
/// Rune lands motion off screen at once).
pub(crate) fn runtime_sized(fill: fn(&mut State), size: (f32, f32)) -> Runtime<State> {
    let app = App::new(AppSpec::new("ZLO test", size, no_init, crate::view));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    fill(&mut s);
    let mut rt = Runtime::new(s, crate::view, size, Env::new(true, 1.));
    rt.frame(0.);
    rt.frame(1. / 60.);
    rt
}

pub(crate) fn frames(rt: &mut Runtime<State>, n: usize) {
    for _ in 0..n {
        rt.frame(1. / 60.);
    }
}

pub(crate) fn rect(rt: &Runtime<State>, key: &str) -> [f32; 4] {
    rt.presented(key)
        .unwrap_or_else(|| panic!("{key} is drawn"))
        .0
}

pub(crate) fn opacity(rt: &Runtime<State>, key: &str) -> f32 {
    rt.presented(key)
        .unwrap_or_else(|| panic!("{key} is drawn"))
        .1
}

/// Commit `f` as the person's own action (or its reply) would: animated.
pub(crate) fn act(rt: &mut Runtime<State>, f: impl FnOnce(&mut State) + 'static) {
    rt.update(move |s, tx| {
        tx.animated();
        f(s);
    });
}

/// Settle every track, then check the page asks for no frame and sets no
/// timer: an idle page draws nothing.
pub(crate) fn assert_at_rest(rt: &mut Runtime<State>, what: &str) {
    frames(rt, 120);
    assert!(!rt.frame(1. / 60.), "{what} asks for frames at rest");
    assert_eq!(rt.wake_deadline(), None, "{what} keeps a timer at rest");
}

/// `mid` lies strictly between `from` and `to` on axis `ix` (0 x, 1 y).
pub(crate) fn between(from: [f32; 4], mid: [f32; 4], to: [f32; 4], ix: usize) -> bool {
    let (lo, hi) = if from[ix] < to[ix] {
        (from[ix], to[ix])
    } else {
        (to[ix], from[ix])
    };
    mid[ix] > lo + 1. && mid[ix] < hi - 1.
}

#[test]
fn a_promoted_provider_glides_to_the_top() {
    let mut rt = runtime(super::fixtures::providers);
    let from = rect(&rt, "provider-zai");
    let anthropic = rect(&rt, "provider-anthropic");
    // set_active's reply: Z.AI becomes the active provider.
    act(&mut rt, |s| {
        if let Some(llm) = s.pages.providers.llm.as_mut() {
            for pv in &mut llm.providers {
                pv.active = pv.id == "zai";
            }
        }
    });
    frames(&mut rt, 4);
    let mid = rect(&rt, "provider-zai");
    frames(&mut rt, 90);
    let to = rect(&rt, "provider-zai");
    assert!((to[1] - anthropic[1]).abs() < 0.5, "it lands on top");
    assert!(
        between(from, mid, to, 1),
        "it glides: {from:?} → {mid:?} → {to:?}"
    );
}

#[test]
fn a_tile_keeps_its_identity_as_the_grid_resorts() {
    let mut rt = runtime_sized(super::fixtures::catalog, (1280., 1600.));
    let from = rect(&rt, "tile-mistral");
    let openrouter = rect(&rt, "tile-openrouter");
    // save_key's reply: OpenAI is connected, so its tile leaves the grid
    // and the tiles after it close the gap.
    act(&mut rt, |s| {
        if let Some(llm) = s.pages.providers.llm.as_mut()
            && let Some(pv) = llm.providers.iter_mut().find(|p| p.id == "openai")
        {
            pv.api_key_set = true;
        }
    });
    frames(&mut rt, 4);
    let mid = rect(&rt, "tile-mistral");
    frames(&mut rt, 90);
    let to = rect(&rt, "tile-mistral");
    assert!(
        (to[0] - openrouter[0]).abs() < 0.5,
        "Mistral takes OpenRouter's column: {to:?} vs {openrouter:?}"
    );
    assert!(
        between(from, mid, to, 0),
        "it glides: {from:?} → {mid:?} → {to:?}"
    );
    assert!(
        rt.presented("tile-openai").is_none(),
        "the connected tile left"
    );
    assert!(
        rt.presented("provider-openai").is_some(),
        "its card joined Connected"
    );
}

#[test]
fn the_tabs_crossfade_in_place() {
    let mut rt = runtime(super::fixtures::catalog);
    let accounts = rect(&rt, "providers-tab-accounts");
    act(&mut rt, |s| s.pages.providers.tab = Tab::Cli);
    frames(&mut rt, 3);
    let cli = opacity(&rt, "providers-tab-cli");
    assert!(cli > 0. && cli < 1., "the CLI list fades in: {cli}");
    assert!(
        (rect(&rt, "providers-tab-cli")[1] - accounts[1]).abs() < 0.5,
        "it takes the accounts grid's place, not a place below it"
    );
    frames(&mut rt, 60);
    assert!(rt.presented("providers-tab-accounts").is_none());
    assert!((opacity(&rt, "providers-tab-cli") - 1.).abs() < 1e-3);
}

#[test]
fn copying_a_command_shows_a_check_on_its_row_only() {
    let mut rt = runtime(super::fixtures::cli);
    let mut tx = Tx::default();
    super::copy_command(&mut rt.state, &mut tx, "codex".into(), "codex login");
    assert!(rt.state.pages.providers.copied("codex"));
    assert!(
        !rt.state.pages.providers.copied("claude"),
        "other rows stay"
    );
}

#[test]
fn the_providers_page_draws_nothing_at_rest() {
    for fill in [
        super::fixtures::providers as fn(&mut State),
        super::fixtures::catalog,
        super::fixtures::cli,
    ] {
        let mut rt = runtime(fill);
        assert_at_rest(&mut rt, "Providers");
    }
}

/// Frames a host draws over `secs`: every frame while the runtime asks for
/// continuous frames, else one at each wake deadline.
pub(crate) fn frames_over(rt: &mut Runtime<State>, secs: f64) -> usize {
    let (mut t, mut frames, mut continuous) = (0., 0, rt.frame(0.));
    while t < secs {
        let dt = if continuous {
            1. / 120.
        } else {
            match rt.wake_deadline() {
                Some(due) => (due - t).max(1. / 120.),
                None => break,
            }
        };
        t += dt;
        continuous = rt.frame(dt);
        frames += 1;
    }
    frames
}
