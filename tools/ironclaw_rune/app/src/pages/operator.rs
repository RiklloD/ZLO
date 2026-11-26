//! Operator: gateway readiness, the OS service, first-run setup, config keys,
//! extension credentials, the log ring, and the run inspector. Every route
//! behind this page is operator-gated; the local single-user token has that
//! role.
//!
//! The page shell (a tab row over panels built on first visit) and the
//! shared widgets; each tab lives in its own file (`operator/status.rs`,
//! `operator/config.rs`, …).

mod config;
mod extensions;
pub mod fixtures;
mod inspector;
mod logs;
#[cfg(test)]
mod motion_tests;
mod setup;
mod status;

use crate::pages::common::controls::{header_with, muted, refresh_icon};
use crate::pages::common::ts::{body_semibold, sm};
use crate::pages::common::{glyph, page_body};
use rune::{Cx, El, TextEdit, col, row, text};
use rune_kit::{Button, Kit};
use zlo_client::product_wire::{
    RebornOperatorConfigDiagnostic, RebornOperatorConfigDiagnosticSeverity,
};

use crate::design::motion;
use crate::icons;
use crate::state::State;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Tab {
    #[default]
    Status,
    Setup,
    Config,
    Extensions,
    Logs,
    Inspector,
}

const TABS: [(Tab, &str); 6] = [
    (Tab::Status, "Status"),
    (Tab::Setup, "Setup"),
    (Tab::Config, "Config"),
    (Tab::Extensions, "Extensions"),
    (Tab::Logs, "Logs"),
    (Tab::Inspector, "Inspector"),
];

/// Page shell: a tab row over panels that are built on first visit.
#[derive(Default)]
pub struct OperatorState {
    pub tab: Tab,
    pub status: status::StatusPanel,
    /// The status panel loaded once.
    status_built: bool,
    pub setup: Option<setup::SetupPanel>,
    pub config: Option<config::ConfigPanel>,
    pub extensions: Option<extensions::ExtensionConfigPanel>,
    pub logs: Option<logs::LogsPanel>,
    pub inspector: Option<inspector::InspectorPanel>,
    /// What a panel field's edit accessor gets when its panel is gone
    /// (Rune's accessors must always return a field).
    status_scratch: TextEdit,
}

fn st(s: &mut State) -> &mut OperatorState {
    &mut s.pages.operator
}

/// The page became visible: load on first visit, refresh after.
pub fn opened(s: &mut State) {
    if !st(s).status_built {
        st(s).status_built = true;
        if st(s).tab != Tab::Status {
            status::refresh(s);
        }
    }
    refresh(s);
}

/// Reload the visible tab only; the others reload when shown.
pub fn refresh(s: &mut State) {
    match st(s).tab {
        Tab::Status => status::refresh(s),
        Tab::Setup => setup::refresh(s),
        Tab::Config => config::refresh(s),
        Tab::Extensions => extensions::refresh(s),
        Tab::Logs => logs::refresh(s),
        Tab::Inspector => inspector::refresh(s),
    }
}

fn show(s: &mut State, tab: Tab) {
    let o = st(s);
    // A new panel's first load and a shown panel's reload read the same
    // routes, so both go through `refresh`.
    match tab {
        Tab::Status => {}
        Tab::Setup => build(&mut o.setup),
        Tab::Config => build(&mut o.config),
        Tab::Extensions => build(&mut o.extensions),
        Tab::Logs => build(&mut o.logs),
        Tab::Inspector => build(&mut o.inspector),
    }
    o.tab = tab;
    refresh(s);
}

/// Build a panel into `slot` if it is empty.
fn build<V: Default>(slot: &mut Option<V>) {
    if slot.is_none() {
        *slot = Some(V::default());
    }
}

/// Open the Logs tab narrowed to one run (a failed run's Logs button).
/// Other areas call this (chat, area A).
pub fn show_run_logs(s: &mut State, run_id: String) {
    crate::workspace::show(s, crate::state::Page::Operator);
    let o = st(s);
    build(&mut o.logs);
    o.tab = Tab::Logs;
    logs::set_run(s, Some(run_id));
}

