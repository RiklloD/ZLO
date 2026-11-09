//! The wizard's frame (t3code `WizardPopup`, `WizardHeader`, `WizardSteps`,
//! `WizardPanel`) and the Connect step (t3code `ConnectionStep` and
//! `PairingForm`). The frame is modal: the background is inert, a click
//! outside does nothing, and Close (or Escape) finishes setup, as t3code's
//! close press does.

use rune::{El, col, el, row, text};
use rune_kit::{Checkbox, Kit};

use crate::pages::common::controls::{Look, Size, button_sized, icon_button_sized};
use crate::pages::common::ts::{MONO_XS, sm, sm_medium, xs};
use crate::state::{Conn, State};

use super::{Finish, STEPS, Step, Toast};
use crate::remote::EnvState;
use zlo_client::environments::Active;

const WIDTH: f32 = 720.;

fn close(s: &mut State) {
    if s.onboarding.projects.importing || s.remote.page.busy.is_some() {
        return;
    }
    super::finish(
        s,
        Finish {
            imported: 0,
            warning: None,
        },
    );
}

pub fn wizard(s: &State, k: &Kit, layer: &mut Vec<El>) {
    let p = &s.palette;
    let o = &s.onboarding;
    let body = match o.step {
        Step::Connect => connect_step(s, k),
        Step::Agents => super::agents::view(s, k),
        Step::Projects => super::projects::view(s, k),
    };
    let mut panel_body = col().w_full().child(body);
    if let Some(e) = &o.finish_error {
        panel_body = panel_body.child(
            text(format!("Could not finish setup. {e}"))
                .font(sm())
                .color(p.danger)
                .pt(12.)
                .wrap(),
        );
    }
    let panel = col()
        .key("onboarding-dialog")
        .role("dialog")
        .label("Set up ZLO")
        .w_full()
        .max_w(WIDTH)
        .radius(12.)
        .fill(p.surface_overlay)
        .border(1., p.border_strong)
        .child(header(s, k))
        .child(
            col()
                .key("onboarding-panel")
                .w_full()
                .px(24.)
                .py(20.)
                .fill(p.bg.alpha(0.5))
                .border(1., p.border)
                .child(panel_body),
        )
        .token_motion(k.motion.overlay)
        .modal(false, close);
    layer.push(
        el().key("onboarding-scrim")
            .pin(Some(0.), Some(0.), Some(0.), Some(0.))
            .fill(rune::Color::BLACK.alpha(0.35))
            .a11y_hidden(),
    );
    layer.push(
        el().key("onboarding-layer")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .padding(16.)
            .center()
            .child(panel),
    );
}

/// t3code `WizardHeader`: the wordmark, the steps, and Close.
fn header(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let busy = s.onboarding.projects.importing || s.remote.page.busy.is_some();
    col()
        .key("onboarding-header")
        .w_full()
        .px(24.)
        .pt(20.)
        .pb(16.)
        .gap(16.)
        .child(
            row()
                .w_full()
                .gap(10.)
                .center_y()
                .child(mark(s))
                .child(
                    text("ZLO")
                        .font(rune::TextStyle::new(22., 600.))
                        .color(p.text_muted)
                        .grow(1.),
                )
                .child(
                    icon_button_sized(&crate::icons::CLOSE, "Close setup", Size::Small)
                        .disabled(busy)
                        .on_press(close)
                        .build(k)
                        .key("onboarding-close"),
                ),
        )
        .child(steps(s))
}

/// The claw mark's three strokes at wordmark size (t3code's identity).
fn mark(s: &State) -> El {
    let p = &s.palette;
    let stroke = |h: f32| {
        el().w(4.)
            .h(h)
            .radius(2.)
            .gradient(crate::design::lift(p.accent, 0.12), p.accent_strong)
    };
    row()
        .h(22.)
        .gap(3.)
        .center_y()
        .child(stroke(14.))
        .child(stroke(20.))
        .child(stroke(14.))
}

