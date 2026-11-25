//! The wizard's frame (t3code `WizardPopup`, `WizardHeader`, `WizardSteps`,
//! `WizardPanel`) and the Connect step (t3code `ConnectionStep` and
//! `PairingForm`). The frame is modal: the background is inert, a click
//! outside does nothing, and Close (or Escape) finishes setup, as t3code's
//! close press does.

use rune::{El, Spring, SpringPair, Transition, Tx, col, el, row, text};
use rune_kit::{Checkbox, IconButtonVariant, Kit};

use crate::design::{kit as zkit, motion};

use crate::pages::common::controls::{Look, Size, button_sized, icon_button_sized};
use crate::pages::common::ts::{MONO_XS, sm, sm_medium, xs};
use crate::pages::common::{pressable, rows_card};
use crate::state::{Conn, State};

use super::{Finish, Step, Toast};
use crate::remote::EnvState;
use zlo_client::environments::Active;

const WIDTH: f32 = 720.;

/// The card's inset from the window's sides.
pub(super) const EDGE_X: f32 = 16.;
/// The card's inset from the window's top and bottom: clear of the
/// titlebar's traffic lights, and the same below so the card sits centred.
pub(super) const EDGE_Y: f32 = 40.;
/// The card's side padding, shared by the header, body and footer.
const PAD_X: f32 = 24.;

/// What a step shows: its scrolling body and its pinned actions.
pub struct StepView {
    pub body: El,
    /// The step's buttons, right-aligned in the footer under the body.
    pub actions: Vec<El>,
}

impl StepView {
    pub fn new(body: El) -> Self {
        StepView {
            body,
            actions: Vec::new(),
        }
    }

    pub fn action(mut self, action: El) -> Self {
        self.actions.push(action);
        self
    }
}

/// How far a step's body travels as it arrives: a nudge toward where the
/// person is going, not a full-width push.
const STEP_NUDGE: f32 = 22.;
/// A step's body arriving (the page entrance spring).
const STEP_IN: Spring = Spring::new(0.32, 0.0);
/// The old body fading out: quick, so the two cross only briefly.
const STEP_OUT: Spring = Spring::new(0.14, 0.0);
/// How long a new step's body waits for the old one's fade.
const STEP_HANDOFF: f32 = 0.05;

/// A step's body: forward arrives from the right, back from the left,
/// fading in; the old body fades out in place, faster. The exit is
/// direction-free because an exiting element keeps the transition of its
/// last render, which cannot know where the person goes next.
pub(super) fn step_transition(direction: i8) -> Transition {
    Transition::asymmetric(
        Transition::offset(f32::from(direction) * STEP_NUDGE, 0.).combined(Transition::opacity()),
        Transition::opacity(),
    )
    .springs(SpringPair::new(STEP_IN, STEP_OUT))
}

