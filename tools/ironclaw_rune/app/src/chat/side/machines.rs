//! Machines: the computers ZLO works on at a glance (Settings → Computers'
//! state, one click to switch), the threads running on the computer in use,
//! and the environments linked to it. Behaviour lives in `crate::remote`;
//! this tab only reads it, so it adds no requests beyond the peer-link list
//! and tailnet discovery the Computers page already makes.
//!
//! ZLO talks to one computer at a time, so running threads are listed for
//! the computer in use only; the others say so instead of guessing.

use chrono::Utc;
use rune::{Color, El, col, el, icon, row};
use rune_kit::Kit;
use zlo_client::environments::{Active, THIS_MAC};
use zlo_client::inbox::{InboxRow, InboxStatus};

use super::ui::{self, sm_med, tiny, xs};
use crate::icons;
use crate::remote::{self, EnvState};
use crate::state::{Page, State};

/// Opening the tab refreshes what it shows that can go stale: the tailnet's
/// online peers (asleep computers) and the linked environments.
pub(super) fn load(s: &mut State) {
    remote::pairing::discover(s);
    remote::linked::load_links(s);
}

/// The tab's buttons in the dock's titlebar.
pub(super) fn actions(k: &Kit) -> Vec<El> {
    vec![
        ui::ibtn(&icons::REFRESH_CW, "Refresh")
            .on_press(load)
            .build(k)
            .key("machines-refresh"),
        ui::ibtn(&icons::SETTINGS, "Manage computers")
            .on_press(|s: &mut State| crate::workspace::show(s, Page::Computers))
            .build(k)
            .key("machines-manage"),
    ]
}

/// Threads running on the computer in use, longest running first.
fn running(s: &State) -> Vec<&InboxRow> {
    let inbox = &s.workspace.inbox;
    let mut rows: Vec<&InboxRow> = inbox
        .list
        .rows
        .iter()
        .filter(|r| {
            matches!(
                inbox.shown_status(r),
                Some(InboxStatus::Working | InboxStatus::Background | InboxStatus::NeedsInput)
            )
        })
        .collect();
    rows.sort_by_key(|r| (r.working_since.unwrap_or(i64::MAX), r.thread_id.clone()));
    rows
}

/// The tab pill's count: threads running on the computer in use.
pub(super) fn running_count(s: &State) -> Option<usize> {
    Some(running(s).len())
}

fn state_color(st: &EnvState, s: &State) -> Color {
    let p = &s.palette;
    match st {
        EnvState::Connected => p.success,
        EnvState::Connecting => p.accent,
        EnvState::Blocked(_) => p.danger,
        EnvState::Asleep | EnvState::Saved => p.text_faint,
    }
}

fn state_label(st: &EnvState) -> &'static str {
    match st {
        EnvState::Connected => "Connected",
        EnvState::Connecting => "Connecting…",
        EnvState::Asleep => "Asleep",
        EnvState::Blocked(_) => "Not connected",
        EnvState::Saved => "Saved",
    }
}

fn status_dot(color: Color, live: bool) -> El {
    let d = ui::dot(color, 8.);
    if live {
        d.cycle(crate::design::kit::loops::breathe(
            1.2,
            0.,
            &crate::design::kit::loops::BREATH_45,
        ))
    } else {
        d
    }
}

