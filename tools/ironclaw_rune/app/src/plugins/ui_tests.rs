//! The mounts driven through `rune::Runtime` over the plugin scenes.

use rune::{App, AppSpec, Env, Runtime};

use crate::state::State;

fn no_init() -> State {
    unreachable!("tests build the state themselves")
}

fn runtime(fill: impl FnOnce(&mut State)) -> Runtime<State> {
    let app = App::new(AppSpec::new(
        "ZLO test",
        (1280., 820.),
        no_init,
        crate::view,
    ));
    let mut s = State::new(crate::jobs::Rt::new(app.handle()));
    fill(&mut s);
    let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
    rt.frame(0.);
    rt.frame(1. / 60.);
    rt
}

fn frames(rt: &mut Runtime<State>, n: usize) {
    for i in 0..n {
        rt.frame((i + 2) as f64 / 60.);
    }
}

fn press(rt: &mut Runtime<State>, key: &str) {
    let (x, y) = rt.find(key).unwrap_or_else(|| panic!("{key} is drawn"));
    rt.pointer_down(x, y);
    rt.pointer_up(x, y);
    frames(rt, 4);
}

fn panel_key() -> super::MountKey {
    super::MountKey {
        extension_id: "linear".into(),
        slot_id: "issues".into(),
        thread_id: Some("t-pin".into()),
    }
}

#[test]
fn a_plugin_button_sends_its_action_for_the_open_thread() {
    let mut rt = runtime(|s| super::fixtures::panel(s, false));
    assert!(!rt.state.plugins.mounts[&panel_key()].busy);
    // The tree's fourth child is the button row; Mark done comes first.
    press(&mut rt, "plug-linear-issues-t-pin-3-0");
    assert!(
        rt.state.plugins.mounts[&panel_key()].busy,
        "the event is on its way; the buttons wait for the reply"
    );
}

#[test]
fn the_plugin_tab_sits_with_the_panes_own_and_badges_end_their_rows() {
    let mut rt = runtime(|s| {
        super::fixtures::panel(s, false);
        s.prefs.side_tab = "activity".into();
    });
    assert!(rt.find("badge-linear-state-t-pin").is_some());
    assert!(rt.find("badge-linear-state-t-needs").is_some());
    // A failed render draws no chip.
    assert!(rt.find("badge-linear-state-t-plain").is_none());
    press(&mut rt, "side-tab-plugin-linear-issues");
    assert_eq!(rt.state.prefs.side_tab, "plugin:linear/issues");
    assert!(
        rt.find("plug-linear-issues-t-pin").is_some(),
        "its tree shows"
    );
}

#[test]
fn a_plugin_tab_whose_slot_went_falls_back_to_activity() {
    let rt = runtime(|s| {
        super::fixtures::panel(s, false);
        s.plugins.set_slots(Vec::new());
    });
    assert!(rt.find("plug-linear-issues-t-pin").is_none());
    assert!(rt.find("side-tab-plugin-linear-issues").is_none());
}

#[test]
fn dont_allow_closes_the_sheet_and_leaves_the_plugin_waiting() {
    let mut rt = runtime(|s| crate::pages::extensions::fixtures::plugins_consent(s, false));
    assert!(rt.find("consent-allow").is_some());
    press(&mut rt, "consent-deny");
    assert!(rt.state.pages.extensions.consent.is_none());
    assert!(rt.find("consent-allow").is_none());
    assert!(rt.find("review-0").is_some(), "Review stays on the card");
}

#[test]
fn a_switch_shows_its_new_value_while_it_saves() {
    let mut rt = runtime(|s| crate::pages::extensions::fixtures::plugin_settings(s, false));
    crate::pages::extensions::plugin::save(
        &mut rt.state,
        "notify".into(),
        serde_json::Value::Bool(false),
    );
    let d = rt
        .state
        .pages
        .extensions
        .detail
        .as_ref()
        .expect("page open");
    assert_eq!(d.saving.as_deref(), Some("notify"));
    let crate::pages::extensions::plugin::SettingsLoad::Ready(list) = &d.settings else {
        panic!("settings loaded")
    };
    assert_eq!(
        list[0].effective(),
        &serde_json::json!(false),
        "flips at once"
    );
}