pub(super) fn close(s: &mut State) {
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

/// The wizard: the stepper on top and the step's actions at the bottom stay
/// put while the step's body scrolls between them, and the card never
/// outgrows the window (it shrinks to it, [`EDGE_Y`] in from each edge).
pub fn wizard(s: &State, k: &Kit, layer: &mut Vec<El>) {
    let p = &s.palette;
    let o = &s.onboarding;
    let step = match o.step {
        Step::Connect => connect_step(s, k),
        Step::Agents => super::agents::view(s, k),
        Step::Projects => super::projects::view(s, k),
    };
    let body = col()
        .key(format!("onboarding-step-body-{:?}", o.step))
        .w_full()
        .child(step.body);
    let mut content = col().w_full().px(PAD_X).pt(20.).pb(24.).child(body);
    if let Some(e) = &o.finish_error {
        content = content.child(
            text(format!("Could not finish setup. {e}"))
                .key("onboarding-finish-error")
                .font(sm())
                .color(p.danger)
                .pt(12.)
                .wrap()
                .transition(motion::reveal()),
        );
    }
    // The step's scroll viewport: it takes what the stepper and the footer
    // leave, and shrinks below its content when the window is short. Each
    // visit to a step has its own, so a step opens at its top whatever it
    // or the last one was scrolled to, even when the person comes back
    // before the old body finished fading; the viewport carries the step's
    // transition.
    let scroll = col()
        .key(scroll_key(o))
        .w_full()
        .shrink()
        .min_h(0.)
        .scroll(rune::ScrollSpec {
            stick_bottom: false,
        })
        .transition(step_transition(o.direction))
        // The old step mostly gone before the new one starts in, so the
        // two don't read as one muddle.
        .enter_delay(STEP_HANDOFF)
        .child(content);
    let panel = col()
        .key("onboarding-dialog")
        .role("dialog")
        .label("Set up ZLO")
        .w_full()
        .max_w(WIDTH)
        .shrink()
        .min_h(0.)
        .radius(crate::design::PANEL_RADIUS)
        .continuous_corners()
        .fill(p.surface_overlay)
        .border(1., p.border_strong)
        .shadow(
            12.,
            32.,
            rune::Color::BLACK.alpha(if p.light { 0.1 } else { 0.4 }),
        )
        // While the card springs to the next step's height, the step
        // fading out must not spill past its edge.
        .clip_rounded(crate::design::PANEL_RADIUS)
        .child(header(s, k))
        .child(rule(p))
        .child(
            // Between the stepper and the footer, and nowhere else: the body
            // fading out keeps its old, maybe taller, rect, and this clip
            // keeps it off the footer and the header.
            col()
                .key("onboarding-viewport")
                .w_full()
                .shrink()
                .min_h(0.)
                .clip_corners([0.; 4])
                .child(scroll),
        )
        .child(rule(p))
        .child(footer(step.actions))
        .token_motion(k.motion.overlay)
        .transition(motion::pop(8.))
        .modal(false, close);
    layer.push(
        el().key("onboarding-scrim")
            .pin(Some(0.), Some(0.), Some(0.), Some(0.))
            .fill(rune::Color::BLACK.alpha(0.35))
            .a11y_hidden()
            .transition(motion::fade()),
    );
    layer.push(
        col()
            .key("onboarding-layer")
            .absolute(0.0, 0.0)
            .w_full()
            .h_full()
            .px(EDGE_X)
            .py(EDGE_Y)
            .center()
            .child(panel),
    );
}

/// The key of the step's scroll viewport for this visit.
pub(super) fn scroll_key(o: &super::OnboardingState) -> String {
    format!("onboarding-scroll-{:?}-{}", o.step, o.entry.0)
}

fn rule(p: &crate::design::Palette) -> El {
    el().w_full().h(1.).fill(p.border).a11y_hidden()
}

/// The pinned footer: the step's actions, on the right.
fn footer(actions: Vec<El>) -> El {
    row()
        .key("onboarding-footer")
        .w_full()
        .px(PAD_X)
        .py(14.)
        .gap(12.)
        .center_y()
        .justify_end()
        .children(actions)
}

/// t3code `WizardHeader`: the wordmark, the steps, and Close.
fn header(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let busy = s.onboarding.projects.importing || s.remote.page.busy.is_some();
    col()
        .key("onboarding-header")
        .w_full()
        .px(PAD_X)
        .pt(18.)
        .pb(16.)
        .gap(14.)
        .child(
            row()
                .w_full()
                .gap(10.)
                .center_y()
                .child(super::stepper::mark(s))
                .child(
                    text("ZLO")
                        .font(rune::TextStyle::new(18., 600.))
                        .color(p.text)
                        .grow(1.),
                )
                .child(
                    icon_button_sized(&crate::icons::CLOSE, "Close setup", Size::Small)
                        .disabled(busy)
                        .on_press(close)
                        .build(k)
                        .key("onboarding-close")
                        .press_scale(motion::PRESS),
                ),
        )
        .child(super::stepper::steps(s))
}

/// t3code `StepShell`: the step's title and description.
pub fn step_shell(s: &State, title: &str, description: &str) -> El {
    let p = &s.palette;
    col()
        .w_full()
        .child(
            text(title.to_string())
                .key("step-title")
                .font(rune::TextStyle::new(22., 600.))
                .color(p.text),
        )
        .child(
            text(description.to_string())
                .font(sm())
                .color(p.text_muted)
                .wrap()
                .pt(6.),
        )
}

/// A labelled group of rows, as Settings draws them: the label, then one
/// block on the wash fill with hairlines between its rows.
pub fn section(s: &State, key: &str, title: Option<&str>, rows: Vec<El>) -> El {
    let p = &s.palette;
    let mut c = col().key(key.to_string()).w_full().pt(20.);
    if let Some(title) = title {
        c = c.child(
            text(title.to_string())
                .font(sm_medium())
                .color(p.text)
                .pb(8.),
        );
    }
    c.child(rows_card(p, rows))
}

/// One row of a [`section`]: a leading mark, a title over a muted line, and
/// the row's status or action on the right (Settings' `setting_row`).
pub fn list_row(
    s: &State,
    key: String,
    lead: El,
    title: &str,
    line: Option<El>,
    trailing: El,
) -> El {
    let p = &s.palette;
    let mut info = col()
        .grow(1.)
        .min_w(0.)
        .gap(1.)
        .child(text(title.to_string()).font(sm_medium()).color(p.text));
    if let Some(line) = line {
        info = info.child(line);
    }
    row()
        .key(key)
        .w_full()
        .min_h(56.)
        .px(16.)
        .py(10.)
        .gap(12.)
        .center_y()
        .child(lead)
        .child(info)
        .child(trailing)
}

/// t3code `CommandBlock`: `$ command` with a copy button.
pub fn command_block(s: &State, k: &Kit, command: &'static str, key: &str) -> El {
    let p = &s.palette;
    row()
        .key(key.to_string())
        .w_full()
        .gap(12.)
        .pl(12.)
        .pr(4.)
        .py(4.)
        .radius(crate::design::CONTROL_RADIUS)
        .continuous_corners()
        .center_y()
        .fill(p.bg)
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
        .child({
            let copy_key = format!("{key}-copy");
            let copied = super::copied(s, &copy_key);
            let ink = zkit::icon_ink(k, IconButtonVariant::Ghost);
            let flash_key = copy_key.clone();
            zkit::glyph_button(
                k,
                &copy_key,
                IconButtonVariant::Ghost,
                if copied { "Copied" } else { "Copy command" },
                zkit::copy_glyph("glyph", copied, 16., ink),
                move |s: &mut State, tx: &mut Tx| {
                    tx.copy(command);
                    super::flash_copied(s, flash_key.clone(), tx);
                },
            )
        })
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

fn connect_step(s: &State, k: &Kit) -> StepView {
    let p = &s.palette;
    let active = s.onboarding.computer.key();
    let ready = matches!(s.conn, Conn::Ready) && s.remote.switching.is_none();
    let mut rows = Vec::new();
    for choice in computers(s) {
        let on = choice.key == active;
        let state = crate::remote::state_of(s, &choice.key);
        let mut line = col().w_full().gap(1.);
        if let Some(caption) = &choice.caption {
            line = line.child(
                text(caption.clone())
                    .font(xs())
                    .color(p.text_muted)
                    .truncate(),
            );
        }
        if let EnvState::Blocked(why) = &state {
            line = line.child(text(why.clone()).font(xs()).color(p.danger).wrap());
        }
        let status_line = text(status_of(s, &choice.key))
            .key("status")
            .font(xs())
            .color(p.text_muted);
        // Breathes on the loaders' 15 fps clock only while it connects;
        // still under Reduce Motion.
        let status_line = if matches!(state, EnvState::Connecting) {
            status_line.cycle(zkit::loops::breathe(1.6, 0., &zkit::loops::BREATH_55))
        } else {
            status_line
        };
        let (row_target, check_target) = (choice.active.clone(), choice.active.clone());
        let lead = row()
            .gap(12.)
            .center_y()
            .child(
                Checkbox::new(on)
                    .label(choice.label.clone())
                    .hide_label()
                    .on_toggle(move |s: &mut State| pick(s, check_target.clone()))
                    .build(&crate::design::kit::checkbox_kit(&s.palette, k))
                    .key(format!("computer-check-{}", choice.key)),
            )
            .child(rune::icon(&crate::icons::MONITOR, 16.).color(p.text_muted));
        rows.push(
            list_row(
                s,
                format!("computer-{}", choice.key),
                lead,
                &choice.label,
                (choice.caption.is_some() || matches!(state, EnvState::Blocked(_))).then_some(line),
                status_line,
            )
            .cursor(rune::Cursor::Pointer)
            .hover(p.hover)
            .hover_motion(motion::HOVER, motion::HOVER)
            .on_press(move |s: &mut State| pick(s, row_target.clone())),
        );
    }
    let mut body = step_shell(
        s,
        "Connect your computer",
        "Choose the computer with your code. We'll set up agents and projects on it.",
    )
    .child(section(s, "computers", None, rows))
    .child(pairing(s, k));
    if matches!(s.conn, Conn::Failed { .. }) {
        body = body.child(
            text(format!(
                "Not connected, so setup cannot go on with {}. Pick another computer, or start IronClaw there.",
                s.onboarding.computer.label()
            ))
            .font(xs())
            .color(p.text_muted)
            .wrap()
            .pt(12.),
        );
    }
    StepView::new(body).action(
        pressable(
            button_sized("Continue", Look::Primary, Size::Small)
                .icon(&crate::icons::ARROW_RIGHT)
                .disabled(!ready || s.remote.page.busy.is_some())
                .on_press(|s: &mut State| super::go_to(s, Step::Agents))
                .build(k),
        )
        .key("connect-continue"),
    )
}

/// t3code "Add a computer": the Computers page's pairing form.
fn pairing(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let open = s.onboarding.connect.pairing_open;
    let chevron = zkit::chevron(
        "pairing-chevron",
        open,
        zkit::Turn::Disclose,
        16.,
        p.text_muted,
    );
    let toggle = list_row(
        s,
        "pairing-toggle".into(),
        rune::icon(&crate::icons::GLOBE, 16.).color(p.text_muted),
        "Add a computer",
        Some(
            text("Pair another computer by its link or code, or find one on your tailnet.")
                .font(xs())
                .color(p.text_muted)
                .wrap(),
        ),
        chevron,
    )
    .role("button")
    .focusable(true)
    .label("Add a computer")
    .cursor(rune::Cursor::Pointer)
    .hover(p.hover)
    .hover_motion(motion::HOVER, motion::HOVER)
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
    });
    let mut rows = vec![toggle];
    if open {
        rows.push(
            col()
                .key("pairing-drawer")
                .w_full()
                .px(16.)
                .py(12.)
                .transition(motion::reveal())
                .child(crate::pages::computers::add_computer(s, k)),
        );
    }
    section(s, "pairing", None, rows).pt(8.)
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
        .transition(toast_transition())
        .enter_delay(TOAST_DELAY)
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

/// The note after setup rises into the corner once the wizard has begun to
/// leave, and sinks away faster.
fn toast_transition() -> Transition {
    Transition::offset(0., 14.)
        .combined(Transition::opacity())
        .springs(SpringPair::with_exit_ratio(STEP_IN, 0.6))
}

/// How long the note waits for the wizard's exit before it rises.
pub(super) const TOAST_DELAY: f32 = 0.12;