/// "3 min" since Unix milliseconds.
fn since(ms: i64) -> String {
    let secs = ((Utc::now().timestamp_millis() - ms) / 1000).max(0);
    match secs {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// One computer: state dot, name, "In use", state and address, and a
/// Connect button when it is not the one in use.
fn machine(
    s: &State,
    k: &Kit,
    key: &str,
    name: String,
    caption: Option<String>,
    target: Active,
    running: usize,
) -> El {
    let p = &s.palette;
    let st = remote::state_of(s, key);
    let in_use = s.onboarding.computer.key() == key;
    let mut title = row()
        .gap(6.)
        .items_center()
        .child(ui::line(name, sm_med(), p.text).shrink().min_w(0.));
    if in_use {
        title = title.child(
            el().h(16.)
                .px(5.)
                .radius(5.)
                .center()
                .fill(p.accent_wash)
                .child(ui::line("In use", tiny(), p.accent)),
        );
    }
    let mut meta = vec![state_label(&st).to_string()];
    if in_use && matches!(st, EnvState::Connected) {
        meta.push(match running {
            0 => "idle".into(),
            1 => "1 running".into(),
            n => format!("{n} running"),
        });
    }
    meta.extend(caption);
    let mut info = col()
        .grow(1.)
        .min_w(0.)
        .gap(2.)
        .child(title)
        .child(ui::line(meta.join(" · "), xs(), p.text_faint));
    if let EnvState::Blocked(why) = &st {
        info = info.child(ui::para(why.clone(), xs(), p.danger).max_lines(2));
    }
    let mut card = row()
        .key(format!("machine-{key}"))
        .w_full()
        .min_h(48.)
        .px(10.)
        .py(8.)
        .gap(10.)
        .items_center()
        .radius(10.)
        .fill(p.surface_card.alpha(if p.light { 0.8 } else { 0.55 }))
        .border(1., if in_use { p.border_strong } else { p.border })
        .child(
            el().size(30., 30.)
                .radius(8.)
                .center()
                .fill(p.hover)
                .child(icon(&icons::MONITOR, 15.).color(p.text_muted)),
        )
        .child(status_dot(
            state_color(&st, s),
            matches!(st, EnvState::Connecting),
        ))
        .child(info);
    if !in_use && !matches!(st, EnvState::Connecting) {
        card = card.child(
            ui::outline("Connect")
                .on_press(move |s: &mut State| remote::switch_to(s, target.clone()))
                .build(k)
                .key(format!("machine-connect-{key}")),
        );
    }
    card
}

pub(super) fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let running = running(s);
    let mut list = col().w_full().px(12.).pt(4.).pb(12.).gap(6.);

    // ── computers ──
    list = list.child(ui::section_label(s, "Computers"));
    if !s.remote.app_only {
        list = list.child(machine(
            s,
            k,
            THIS_MAC,
            "This Mac".into(),
            None,
            Active::ThisMac,
            running.len(),
        ));
    }
    for env in remote::saved(s) {
        list = list.child(machine(
            s,
            k,
            &env.environment_id,
            env.label.clone(),
            env.routes.first().map(|r| r.caption()),
            Active::Environment(env.environment_id.clone()),
            running.len(),
        ));
    }
    if remote::saved(s).is_empty() {
        list = list.child(
            ui::hover_row(s, "machines-add")
                .min_h(36.)
                .child(icon(&icons::PLUS, 14.).color(p.text_muted))
                .child(ui::line("Add a computer", xs(), p.text_muted))
                .on_press(|s: &mut State| crate::workspace::show(s, Page::Computers)),
        );
    }

    // ── running threads ──
    let label = remote::computer_label(s);
    list = list.child(ui::section_label(
        s,
        format!("Running on {label} · {}", running.len()),
    ));
    if running.is_empty() {
        list = list.child(el().w_full().px(4.).py(6.).child(ui::line(
            format!("Nothing running on {label}."),
            xs(),
            p.text_muted,
        )));
    }
    for r in &running {
        let status = s.workspace.inbox.shown_status(r);
        let (word, color) = match status {
            Some(InboxStatus::NeedsInput) => ("Needs you", p.warning),
            Some(InboxStatus::Background) => ("Subagents working", p.accent),
            _ => ("Working", p.accent),
        };
        let meta = match r.working_since {
            Some(ms) => format!("{word} · {}", since(ms)),
            None => word.to_string(),
        };
        let title = r
            .title
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| "Untitled thread".into());
        let thread = r.thread_id.clone();
        list = list.child(
            ui::hover_row(s, format!("machine-thread-{}", r.thread_id))
                // Live-sorted: a resort glides, an arrival or a finished
                // thread wipes in and out, in commits that animate.
                .motion(crate::design::motion::ROW_RESORT)
                .transition(rune::Transition::reveal(rune::Edge::Top))
                .min_h(40.)
                .py(4.)
                .child(status_dot(color, true))
                .child(
                    col()
                        .grow(1.)
                        .min_w(0.)
                        .gap(1.)
                        .child(ui::line(title, sm_med(), p.text))
                        .child(ui::line(meta, xs(), p.text_faint)),
                )
                .child(icon(&icons::ARROW_RIGHT, 13.).color(p.text_faint))
                .on_press(move |s: &mut State| crate::workspace::open_thread(s, thread.clone())),
        );
    }
    if !remote::saved(s).is_empty() {
        list = list.child(el().w_full().px(4.).child(ui::para(
            "Threads on other computers show here once ZLO connects to them.",
            tiny(),
            p.text_faint,
        )));
    }

    // ── linked environments ──
    match &s.remote.linked.links {
        Some(Ok(links)) if !links.is_empty() => {
            list = list.child(ui::section_label(s, "Linked environments"));
            for link in links {
                let mut meta = vec![
                    remote::linked::access_label(link.access).to_string(),
                    remote::linked::preference_label(link.preference).to_string(),
                ];
                if let Some(url) = link.urls.first() {
                    meta.push(zlo_client::environments::host_of(url).to_string());
                }
                let (dot, line) = if link.revoked {
                    (p.danger, "Link revoked or expired".to_string())
                } else {
                    (p.success, meta.join(" · "))
                };
                list = list.child(
                    row()
                        .key(format!("machine-link-{}", link.environment_id))
                        .w_full()
                        .min_h(40.)
                        .px(8.)
                        .gap(8.)
                        .items_center()
                        .child(icon(&icons::NETWORK, 14.).color(p.text_muted))
                        .child(ui::dot(dot, 6.))
                        .child(
                            col()
                                .grow(1.)
                                .min_w(0.)
                                .gap(1.)
                                .child(ui::line(link.label.clone(), sm_med(), p.text))
                                .child(ui::line(
                                    line,
                                    xs(),
                                    if link.revoked { p.danger } else { p.text_faint },
                                )),
                        ),
                );
            }
        }
        Some(Err(e)) => {
            list = list
                .child(ui::section_label(s, "Linked environments"))
                .child(ui::error_line(s, e.clone()));
        }
        _ => {}
    }

    list = list.child(
        el().w_full().pt(6.).child(
            ui::outline("Manage computers")
                .icon(&icons::SETTINGS)
                .on_press(|s: &mut State| crate::workspace::show(s, Page::Computers))
                .build(k)
                .key("machines-manage-footer"),
        ),
    );
    ui::scroll("machines-list", list)
}
