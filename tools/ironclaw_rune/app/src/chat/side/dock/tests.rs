//! The docks' layout and motion, over `rune::Runtime`.

use rune::{Env, Runtime};

use crate::design::panels;

use super::super::{Dock, Tab, dock_drawn, poll, toggle_right_pane, toggle_work_panel};
use crate::state::State;

fn blank(_: &State, _: &mut rune::Cx<State>) -> rune::El {
    rune::el()
}

/// While the backend is remote, the git and shell tabs say where they
/// run, Files stays (read through the gateway), and no folder on this
/// Mac is adopted for the thread.
#[test]
fn remote_backend_disables_the_local_only_tabs() {
    use super::super::set_folder;
    let off: Vec<Tab> = [
        Tab::Changes,
        Tab::Files,
        Tab::Outputs,
        Tab::History,
        Tab::Console,
    ]
    .into_iter()
    .filter(|t| t.local_only())
    .collect();
    assert_eq!(off, vec![Tab::Changes, Tab::History, Tab::Console]);
    assert_eq!(
        super::empty::remote_line(Tab::Console, "studio"),
        "Console: Runs on studio — not available remotely yet"
    );

    let mut s = crate::chat::fixtures::test_state();
    s.chat = Some(crate::chat::ChatState::new("t1".into()));
    s.onboarding.computer = crate::onboarding::gateway::Computer::remote("env-studio", "Studio");
    set_folder(
        &mut s,
        std::path::PathBuf::from("/Users/you/code/zlo"),
        false,
    );
    assert!(s.chat.as_ref().is_some_and(|c| c.side.folder.is_none()));
}

/// The titlebar's toggles hold their place whichever columns are open:
/// a click that opens or closes one never moves another out from under
/// the pointer. Each column's header stops short of the toggles.
#[test]
fn the_titlebar_toggles_never_move() {
    const TOGGLES: [&str; 3] = [
        "side-session-toggle",
        "side-work-toggle",
        "side-pane-toggle",
    ];
    let mut s = crate::chat::fixtures::test_state();
    crate::scenes::chat(&mut s);
    s.prefs.side_open = false;
    s.prefs.work_open = false;
    let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
    let mut t = 0.;
    let mut settle = |rt: &mut Runtime<State>| {
        for _ in 0..90 {
            t += 1. / 60.;
            rt.frame(t);
            rt.update(|s, tx| {
                poll(s, t, tx);
            });
        }
    };
    let place = |rt: &Runtime<State>| -> Vec<[f32; 2]> {
        TOGGLES
            .iter()
            .map(|k| {
                let (r, _) = rt.presented(k).expect("toggle drawn");
                [r[0], r[1]]
            })
            .collect()
    };
    settle(&mut rt);
    let closed = place(&rt);
    assert!(
        closed.windows(2).all(|w| w[0][0] < w[1][0]),
        "session, work, pane from left to right: {closed:?}"
    );
    for (step, flip) in [
        ("pane open", toggle_right_pane as fn(&mut State)),
        ("both open", toggle_work_panel),
        ("work only", toggle_right_pane),
        ("both closed", toggle_work_panel),
    ] {
        rt.update(|s, _| flip(s));
        settle(&mut rt);
        assert_eq!(place(&rt), closed, "{step}: the toggles moved");
        if let Some((tabs, _)) = rt.presented("side-side-header") {
            let (pane, _) = rt.presented("side-session-toggle").expect("drawn");
            assert!(
                tabs[0] + tabs[2] <= pane[0],
                "{step}: the pane's header runs under the toggles"
            );
        }
    }
    // The tabs and toggles are buttons the keyboard reaches.
    rt.update(|s, _| toggle_right_pane(s));
    settle(&mut rt);
    let elements = rt.elements();
    for key in TOGGLES.iter().copied().chain([
        "side-tab-activity",
        "side-tab-browser",
        "side-tab-agents",
        "side-tab-routines",
        "side-tab-machines",
    ]) {
        let e = elements
            .iter()
            .find(|e| e.key.as_deref() == Some(key))
            .expect("drawn");
        assert!(e.focusable, "{key} is not focusable");
    }
}

