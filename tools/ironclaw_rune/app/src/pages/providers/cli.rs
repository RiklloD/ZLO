//! Providers → CLI agents: one row per known coding-agent CLI, with its
//! install or sign-in command to copy and the remove / add-back button. An
//! agent this Mac does not have yet installs from its row (`cli_install`).

use rune::{El, col, row, text};
use rune_kit::Kit;
use zlo_client::orchestration::RouteProvider;

use super::catalog::{KNOWN_CLIS, KnownCli};
use super::view::LOGO;
use crate::cli_install::Phase;
use crate::design::brands::provider_logo;
use crate::design::{kit, motion};
use crate::icons;
use crate::pages::common::controls::{
    Look, Size, button_sized, icon_button_sized, muted, outlined_tag,
};
use crate::pages::common::ts::{MONO_XS, body_semibold, xs};
use crate::pages::common::{Tone, card, pressable};
use crate::state::State;

/// One CLI agent row: logo, name, blurb, a shell command to copy, status,
/// and the remove / add-back button.
struct CliRow {
    id: String,
    logo_ids: Vec<String>,
    name: String,
    blurb: String,
    status: El,
    commands: Vec<(&'static str, &'static str)>,
    remove: Option<(String, bool)>,
    /// The agent's models in the picker, while it is ready.
    models: Option<El>,
    /// Install / Cancel / Retry beside the status, for an agent not found.
    action: Option<El>,
    /// Under the row: the installer's output, or why it failed.
    below: Option<El>,
}

fn cli_row(s: &State, k: &Kit, r: CliRow) -> El {
    let p = &s.palette;
    let ids: Vec<&str> = r.logo_ids.iter().map(String::as_str).collect();
    let hidden = r.remove.as_ref().is_some_and(|(_, h)| *h);
    let mut info = col()
        .grow(1.)
        .min_w(0.)
        .child(text(r.name).font(body_semibold()).color(p.text))
        .child(text(r.blurb).font(xs()).color(p.text_muted).wrap());
    for (i, (label, cmd)) in r.commands.into_iter().enumerate() {
        // The first command's copy button keeps the row's own key.
        let copy_id = if i == 0 {
            r.id.clone()
        } else {
            format!("{}-{i}", r.id)
        };
        info = info.child(
            row()
                .pt(4.)
                .gap(8.)
                .center_y()
                .child(text(label).font(xs()).color(p.text_muted))
                .child(text(cmd).font(MONO_XS).color(p.text).selectable(true))
                .child(copy_button(s, k, &copy_id, cmd)),
        );
    }
    let mut line = row()
        .w_full()
        .gap(12.)
        .center_y()
        .child(provider_logo(s, &ids, LOGO))
        .child(info)
        .child(r.status);
    if let Some(action) = r.action {
        line = line.child(action);
    }
    if let Some((provider, hidden)) = r.remove {
        let b = if hidden {
            pressable(
                button_sized("Add back", Look::Outline, Size::XSmall)
                    .on_press(move |s: &mut State| {
                        super::set_cli_removed(s, provider.clone(), false)
                    })
                    .build(k),
            )
        } else {
            icon_button_sized(
                &icons::CLOSE,
                "Remove from ZLO and its model picker (the CLI stays installed)",
                Size::XSmall,
            )
            .on_press(move |s: &mut State| super::set_cli_removed(s, provider.clone(), true))
            .build(k)
            .press_scale(motion::PRESS)
        };
        line = line.child(b.key(format!("cli-remove-{}", r.id)));
    }
    // Removing a CLI dims its row, and adding it back lights it, on the
    // slide spring rather than in one frame.
    let mut c = card(p)
        .key(format!("cli-{}", r.id))
        .opacity_motion(motion::SLIDE)
        .opacity(if hidden { 0.6 } else { 1. })
        .child(line);
    if let Some(below) = r.below {
        c = c.child(below);
    }
    if let Some(models) = r.models {
        c = c.child(models);
    }
    c
}

