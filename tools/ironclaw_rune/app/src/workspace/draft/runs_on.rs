//! "Runs on" computer picker for the new-thread screen, carrying an unsent
//! draft across computer switches, applying the "New threads start on"
//! preference, and offering "Run on <label>" when computer use is unavailable
//! on the focused backend.
//!
//! A draft moves only to a computer that is already connected, in one
//! UI-thread step (a warm handover): there is no window in which the person
//! can edit a draft that is "on its way". Nothing is merged or deleted: when
//! the destination already holds an unsent draft, the two swap places.

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
    /// Computer key the draft was moved from (its saved copy is retired only
    /// after the move lands on `target`).
    source: String,
    /// Intended destination computer key.
    target: String,
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

/// The "Runs on" control for the draft composer's action row. Hidden when only one computer is
/// usable (single-computer view stays pixel-identical).
pub(super) fn control(s: &State, k: &Kit, layer: &mut Vec<El>) -> Option<El> {
    let choices = peers::usable(s);
    // A triage draft is about the computer it was started on (its session
    // is being prepared there): it does not move.
    if choices.len() <= 1 || crate::workspace::triage::is_triage_draft(s) {
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
                // Only a connected computer can take the draft.
                .disabled(!is_focused && !choice.ready)
                .on_select(move |s: &mut State| pick(s, key.clone())),
        );
    }

    Some(menu.build(k, layer))
}