/// Both docks' segmented tab switchers draw every tab pill with generous
/// width inside their own dock bounds and with zero overlap.
#[test]
fn both_docks_draw_non_overlapping_labeled_tab_pills() {
    let mut s = crate::chat::fixtures::test_state();
    crate::scenes::chat(&mut s);
    s.prefs.work_open = true;
    s.prefs.side_open = true;
    let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
    let mut t = 0.;
    for _ in 0..90 {
        t += 1. / 60.;
        rt.frame(t);
        rt.update(|s, tx| {
            poll(s, t, tx);
        });
    }
    let (work_box, _) = rt.presented("side-work-pane").expect("work pane drawn");
    let (side_box, _) = rt.presented("side-side-pane").expect("side pane drawn");
    // Each dock: five pills, three on the first row and two below.
    let docks = [
        (
            work_box,
            ["side-tab-changes", "side-tab-files", "side-tab-outputs"],
            ["side-tab-history", "side-tab-console"],
        ),
        (
            side_box,
            ["side-tab-activity", "side-tab-browser", "side-tab-agents"],
            ["side-tab-routines", "side-tab-machines"],
        ),
    ];
    for (pane, top, bottom) in docks {
        let row1: Vec<[f32; 4]> = top.iter().map(|k| rt.presented(k).expect(k).0).collect();
        let row2: Vec<[f32; 4]> = bottom.iter().map(|k| rt.presented(k).expect(k).0).collect();
        for row in [&row1, &row2] {
            for r in row.iter() {
                assert!(r[2] >= 70., "pill should be generously wide: {r:?}");
                assert!(
                    r[0] >= pane[0] && r[0] + r[2] <= pane[0] + pane[2] + 0.5,
                    "pill {r:?} escapes its pane {pane:?}"
                );
                assert!(
                    (r[2] - row[0][2]).abs() <= 1.0 && (r[1] - row[0][1]).abs() <= 0.5,
                    "a row's pills share width and line: {row:?}"
                );
            }
            for pair in row.windows(2) {
                assert!(
                    pair[0][0] + pair[0][2] <= pair[1][0] + 0.5,
                    "pills overlap: {row:?}"
                );
            }
        }
        assert!(
            row1[0][1] + row1[0][3] <= row2[0][1] + 0.5,
            "the 2 rows do not overlap"
        );
    }

    // Widening the side dock collapses its 5 tool pills onto a single row.
    rt.update(|s, _| {
        if let Some(x) = super::side_mut(s) {
            x.wide[Dock::Side.ix()] = true;
        }
    });
    for _ in 0..90 {
        t += 1. / 60.;
        rt.frame(t);
        rt.update(|s, tx| {
            poll(s, t, tx);
        });
    }
    let wide_pills: Vec<[f32; 4]> = [
        "side-tab-activity",
        "side-tab-browser",
        "side-tab-agents",
        "side-tab-routines",
        "side-tab-machines",
    ]
    .into_iter()
    .map(|k| rt.presented(k).expect(k).0)
    .collect();
    for pair in wide_pills.windows(2) {
        assert!(
            (pair[0][1] - pair[1][1]).abs() <= 0.5,
            "wide pills should share one row: {wide_pills:?}"
        );
        assert!(
            pair[0][0] + pair[0][2] <= pair[1][0] + 0.5,
            "wide pills overlap: {wide_pills:?}"
        );
    }
}

/// The raised highlight sits exactly on the active pill in both docks,
/// at rest, and glides there on a switch: mid-switch it is between the
/// two pills rather than snapped onto either.
#[test]
fn the_active_tab_highlight_sits_on_its_pill_and_glides_on_a_switch() {
    let mut s = crate::chat::fixtures::test_state();
    crate::scenes::chat(&mut s);
    s.prefs.work_open = true;
    s.prefs.side_open = true;
    let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
    let mut t = 0.;
    let mut run = |rt: &mut Runtime<State>, frames: usize| {
        for _ in 0..frames {
            t += 1. / 60.;
            rt.frame(1. / 60.);
            rt.update(|s, tx| {
                poll(s, t, tx);
            });
        }
    };
    run(&mut rt, 90);
    let on = |rt: &Runtime<State>, dock: &str, tab: &str| {
        let (glide, _) = rt
            .presented(&format!("side-{dock}-tab-glide"))
            .expect("highlight drawn");
        let (pill, _) = rt.presented(&format!("side-tab-{tab}")).expect("pill");
        glide
            .iter()
            .zip(pill.iter())
            .all(|(a, b)| (a - b).abs() <= 0.5)
    };
    for (key, dock) in [("work", Dock::Work), ("side", Dock::Side)] {
        let tab = super::super::dock_tab(&rt.state, dock);
        assert!(
            on(&rt, key, tab.id()),
            "{key}: the highlight is off {tab:?}"
        );
    }
    // Switch the right pane to a pill on the other row.
    rt.update(|s, _| super::super::show(s, Tab::Machines));
    run(&mut rt, 4);
    let (mid, _) = rt.presented("side-side-tab-glide").expect("drawn");
    let (to, _) = rt.presented("side-tab-machines").expect("drawn");
    assert!(
        (mid[1] - to[1]).abs() > 1.,
        "a few frames in, the highlight is still on its way: {mid:?} vs {to:?}"
    );
    run(&mut rt, 60);
    assert!(on(&rt, "side", "machines"), "it lands on the new pill");
    // Widened to one row, it rides with the pills.
    rt.update(|s, _| {
        if let Some(x) = super::side_mut(s) {
            x.wide[Dock::Side.ix()] = true;
        }
    });
    run(&mut rt, 90);
    assert!(on(&rt, "side", "machines"), "it follows a resize");
}