/// The page's title, its actions and the tab row.
fn top(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let o = &s.pages.operator;
    let actions = vec![
        refresh_icon()
            .on_press(|s: &mut State| refresh(s))
            .build(k)
            .key("refresh-operator")
            .press_scale(motion::PRESS),
    ];
    let tabs = crate::pages::common::tab_bar_still(k, "operator-tabs", &TABS, o.tab, show).pb(16.);
    col()
        .w_full()
        .child(header_with(p, "Operator", actions))
        .child(tabs)
}

pub fn view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let o = &s.pages.operator;
    // Logs can run to thousands of lines: that tab is one virtual list.
    if o.tab == Tab::Logs
        && let Some(panel) = o.logs.as_ref()
    {
        return logs::page(panel, k);
    }
    let panel = match o.tab {
        Tab::Status => Some(status::view(s, k)),
        Tab::Setup => o.setup.as_ref().map(|x| setup::view(s, k, x)),
        Tab::Config => o.config.as_ref().map(|x| config::view(s, k, x)),
        Tab::Extensions => o.extensions.as_ref().map(|x| extensions::view(s, k, x)),
        Tab::Logs => None,
        Tab::Inspector => o
            .inspector
            .as_ref()
            .map(|x| inspector::view(s, cx, k, layer, x)),
    };
    let mut body = col().w_full().child(top(s, k));
    if let Some(panel) = panel {
        // Keyed by tab: switching tabs crossfades the panels in place.
        body = body.child(panel_motion(panel, o.tab));
    }
    page_body("operator-page", body)
}

/// A tab's panel, keyed by its tab so a switch crossfades the two panels
/// (`motion::fade`) instead of swapping them in one frame.
fn panel_motion(panel: El, tab: Tab) -> El {
    panel
        .key(format!("operator-panel-{tab:?}"))
        .transition(motion::fade())
}

// --- shared widgets ---

/// A section title: 20 px above, 8 below, semibold.
fn section_title(s: &State, title: &str) -> El {
    text(title.to_string())
        .font(body_semibold())
        .color(s.palette.text)
        .pt(20.)
        .pb(8.)
}

/// A success notice line; it wipes in and out (`motion::reveal`).
fn notice_line(s: &State, msg: &str) -> El {
    text(msg.to_string())
        .key("operator-notice")
        .transition(motion::reveal())
        .font(sm())
        .color(s.palette.success)
        .wrap()
        .pb(12.)
}

/// A danger-outlined confirm row: what will happen, then confirm or cancel.
/// It wipes open under what it confirms and away when answered; callers
/// key it among their siblings.
fn confirm_row(s: &State, k: &Kit, message: String, confirm: Button, cancel: Button) -> El {
    let p = &s.palette;
    row()
        .transition(motion::reveal())
        .w_full()
        .gap(8.)
        .center_y()
        .padding(8.)
        .radius(crate::design::CONTROL_RADIUS)
        .border(1., p.danger)
        .child(
            text(message)
                .font(sm())
                .color(p.text)
                .wrap()
                .grow(1.)
                .min_w(0.),
        )
        .child(confirm.build(k).key("confirm").press_scale(motion::PRESS))
        .child(cancel.build(k).key("cancel").press_scale(motion::PRESS))
}

fn diagnostic_row(s: &State, d: &RebornOperatorConfigDiagnostic) -> El {
    let p = &s.palette;
    let (glyph_icon, color) = match d.severity {
        RebornOperatorConfigDiagnosticSeverity::Error => (&icons::CIRCLE_X, p.danger),
        RebornOperatorConfigDiagnosticSeverity::Warning => (&icons::TRIANGLE_ALERT, p.warning),
        RebornOperatorConfigDiagnosticSeverity::Info => (&icons::INFO, p.text_muted),
    };
    let mut c = col()
        .grow(1.)
        .min_w(0.)
        .gap(2.)
        .child(text(d.message.clone()).font(sm()).color(p.text).wrap())
        .child(muted(p, format!("{}  ·  {}", d.key, d.reason_code)));
    if !d.remediation.is_empty() {
        c = c.child(muted(p, d.remediation.clone()));
    }
    row()
        .w_full()
        .gap(8.)
        .items_start()
        .py(6.)
        .child(col().pt(2.).child(glyph(glyph_icon, 14., color)))
        .child(c)
}