/// Move the open draft to `key`, a connected computer, keeping its text,
/// caret, pills, staged files and (when that computer offers them) profile
/// and model pick. One step: the handover lands before this returns. A
/// computer that is not connected is not a destination (its menu item is
/// disabled); the draft stays where it is, untouched.
pub(crate) fn pick(s: &mut State, key: String) {
    if let Some(d) = s.workspace.draft.as_mut() {
        d.runs_on_open = false;
    }
    let source = s.onboarding.computer.key();
    let ready = matches!(s.conn, crate::state::Conn::Ready)
        && peers::get(s, &key).is_some_and(|p| p.conn == PeerConn::Ready);
    if source == key || !ready || crate::workspace::triage::is_triage_draft(s) {
        return;
    }
    if let Some(d) = s.workspace.draft.as_ref() {
        s.workspace.carried_draft = Some(CarriedDraft {
            source,
            target: key.clone(),
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
    // `connected` consumed the carry when the handover landed. Anything
    // left means it did not (fixtures): the draft is still on its screen.
    s.workspace.carried_draft = None;
}

/// Whether opening a fresh draft would move to another, ready computer
/// (the "New threads start on" preference).
pub(in crate::workspace) fn default_elsewhere(s: &State) -> bool {
    if s.workspace.draft.is_some()
        || s.workspace.carried_draft.is_some()
        || crate::workspace::triage::is_triage_draft(s)
    {
        return false;
    }
    default_computer::effective(s).is_some_and(|(key, _)| {
        key != s.onboarding.computer.key()
            && peers::get(s, &key).is_some_and(|p| p.conn == PeerConn::Ready)
    })
}

/// Check the "New threads start on" preference when opening a fresh draft.
/// Moves focus when the preferred computer is a ready peer; otherwise stays on
/// the current computer and returns a one-line note if the preferred computer
/// is unavailable.
pub(super) fn apply_default_on_open(s: &mut State) -> DefaultOutcome {
    if s.workspace.carried_draft.is_some()
        || s.remote.pending_thread.as_ref().is_some_and(|p| p.live(s))
        || crate::workspace::triage::is_triage_draft(s)
    {
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
                source: s.onboarding.computer.key(),
                target: target_key.clone(),
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
    // Only on its own destination; anything else drops the carry and leaves
    // the source computer's saved draft as it was.
    if carried.target != s.onboarding.computer.key() {
        return false;
    }
    if carried.from_new_draft {
        super::open(s);
        return true;
    }
    // What this computer had waiting, unsent.
    let existing = crate::drafts::restore(s, crate::drafts::NEW_THREAD);
    let carried_empty =
        carried.text.trim().is_empty() && carried.pills.is_empty() && carried.staged.is_empty();
    let list = &s.workspace.inbox.list;
    let preferred_profile = if carried_empty {
        existing
            .as_ref()
            .and_then(|d| d.profile.clone())
            .unwrap_or_else(|| carried.profile.clone())
    } else {
        carried.profile.clone()
    };
    let profile = if list
        .profiles
        .iter()
        .any(|p| p.profile_id == preferred_profile)
    {
        preferred_profile
    } else {
        list.last_used_profile_id
            .clone()
            .or_else(|| list.profiles.first().map(|p| p.profile_id.clone()))
            .unwrap_or_else(|| DEFAULT_PROFILE_ID.into())
    };
    // An empty carry shows what was waiting here. Otherwise the carried
    // draft takes this screen and the one that was waiting goes to the
    // source computer's slot (a swap): neither is merged, neither is lost.
    let (text, pills, staged, caret, displaced) = match existing {
        Some(saved) if carried_empty => {
            let pills: Vec<_> = saved.tokens.into_iter().map(|t| (t.range, t.id)).collect();
            let caret = saved.text.len();
            (saved.text, pills, saved.files, caret, None)
        }
        waiting => {
            let caret = carried.caret.min(carried.text.len());
            (carried.text, carried.pills, carried.staged, caret, waiting)
        }
    };
    let mut draft = empty_draft(&profile);
    draft.staged = staged;
    draft.worktree = carried.worktree;
    draft.fresh = false;
    inline_files::set_content(&mut draft.input, &text, &pills);
    draft.input.set_caret(caret.min(draft.input.text.len()));
    draft.input.focused = true;
    let generation = s.workspace.draft_opens.bump();
    draft.generation = generation;
    s.workspace.draft = Some(draft);
    s.page = Page::Draft;
    load_menu(s, generation);
    let catalog = catalog_thread(s);
    crate::chat::model_picker::draft_carry_pick(s, carried.model_pick, carried.effort);
    crate::chat::model_picker::draft_set_profile(s, profile, catalog);
    // This computer's slot now holds what is on screen.
    record(s);
    if carried.source != carried.target {
        // The source's slot: the draft that was waiting here, else nothing
        // (the moved draft lives here now, so going back cannot bring a
        // second copy of it).
        match displaced {
            Some(waiting) => {
                crate::drafts::record_for(s, &carried.source, crate::drafts::NEW_THREAD, waiting)
            }
            None => crate::drafts::remove_for(s, &carried.source, crate::drafts::NEW_THREAD),
        }
    }
    restage_for_remote(s);
    true
}

/// Files staged by path can only be opened by this Mac's backend. A draft
/// that just moved to another computer reads them now (within that
/// computer's limits), as attaching them there would have; each keeps its
/// key, so its card in the text still points at it. One that cannot be read
/// stays as it was, and the draft says why (a send refuses it by name).
pub(super) fn restage_for_remote(s: &mut State) {
    use zlo_client::attachments::{self, Backend, Body};
    if !crate::remote::backend_is_remote(s) {
        return;
    }
    let (Some(session), Some(draft)) = (s.session.as_ref(), s.workspace.draft.as_ref()) else {
        return;
    };
    let paths: Vec<(u64, std::path::PathBuf)> = draft
        .staged
        .iter()
        .filter_map(|f| match &f.body {
            Body::Path(path) => Some((f.key, path.clone())),
            Body::Bytes(_) => None,
        })
        .collect();
    if paths.is_empty() {
        return;
    }
    let limits = session.attachments.clone();
    let check = limits.clone();
    let generation = draft.generation;
    // What already uploads counts against the message's budget, and a file
    // is read only while the rest of that budget has room for it: a draft
    // with many large paths never reads more than one message may carry.
    let (mut count, mut total) = draft
        .staged
        .iter()
        .filter(|f| f.bytes().is_some())
        .fold((0usize, 0u64), |(n, bytes), f| (n + 1, bytes + f.size));
    crate::jobs::run(
        s,
        "restage attachments",
        move |_| async move {
            let mut out = Vec::new();
            for (key, path) in paths {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("attachment")
                    .to_string();
                let fits = match tokio::fs::metadata(&path).await {
                    Ok(meta) => {
                        count < limits.max_count
                            && total.saturating_add(meta.len()) <= limits.max_total_bytes
                    }
                    // `stage` says why it cannot be read.
                    Err(_) => true,
                };
                if !fits {
                    out.push((
                        key,
                        Err(format!(
                            "{name} does not fit in one message to this computer; remove it or send it separately."
                        )),
                    ));
                    continue;
                }
                let staged = attachments::stage(&path, &limits, Backend::Remote).await;
                if let Ok(read) = &staged {
                    count += 1;
                    total = total.saturating_add(read.size);
                }
                out.push((key, staged));
            }
            Ok(out)
        },
        move |s, result, _| {
            let Ok(results) = result else { return };
            let Some(draft) = s
                .workspace
                .draft
                .as_mut()
                .filter(|d| d.generation == generation)
            else {
                return;
            };
            let mut problems = Vec::new();
            for (key, staged) in results {
                // Within the message's count and total limits too, counting
                // what already uploads; one that does not fit stays a path,
                // which a send refuses by name.
                let others: Vec<_> = draft
                    .staged
                    .iter()
                    .filter(|f| f.key != key)
                    .cloned()
                    .collect();
                match staged
                    .and_then(|read| attachments::check(&read, &others, &check).map(|()| read))
                {
                    Ok(read) => {
                        if let Some(file) = draft.staged.iter_mut().find(|f| f.key == key) {
                            file.body = read.body;
                            file.size = read.size;
                            file.mime_type = read.mime_type;
                        }
                    }
                    Err(problem) => problems.push(problem),
                }
            }
            if !problems.is_empty() {
                draft.error = Some(problems.join(" "));
            }
            record(s);
        },
    );
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