/// A dock slides on `panels::REVEAL`, the sidebar's spring: the
/// shell's split view sets it on the whole main column, so a toggle
/// from anywhere rides it, and the chat's own buttons also queue the
/// sidebar's follow-ups (`dock_slide`) that land the rewrapped
/// conversation.
#[test]
fn a_dock_slides_on_the_reveal_spring() {
    let mut s = crate::chat::fixtures::test_state();
    crate::scenes::chat(&mut s);
    s.prefs.side_open = false;
    s.prefs.work_open = false;
    let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
    for _ in 0..30 {
        rt.frame(1. / 60.);
    }
    rt.update(|s, tx| {
        super::super::dock_slide(s, tx);
        toggle_right_pane(s);
    });
    assert!(
        rt.state.workspace.sidebar_slid.get(),
        "follow-ups wait for the view"
    );
    let full = super::width(&rt.state, Dock::Side, 1440., 1440. - 240.);
    let mut seen = Vec::new();
    for _ in 0..12 {
        rt.frame(1. / 60.);
        seen.push(rt.presented("side-side-dock").expect("drawn").0[2]);
    }
    assert!(
        !rt.state.workspace.sidebar_slid.get(),
        "the view queued them"
    );
    let between = seen.iter().filter(|w| **w > 1. && **w < full - 1.).count();
    assert!(
        between >= 4,
        "the dock slides through {between} frames: {seen:?}"
    );
    // Eight frames in, where REVEAL has it (within a frame's travel).
    let at = |spring: rune::Spring, frames: f32| full * spring.sample(0., 0., 1., frames / 60.).0;
    let drawn = seen[7];
    assert!(
        at(panels::REVEAL, 7.) - 1. <= drawn && drawn <= at(panels::REVEAL, 9.) + 1.,
        "{drawn} is not on REVEAL ({}–{})",
        at(panels::REVEAL, 7.),
        at(panels::REVEAL, 9.)
    );
}

/// The hold that keeps a closed dock drawn covers its clip's slide on
/// REVEAL for the widest column, and is far shorter than the fixed
/// 0.6 s it replaced.
#[test]
fn the_close_hold_covers_the_reveal_slide() {
    let hold = super::close_hold();
    assert!((0.2..0.5).contains(&hold), "{hold}");
    let widest = super::WIDEST;
    let left = widest * (1. - panels::REVEAL.sample(0., 0., 1., hold as f32).0);
    assert!(left.abs() <= 0.5, "within half a point of shut: {left}");
}

/// In a narrow window with both columns open the right pane gives up
/// its roomy minimum before it pushes the toggles past the window's
/// edge.
#[test]
fn a_narrow_window_keeps_the_toggles_inside() {
    let mut s = crate::chat::fixtures::test_state();
    crate::scenes::chat(&mut s);
    s.prefs.side_open = true;
    s.prefs.work_open = true;
    let width = 820.;
    let mut rt = Runtime::new(s, crate::view, (width, 760.), Env::new(true, 1.));
    let mut t = 0.;
    for _ in 0..90 {
        t += 1. / 60.;
        rt.frame(t);
        rt.update(|s, tx| {
            poll(s, t, tx);
        });
    }
    for key in [
        "side-session-toggle",
        "side-work-toggle",
        "side-pane-toggle",
    ] {
        let (r, _) = rt.presented(key).expect("toggle drawn");
        assert!(
            r[0] >= 0. && r[0] + r[2] <= width,
            "{key} at {r:?} is outside the {width} pt window"
        );
    }
}