/// t3code `WizardSteps`: done steps take a check and can be reopened.
fn steps(s: &State) -> El {
    let p = &s.palette;
    let current = s.onboarding.step.index();
    let busy = s.onboarding.projects.importing;
    let mut list = row()
        .key("onboarding-steps")
        .role("list")
        .label("Setup progress")
        .w_full()
        .gap(4.)
        .padding(4.)
        .radius(12.)
        .fill(p.bg)
        .border(1., p.border);
    for (step, name) in STEPS {
        let ix = step.index();
        let here = ix == current;
        let done = ix < current;
        let badge = el()
            .w(20.)
            .h(20.)
            .radius(10.)
            .center()
            .fill(if done {
                p.accent
            } else if here {
                p.accent.alpha(0.12)
            } else {
                p.surface_card
            })
            .border(
                1.,
                if done {
                    p.accent
                } else if here {
                    p.accent.alpha(0.3)
                } else {
                    p.border_strong
                },
            )
            .child(if done {
                rune::icon(&crate::icons::CHECK, 14.).color(p.on_accent())
            } else {
                text((ix + 1).to_string()).font(sm_medium()).color(if here {
                    p.accent
                } else {
                    p.text_muted
                })
            });
        let mut item = row()
            .key(format!("onboarding-step-{ix}"))
            .grow(1.)
            .min_w(0.)
            .gap(8.)
            .px(10.)
            .py(8.)
            .radius(8.)
            .center_y()
            .label(format!("{name}, step {}", ix + 1))
            .child(badge)
            .child(
                text(name)
                    .font(sm_medium())
                    .color(if here { p.text } else { p.text_muted })
                    .truncate()
                    .min_w(0.),
            );
        if here {
            item = item.fill(p.surface_card).border(1., p.border);
        }
        if done && !busy {
            item = item
                .role("button")
                .focusable(true)
                .cursor(rune::Cursor::Pointer)
                .hover(p.surface_card)
                .on_press(move |s: &mut State| super::go_to(s, step));
        }
        list = list.child(item);
    }
    list
}

/// t3code `StepShell`: the step's title and description.
pub fn step_shell(s: &State, title: &str, description: &str) -> El {
    let p = &s.palette;
    col()
        .w_full()
        .child(
            text(title.to_string())
                .font(rune::TextStyle::new(22., 600.))
                .color(p.text),
        )
        .child(
            text(description.to_string())
                .font(sm())
                .color(p.text_muted)
                .wrap()
                .pt(10.),
        )
}

/// t3code `CommandBlock`: `$ command` with a copy button.
pub fn command_block(s: &State, k: &Kit, command: &'static str, key: &str) -> El {
    let p = &s.palette;
    row()
        .key(key.to_string())
        .w_full()
        .gap(12.)
        .px(12.)
        .py(10.)
        .radius(8.)
        .center_y()
        .fill(p.hover)
        .border(1., p.border)
        .child(
            row()
                .grow(1.)
                .min_w(0.)
                .gap(8.)
                .child(text("$").font(MONO_XS).color(p.text_muted))
                .child(
                    text(command)
                        .font(MONO_XS)
                        .color(p.text)
                        .selectable(true)
                        .truncate()
                        .min_w(0.),
                ),
        )
        .child(
            icon_button_sized(&crate::icons::COPY, "Copy command", Size::XSmall)
                .on_press_tx(move |_: &mut State, tx| tx.copy(command))
                .build(k)
                .key(format!("{key}-copy")),
        )
}

/// A computer setup can pick: its key, label and caption.
struct Choice {
    key: String,
    label: String,
    caption: Option<String>,
    active: Active,
}

/// This Mac, then every saved computer.
fn computers(s: &State) -> Vec<Choice> {
    let mut out = Vec::new();
    // A Light bundle has no IronClaw of its own to offer.
    if !s.remote.app_only {
        out.push(Choice {
            key: zlo_client::environments::THIS_MAC.into(),
            label: "This Mac".into(),
            caption: None,
            active: Active::ThisMac,
        });
    }
    for env in crate::remote::saved(s) {
        out.push(Choice {
            key: env.environment_id.clone(),
            label: env.label.clone(),
            caption: env
                .routes
                .first()
                .map(zlo_client::environments::Route::caption),
            active: Active::Environment(env.environment_id.clone()),
        });
    }
    out
}

/// What a computer's row says, t3code's connection labels.
fn status_of(s: &State, key: &str) -> &'static str {
    match crate::remote::state_of(s, key) {
        EnvState::Connected => "Connected",
        EnvState::Connecting => "Connecting…",
        EnvState::Asleep => "Asleep",
        EnvState::Blocked(_) => "Not connected",
        EnvState::Saved => "Saved",
    }
}

/// Pick a computer. ZLO works on one at a time, so picking another
/// switches to it (t3code adds it to the set it sets up).
fn pick(s: &mut State, choice: Active) {
    crate::remote::switch_to(s, choice);
}