/// An agent not found: its status, and what its row offers. This Mac runs
/// the installer from the row; another computer gets the command to run
/// there.
fn not_found(s: &State, k: &Kit, known: &'static KnownCli) -> (El, Option<El>, Option<El>) {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let breathing = |label: &'static str| {
        outlined_tag(p, Tone::Secondary, label).cycle(crate::design::kit::loops::breathe(
            1.6,
            0.,
            &crate::design::kit::loops::BREATH_55,
        ))
    };
    let small = |label: &'static str, look: Look| button_sized(label, look, Size::XSmall);
    let runtime = known.runtime;
    match s.cli_installs.get(runtime).map(|i| (&i.phase, &i.tail)) {
        Some((Phase::Running, tail)) => {
            let mut lines = col()
                .key("install-tail")
                .pt(8.)
                .pl(LOGO + 12.)
                .gap(2.)
                .min_w(0.);
            if tail.is_empty() {
                lines = lines.child(
                    text("Starting the installer…")
                        .font(MONO_XS)
                        .color(p.text_muted),
                );
            }
            for (i, line) in tail.iter().enumerate() {
                lines = lines.child(
                    text(line.clone())
                        .key(format!("tail-{i}"))
                        .font(MONO_XS)
                        .color(p.text_muted)
                        .truncate(),
                );
            }
            let cancel = pressable(
                small("Cancel", Look::Ghost)
                    .on_press(move |s: &mut State| crate::cli_install::cancel(s, runtime))
                    .build(k),
            )
            .key(format!("cli-install-cancel-{runtime}"));
            (
                breathing("Installing…").key("installing"),
                Some(cancel),
                Some(lines.transition(motion::reveal())),
            )
        }
        Some((Phase::Finding, _)) => (breathing("Finding it…").key("finding"), None, None),
        Some((Phase::Failed(why), _)) => {
            let buttons = row()
                .gap(4.)
                .center_y()
                .child(
                    pressable(
                        small("Retry", Look::Outline)
                            .on_press(move |s: &mut State| crate::cli_install::start(s, runtime))
                            .build(k),
                    )
                    .key(format!("cli-install-retry-{runtime}")),
                )
                .child(
                    pressable(
                        small("Open in Terminal", Look::Ghost)
                            .on_press(move |s: &mut State| super::install_in_terminal(s, runtime))
                            .build(k),
                    )
                    .key(format!("cli-install-terminal-{runtime}")),
                );
            let why = row()
                .key("install-failed")
                .pt(8.)
                .pl(LOGO + 12.)
                .gap(8.)
                .center_y()
                .child(
                    text(why.clone())
                        .font(xs())
                        .color(p.warning)
                        .wrap()
                        .grow(1.)
                        .min_w(0.),
                )
                .child(
                    icon_button_sized(&icons::CLOSE, "Dismiss", Size::XSmall)
                        .on_press(move |s: &mut State| crate::cli_install::dismiss(s, runtime))
                        .build(k)
                        .press_scale(motion::PRESS),
                )
                .transition(motion::reveal());
            (
                outlined_tag(p, Tone::Warning, "Install failed").key("failed"),
                Some(buttons),
                Some(why),
            )
        }
        None => {
            let status = if known.planned {
                outlined_tag(p, Tone::Secondary, "Driver coming next")
            } else if ps.runtimes.is_none() {
                outlined_tag(p, Tone::Secondary, "Unknown")
            } else {
                outlined_tag(p, Tone::Warning, "Not detected")
            };
            if known.planned || known.install.is_none() {
                return (status, None, None);
            }
            if !crate::cli_install::can_install_here(s, known) {
                let host = crate::remote::computer_label(s);
                let hint = text(format!(
                    "Run this on {host}. It shows up here once it is installed."
                ))
                .key("install-remote")
                .font(xs())
                .color(p.text_muted)
                .wrap()
                .pt(6.);
                return (status, None, Some(hint));
            }
            let install = pressable(
                small("Install", Look::Outline)
                    .on_press(move |s: &mut State| crate::cli_install::start(s, runtime))
                    .build(k),
            )
            .key(format!("cli-install-{runtime}"));
            (status, Some(install), None)
        }
    }
}

/// A CLI row's copy button: the copy glyph turns into a check for
/// `kit::COPIED_FOR` after the person copies the command, then back.
fn copy_button(s: &State, k: &Kit, id: &str, cmd: &'static str) -> El {
    let copied = s.pages.providers.copied(id);
    let ink = kit::icon_ink(k, rune_kit::IconButtonVariant::Ghost);
    let row_id = id.to_string();
    kit::glyph_button(
        k,
        &format!("copy-{id}"),
        rune_kit::IconButtonVariant::Ghost,
        "Copy command",
        kit::copy_glyph("glyph", copied, 16., ink),
        move |s: &mut State, tx| super::copy_command(s, tx, row_id.clone(), cmd),
    )
}

pub(super) fn render_cli(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let ps = &s.pages.providers;
    let runtimes = ps.runtimes.clone().unwrap_or_default();
    let query = ps.search.text.trim().to_lowercase();
    let mut rows: Vec<El> = Vec::new();
    for known in KNOWN_CLIS {
        if !query.is_empty() && !known.name.to_lowercase().contains(&query) {
            continue;
        }
        let found: Vec<&RouteProvider> = runtimes
            .iter()
            .filter(|r| {
                r.provider.starts_with(known.runtime) || r.runtime.starts_with(known.runtime)
            })
            .collect();
        if found.is_empty() {
            let (status, action, below) = not_found(s, k, known);
            rows.push(cli_row(
                s,
                k,
                CliRow {
                    id: known.runtime.into(),
                    logo_ids: vec![known.runtime.to_string()],
                    name: known.name.to_string(),
                    blurb: known.blurb.to_string(),
                    status,
                    commands: known.install.map(|c| ("Install", c)).into_iter().collect(),
                    remove: None,
                    models: None,
                    action,
                    below,
                },
            ));
            continue;
        }
        for r in found {
            let hidden = ps
                .picker
                .as_ref()
                .is_some_and(|pp| pp.provider_hidden(&r.provider));
            let status = if hidden {
                outlined_tag(p, Tone::Secondary, "Removed")
            } else if r.authenticated {
                outlined_tag(
                    p,
                    Tone::Success,
                    format!("Ready · {} models", r.models.len()),
                )
            } else if r.needs_native_cli() {
                outlined_tag(p, Tone::Warning, "Needs the CLI to sign in")
            } else {
                outlined_tag(p, Tone::Warning, "Not signed in")
            };
            let commands = super::catalog::next_steps(known, r);
            rows.push(cli_row(
                s,
                k,
                CliRow {
                    id: r.provider.clone(),
                    logo_ids: vec![r.provider.clone(), r.runtime.clone()],
                    name: r.name.clone(),
                    blurb: known.blurb.to_string(),
                    status,
                    commands,
                    remove: ps.picker.is_some().then(|| (r.provider.clone(), hidden)),
                    models: (!hidden && r.authenticated && !r.models.is_empty())
                        .then(|| super::view::render_picker_models(s, k, r)),
                    action: None,
                    below: None,
                },
            ));
        }
    }
    let mut c = col().w_full().gap(12.);
    if ps.runtimes.is_none() {
        c = c.child(muted(
            p,
            "Start one thread to let IronClaw report which CLI agents it found.",
        ));
    }
    c.children(rows)
}