/// A closed dock stays drawn (so the clip, not a fade, hides it) until
/// [`close_hold`] has passed on the app clock, and a reopen mid-hold
/// keeps it.
#[test]
fn a_closed_dock_is_drawn_until_its_clip_has_shut() {
    let mut s = crate::chat::fixtures::test_state();
    s.chat = Some(crate::chat::ChatState::new("t1".into()));
    s.prefs.side_open = true;
    let mut rt = Runtime::new(s, blank, (100., 100.), Env::new(true, 1.));
    rt.update(|s, tx| {
        poll(s, 0., tx);
    });
    rt.update(|s, _| toggle_right_pane(s));
    assert!(!rt.state.prefs.side_open);
    rt.update(|s, tx| {
        poll(s, 1., tx);
    });
    assert!(dock_drawn(&rt.state, Dock::Side), "held while sliding");
    rt.update(|s, tx| {
        poll(s, 1. + super::close_hold() / 2., tx);
    });
    assert!(dock_drawn(&rt.state, Dock::Side), "still sliding");
    let mut changed = false;
    rt.update(|s, tx| changed = poll(s, 1. + super::close_hold(), tx));
    assert!(changed, "the drop commits");
    assert!(!dock_drawn(&rt.state, Dock::Side), "gone once shut");
    // Closed, reopened mid-hold and closed again: the first close's
    // deadline must not drop the second close's slide.
    rt.update(|s, _| toggle_right_pane(s));
    rt.update(|s, tx| {
        poll(s, 5., tx);
    });
    rt.update(|s, _| toggle_right_pane(s));
    rt.update(|s, tx| {
        poll(s, 5.1, tx);
    });
    rt.update(|s, _| toggle_right_pane(s));
    rt.update(|s, tx| {
        poll(s, 5.2, tx);
    });
    rt.update(|s, _| toggle_right_pane(s));
    rt.update(|s, tx| {
        poll(s, 5.3, tx);
    });
    rt.update(|s, tx| {
        poll(s, 5.1 + super::close_hold() + 0.05, tx);
    });
    assert!(
        dock_drawn(&rt.state, Dock::Side),
        "a reopen restarts the hold"
    );
}

/// The widen button expands its dock over the whole conversation, composer
/// included: beside the open right pane it takes everything else, alone it
/// takes the right pane's room too, and the toggles stay in the window.
/// Pressing it again restores the saved width.
#[test]
fn an_expanded_dock_covers_the_conversation() {
    let mut s = crate::chat::fixtures::test_state();
    crate::scenes::chat(&mut s);
    s.prefs.work_open = true;
    s.prefs.side_open = true;
    let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
    let mut t = 0.;
    let mut run = |rt: &mut Runtime<State>, frames: usize| {
        for _ in 0..frames {
            t += 1. / 60.;
            rt.frame(1. / 60.);
            rt.update(|s, tx| {
                poll(s, t, tx);
            });
        }
    };
    let press = |rt: &mut Runtime<State>, key: &str| {
        let (r, _) = rt.presented(key).expect(key);
        let (x, y) = (r[0] + r[2] / 2., r[1] + r[3] / 2.);
        rt.pointer_move(x, y);
        rt.pointer_down(x, y);
        rt.pointer_up(x, y);
    };
    let w = |rt: &Runtime<State>, key: &str| rt.presented(key).expect(key).0[2];
    run(&mut rt, 90);
    let normal = w(&rt, "side-work-dock");
    let root = w(&rt, "chat-root");

    press(&mut rt, "side-work-wide");
    run(&mut rt, 90);
    let side = w(&rt, "side-side-dock");
    assert!(
        (w(&rt, "side-work-dock") - (root - side)).abs() <= 1.,
        "the work panel takes all but the right pane"
    );
    assert!(w(&rt, "conversation") <= 1., "the conversation is covered");
    let (capsule, _) = rt.presented("side-titlebar").expect("toggles");
    assert!(
        capsule[0] + capsule[2] <= 1440.5,
        "toggles inside: {capsule:?}"
    );

    rt.update(|s, _| toggle_right_pane(s));
    run(&mut rt, 90);
    assert!(
        (w(&rt, "side-work-dock") - root).abs() <= 1.,
        "with the right pane shut it takes that room too"
    );

    press(&mut rt, "side-work-wide");
    run(&mut rt, 90);
    assert!((w(&rt, "side-work-dock") - normal).abs() <= 1., "restored");
    assert!(w(&rt, "conversation") > 300., "the conversation is back");
}