fn connect_step(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let active = s.onboarding.computer.key();
    let ready = matches!(s.conn, Conn::Ready) && s.remote.switching.is_none();
    let mut list = col().key("computers").w_full().gap(8.).pt(20.);
    for choice in computers(s) {
        let on = choice.key == active;
        let status = status_of(s, &choice.key);
        let mut info = col().grow(1.).min_w(0.).child(
            row()
                .w_full()
                .gap(12.)
                .child(
                    text(choice.label.clone())
                        .font(sm_medium())
                        .color(p.text)
                        .grow(1.)
                        .min_w(0.)
                        .wrap(),
                )
                .child(text(status).font(xs()).color(p.text_muted)),
        );
        if let Some(caption) = &choice.caption {
            info = info.child(text(caption.clone()).font(xs()).color(p.text_muted).pt(2.));
        }
        if let EnvState::Blocked(why) = crate::remote::state_of(s, &choice.key) {
            info = info.child(text(why).font(xs()).color(p.danger).wrap().pt(2.));
        }
        let (row_target, check_target) = (choice.active.clone(), choice.active.clone());
        list = list.child(
            row()
                .key(format!("computer-{}", choice.key))
                .w_full()
                .gap(12.)
                .px(12.)
                .py(12.)
                .radius(8.)
                .center_y()
                .border(1., if on { p.accent.alpha(0.5) } else { p.border })
                .fill(p.bg)
                .cursor(rune::Cursor::Pointer)
                .on_press(move |s: &mut State| pick(s, row_target.clone()))
                .child(
                    Checkbox::new(on)
                        .label(choice.label.clone())
                        .hide_label()
                        .on_toggle(move |s: &mut State| pick(s, check_target.clone()))
                        .build(k)
                        .key(format!("computer-check-{}", choice.key)),
                )
                .child(rune::icon(&crate::icons::MONITOR, 16.).color(p.text_muted))
                .child(info),
        );
    }
    list = list.child(pairing(s, k));
    if matches!(s.conn, Conn::Failed { .. }) {
        list = list.child(
            text(format!(
                "Not connected, so setup cannot go on with {}. Pick another computer, or start IronClaw there.",
                s.onboarding.computer.label()
            ))
            .font(xs())
            .color(p.text_muted)
            .wrap()
            .pt(8.),
        );
    }
    step_shell(
        s,
        "Connect your computer",
        "Choose the computer with your code. We'll set up agents and projects on it.",
    )
    .child(list)
    .child(
        row().w_full().pt(24.).justify_end().child(
            button_sized("Continue", Look::Primary, Size::Small)
                .icon(&crate::icons::ARROW_RIGHT)
                .disabled(!ready || s.remote.page.busy.is_some())
                .on_press(|s: &mut State| super::go_to(s, Step::Agents))
                .build(k)
                .key("connect-continue"),
        ),
    )
}

/// t3code "Add a computer": the Computers page's pairing form.
fn pairing(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let open = s.onboarding.connect.pairing_open;
    let chevron = rune::icon(
        if open {
            &crate::icons::CHEVRON_DOWN
        } else {
            &crate::icons::CHEVRON_RIGHT
        },
        16.,
    )
    .color(p.text_muted);
    let card = col()
        .key("pairing")
        .w_full()
        .radius(8.)
        .border(1., p.border)
        .fill(p.bg)
        .child(
            row()
                .key("pairing-toggle")
                .w_full()
                .min_h(52.)
                .px(12.)
                .gap(10.)
                .center_y()
                .role("button")
                .focusable(true)
                .label("Add a computer")
                .cursor(rune::Cursor::Pointer)
                .hover(p.hover)
                .on_press(|s: &mut State| {
                    if s.remote.page.busy.is_some() {
                        return;
                    }
                    let c = &mut s.onboarding.connect;
                    c.pairing_open = !c.pairing_open;
                    s.remote.page.input.focused = c.pairing_open;
                    if c.pairing_open {
                        crate::remote::pairing::discover(s);
                    }
                })
                .child(rune::icon(&crate::icons::GLOBE, 16.).color(p.text_muted))
                .child(text("Add a computer").font(sm()).color(p.text).grow(1.))
                .child(chevron),
        );
    if !open {
        return card;
    }
    card.child(
        col()
            .w_full()
            .px(12.)
            .pb(12.)
            .child(crate::pages::computers::add_computer(s, k)),
    )
}

/// The note after setup (t3code's toast), bottom right.
pub fn toast(s: &State, k: &Kit, toast: &Toast) -> El {
    let p = &s.palette;
    let mut copy = col()
        .grow(1.)
        .min_w(0.)
        .gap(2.)
        .child(text(toast.title.clone()).font(sm_medium()).color(p.text));
    if let Some(d) = &toast.description {
        copy = copy.child(text(d.clone()).font(xs()).color(p.text_muted).wrap());
    }
    el().key("onboarding-toast-layer")
        .pin(None, None, Some(20.), Some(20.))
        .child(
            row()
                .key("onboarding-toast")
                .role("status")
                .w(340.)
                .gap(10.)
                .padding(12.)
                .radius(10.)
                .items_start()
                .fill(p.surface_overlay)
                .border(1., p.border_strong)
                .token_motion(k.motion.overlay)
                .child(
                    rune::icon(
                        if toast.warning {
                            &crate::icons::TRIANGLE_ALERT
                        } else {
                            &crate::icons::CIRCLE_CHECK
                        },
                        16.,
                    )
                    .color(if toast.warning { p.warning } else { p.success }),
                )
                .child(copy)
                .child(
                    icon_button_sized(&crate::icons::CLOSE, "Dismiss", Size::XSmall)
                        .on_press(|s: &mut State| s.onboarding.toast = None)
                        .build(k)
                        .key("onboarding-toast-close"),
                ),
        )
}
