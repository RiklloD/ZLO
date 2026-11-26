//! "Runs on" computer picker for the new-thread screen, carrying an unsent
//! draft across computer switches, applying the "New threads start on"
//! preference, and offering "Run on <label>" when computer use is unavailable
//! on the focused backend.

use std::ops::Range;

use rune::{El, row};
use rune_kit::{Kit, Menu, MenuItem, Placement};
use zlo_client::attachments::StagedFile;
use zlo_client::profiles::DEFAULT_PROFILE_ID;

use super::{catalog_thread, empty_draft, load_menu, record};
use crate::chat::inline_files;
use crate::design::kit::compact::{XS as COMPACT_XS, ghost_button, txt};
use crate::icons;
use crate::pages::computers::default_computer;
use crate::remote::peers::{self, PeerConn};
use crate::state::{Page, State};
use crate::workspace::ui::{self, XS, t};

/// An unsent new-thread draft carried across a computer focus switch.
#[derive(Clone)]
pub(crate) struct CarriedDraft {
    profile: String,
    text: String,
    caret: usize,
    pills: Vec<(Range<usize>, String)>,
    staged: Vec<StagedFile>,
    worktree: Option<bool>,
    model_pick: Option<(String, String)>,
    effort: Option<String>,
    /// True when this switch was triggered by opening a brand-new draft for
    /// the "New threads start on" preference rather than carrying an existing
    /// draft's edits.
    from_new_draft: bool,
}

/// Result of checking the "New threads start on" preference when opening a
/// new draft.
pub(super) enum DefaultOutcome {
    /// Focus moved to the preferred computer; `connected` will open the draft.
    Switched,
    /// Stay on the currently focused computer, optionally showing a one-line
    /// note explaining why the preferred computer was skipped.
    Stay(Option<String>),
}

/// The "Runs on" control for the draft composer's action row, next to the
/// worktree control and in its exact style. Hidden when only one computer is
/// usable (single-computer view stays pixel-identical).
pub(super) fn control(s: &State, k: &Kit, layer: &mut Vec<El>) -> Option<El> {
    let choices = peers::usable(s);
    if choices.len() <= 1 {
        return None;
    }
    let draft = s.workspace.draft.as_ref()?;
    let p = &s.palette;
    let open = draft.runs_on_open;
    let focused_key = s.onboarding.computer.key();
    let label = s.onboarding.computer.label();

    let trigger = row()
        .key("draft-runs-on")
        .role("button")
        .label(format!("Runs on {label}"))
        .px(8.)
        .h(28.)
        .gap(4.)
        .center_y()
        .radius(crate::design::CONTROL_RADIUS)
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .hover_group()
        .help("Choose which computer runs this new thread")
        .child(ui::glyph(
            &icons::MONITOR,
            12.,
            if open { p.text } else { p.text_faint },
        ))
        .child(t(label, XS, if open { p.text } else { p.text_faint }).group_color(p.text))
        .on_press(|s: &mut State| {
            if let Some(d) = s.workspace.draft.as_mut() {
                d.runs_on_open = !d.runs_on_open;
            }
        });

    let mut menu = Menu::new("draft-runs-on")
        .trigger(trigger)
        .open(open)
        .placement(Placement::BottomEnd)
        .min_width(180.)
        .on_close(|s: &mut State| {
            if let Some(d) = s.workspace.draft.as_mut() {
                d.runs_on_open = false;
            }
        });

    for choice in choices {
        let is_focused = choice.key == focused_key;
        let item_label = match &choice.conn {
            PeerConn::Ready => choice.label.clone(),
            PeerConn::Connecting => format!("{} · Connecting…", choice.label),
            PeerConn::Asleep => format!("{} · Asleep", choice.label),
            PeerConn::Failed(_) => format!("{} · Not connected", choice.label),
        };
        let key = choice.key.clone();
        menu = menu.item(
            MenuItem::new(item_label)
                .icon(&icons::MONITOR)
                .checked(is_focused)
                .on_select(move |s: &mut State| pick(s, key.clone())),
        );
    }

    Some(menu.build(k, layer))
}

/// Move the open draft to `key`, preserving text, caret, pills, staged files
/// and (when offered by the target computer) profile and model pick.
pub(crate) fn pick(s: &mut State, key: String) {
    if let Some(d) = s.workspace.draft.as_mut() {
        d.runs_on_open = false;
    }
    if s.onboarding.computer.key() == key {
        return;
    }
    if !crate::workspace::triage::is_triage_draft(s)
        && let Some(d) = s.workspace.draft.as_ref()
    {
        s.workspace.carried_draft = Some(CarriedDraft {
            profile: d.profile.clone(),
            text: d.input.text.clone(),
            caret: d.input.range().1,
            pills: inline_files::pills(&d.input),
            staged: d.staged.clone(),
            worktree: d.worktree,
            model_pick: d.picker.pending(),
            effort: d.picker.pending_effort(),
            from_new_draft: false,
        });
    }
    peers::focus(s, &key);
}

/// Check the "New threads start on" preference when opening a fresh draft.
/// Moves focus when the preferred computer is a ready peer; otherwise stays on
/// the current computer and returns a one-line note if the preferred computer
/// is unavailable.
pub(super) fn apply_default_on_open(s: &mut State) -> DefaultOutcome {
    if s.workspace.carried_draft.is_some() || crate::workspace::triage::is_triage_draft(s) {
        return DefaultOutcome::Stay(None);
    }
    let Some((target_key, target_label)) = default_computer::effective(s) else {
        return DefaultOutcome::Stay(None);
    };
    if target_key == s.onboarding.computer.key() {
        return DefaultOutcome::Stay(None);
    }
    let current_label = s.onboarding.computer.label();
    match peers::get(s, &target_key) {
        Some(peer) if peer.conn == PeerConn::Ready => {
            s.workspace.carried_draft = Some(CarriedDraft {
                profile: DEFAULT_PROFILE_ID.into(),
                text: String::new(),
                caret: 0,
                pills: Vec::new(),
                staged: Vec::new(),
                worktree: None,
                model_pick: None,
                effort: None,
                from_new_draft: true,
            });
            peers::focus(s, &target_key);
            // If handover ran synchronously (live backend), `connected` already
            // opened the draft on `target_key`. In fixture mode (no live
            // backend), `peers::focus` started a no-op switch so clear the
            // unused carry and stay put.
            if s.onboarding.computer.key() == target_key {
                DefaultOutcome::Switched
            } else {
                s.workspace.carried_draft = None;
                DefaultOutcome::Stay(Some(format!(
                    "{target_label} isn't connected — starting on {current_label} instead."
                )))
            }
        }
        Some(peer) if peer.conn == PeerConn::Asleep => DefaultOutcome::Stay(Some(format!(
            "{target_label} is asleep — starting on {current_label} instead."
        ))),
        Some(peer) if peer.conn == PeerConn::Connecting => DefaultOutcome::Stay(Some(format!(
            "{target_label} is still connecting — starting on {current_label} instead."
        ))),
        _ => DefaultOutcome::Stay(Some(format!(
            "{target_label} isn't connected — starting on {current_label} instead."
        ))),
    }
}

/// Restore a carried draft after focus lands on the target computer. Returns
/// `true` when the draft was restored (so `workspace::connected` stays on the
/// new-thread screen instead of opening the last thread).
pub(crate) fn connected(s: &mut State) -> bool {
    let Some(carried) = s.workspace.carried_draft.take() else {
        return false;
    };
    if carried.from_new_draft {
        super::open(s);
        return true;
    }
    let list = &s.workspace.inbox.list;
    let profile = if list
        .profiles
        .iter()
        .any(|p| p.profile_id == carried.profile)
    {
        carried.profile
    } else {
        list.last_used_profile_id
            .clone()
            .or_else(|| list.profiles.first().map(|p| p.profile_id.clone()))
            .unwrap_or_else(|| DEFAULT_PROFILE_ID.into())
    };
    let mut draft = empty_draft(&profile);
    draft.staged = carried.staged;
    draft.worktree = carried.worktree;
    draft.fresh = false;
    inline_files::set_content(&mut draft.input, &carried.text, &carried.pills);
    let caret = carried.caret.min(draft.input.text.len());
    draft.input.set_caret(caret);
    draft.input.focused = true;
    let generation = s.workspace.draft_opens.bump();
    draft.generation = generation;
    s.workspace.draft = Some(draft);
    s.page = Page::Draft;
    load_menu(s, generation);
    let catalog = catalog_thread(s);
    crate::chat::model_picker::draft_carry_pick(s, carried.model_pick, carried.effort);
    crate::chat::model_picker::draft_set_profile(s, profile, catalog);
    record(s);
    true
}

/// When the focused backend is known to lack computer use and another ready
/// peer can do it, show the "not available" note with a "Run on <label>"
/// action that moves this draft there.
pub(super) fn computer_use_note(s: &State, k: &Kit) -> Option<El> {
    let readiness = s.remote.focused_computer_readiness.as_ref()?;
    if readiness.ready() {
        return None;
    }
    let (peer_key, peer_label) = crate::chat::computer::remote::capable_peer(s)?;
    let p = &s.palette;
    let line = crate::chat::computer::remote::line(&crate::remote::computer_label(s), readiness);
    Some(
        row()
            .key("draft-cu-note")
            .max_w(super::HERO_WIDTH)
            .gap(8.)
            .px(10.)
            .py(8.)
            .center_y()
            .radius(crate::design::CONTROL_RADIUS)
            .fill(p.surface_overlay)
            .border(1., p.border)
            .child(
                txt(line, COMPACT_XS, p.text_muted)
                    .wrap()
                    .grow(1.)
                    .min_w(0.),
            )
            .child(ghost_button(
                k,
                "draft-cu-run-on",
                &format!("Run on {peer_label}"),
                None,
                false,
                move |s| pick(s, peer_key.clone()),
            )),
    )
}

#[cfg(test)]
#[path = "runs_on_tests.rs"]
mod tests;
