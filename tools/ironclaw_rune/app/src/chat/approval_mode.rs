//! The composer's approval pill (GPUI `chat/approval_mode.rs`): "Full
//! access" (tool calls run without asking) or "Ask first" (calls that need
//! approval open a card). The mode belongs to the thread's profile, so
//! switching it here changes every thread on that profile, running ones
//! included from their next tool call; the profile editor edits the same
//! value. A profile without a mode follows the global auto-approve setting,
//! and the pill shows what that resolves to.
//!
//! GPUI drew its own popover; here the menu is the native pop-up menu
//! (`Tx::popup_menu`): a disabled "Access" heading, the three choices with a
//! check on the current one (the global row says what it resolves to), and
//! the hint for the current choice at the bottom.

use zlo_client::profiles::{ApprovalMode, DEFAULT_PROFILE_ID, ProfileInfo};
use rune::{Color, El, PopupItem, PopupMenu, Tx, icon, row, text};
use rune_kit::Kit;

use super::{ChatState, chat_for, chat_ref, flow};
use crate::design::{self, Palette};
use crate::icons;
use crate::jobs;
use crate::state::State;

/// The global switch's key in the tool settings list.
const AUTO_APPROVE_KEY: &str = "agent.auto_approve_tools";

/// Menu order, shared with the profile editor.
pub const CHOICES: [Option<ApprovalMode>; 3] = [
    Some(ApprovalMode::FullAccess),
    Some(ApprovalMode::AskFirst),
    None,
];

pub fn label(mode: Option<ApprovalMode>) -> &'static str {
    match mode {
        Some(ApprovalMode::FullAccess) => "Full access",
        Some(ApprovalMode::AskFirst) => "Ask first",
        None => "Follow global setting",
    }
}

pub fn key(mode: Option<ApprovalMode>) -> &'static str {
    match mode {
        Some(ApprovalMode::FullAccess) => "full-access",
        Some(ApprovalMode::AskFirst) => "ask-first",
        None => "global",
    }
}

fn from_key(id: &str) -> Option<Option<ApprovalMode>> {
    CHOICES.into_iter().find(|m| key(*m) == id)
}

/// The pill's state in [`ChatState::approval`].
#[derive(Default)]
pub struct ApprovalPill {
    /// The thread's profile, once loaded.
    pub profile: Option<ProfileInfo>,
    /// The global auto-approve switch, once loaded.
    pub global: Option<bool>,
    /// Icon only (a side chat's narrow composer).
    pub compact: bool,
    pub busy: bool,
    /// Counts loads and saves; a reply from an older one is dropped.
    seq: u64,
}

impl ApprovalPill {
    /// What the gate does on this profile now.
    fn effective_label(&self, mode: Option<ApprovalMode>) -> &'static str {
        if ApprovalMode::resolve_auto_approve(mode, self.global.unwrap_or(false)) {
            label(Some(ApprovalMode::FullAccess))
        } else {
            label(Some(ApprovalMode::AskFirst))
        }
    }
}

/// An approval mode or the global switch was saved, from any page: every
/// open pill reloads (GPUI's `ApprovalEpoch`).
pub fn changed(s: &mut State) {
    let mut ids: Vec<String> = s.chat.iter().map(|c| c.thread_id.clone()).collect();
    ids.extend(super::side::side_chat_ids(s));
    for id in ids {
        load(s, &id);
    }
}

/// Reads the thread's profile and the global switch (again on each open,
/// so an edit on the Profiles page shows up here).
pub(crate) fn load(s: &mut State, id: &str) {
    let Some(c) = chat_for(s, id) else { return };
    c.approval.seq += 1;
    let seq = c.approval.seq;
    let thread = id.to_string();
    let req = thread.clone();
    jobs::run(
        s,
        "approval",
        move |c| async move {
            let inbox = c.inbox().await?;
            let id = inbox
                .rows
                .iter()
                .find(|r| r.thread_id == req)
                .map(|r| r.profile_id.clone())
                .unwrap_or_else(|| DEFAULT_PROFILE_ID.to_string());
            c.profile(&id).await
        },
        {
            let thread = thread.clone();
            move |s, result, _| {
                // silent-ok: an unreadable profile hides the pill; the gate still applies.
                if let (Ok(info), Some(c)) = (result, chat_for(s, &thread))
                    && c.approval.seq == seq
                {
                    c.approval.profile = Some(info);
                }
            }
        },
    );
    jobs::run(
        s,
        "approval",
        |c| async move { c.tool_settings().await },
        move |s, result, _| {
            // silent-ok: without the global value the pill shows "Ask first" for an unset mode.
            if let (Ok(list), Some(c)) = (result, chat_for(s, &thread))
                && c.approval.seq == seq
            {
                c.approval.global = list
                    .entries
                    .iter()
                    .find(|e| e.key == AUTO_APPROVE_KEY)
                    .and_then(|e| e.value.as_bool());
            }
        },
    );
}

/// Saves `mode` on the profile. The profile is re-read first so a save here
/// never overwrites an edit made elsewhere; the pill shows the stored result.
fn pick(s: &mut State, id: &str, mode: Option<ApprovalMode>) {
    let Some(c) = chat_for(s, id) else { return };
    let Some(profile_id) = c.approval.profile.as_ref().map(|p| p.profile_id.clone()) else {
        return;
    };
    c.approval.busy = true;
    // A load still in flight must not overwrite what this save stores.
    c.approval.seq += 1;
    let thread = id.to_string();
    jobs::run(
        s,
        "approval",
        move |c| async move {
            let mut info = c.profile(&profile_id).await?;
            info.settings.approval_mode = mode;
            c.update_profile(&profile_id, &info.settings).await
        },
        move |s, result, _| {
            let Some(c) = chat_for(s, &thread) else {
                return;
            };
            c.approval.busy = false;
            // silent-ok: a failed save keeps the stored mode on the pill (jobs logs it).
            if let Ok(info) = result {
                c.approval.profile = Some(info);
                // Other chats on this profile show it too.
                changed(s);
            }
        },
    );
}

/// The icon a mode shows on the pill and in the menu.
fn mode_icon(mode: Option<ApprovalMode>) -> &'static rune::Icon {
    match mode {
        Some(ApprovalMode::FullAccess) => &icons::ZAP,
        Some(ApprovalMode::AskFirst) => &icons::SHIELD_CHECK,
        None => &icons::SETTINGS_2,
    }
}

/// Full access reads warm (nothing stops a call), ask first calm.
fn tint(mode: Option<ApprovalMode>, global: Option<bool>, p: &Palette) -> Color {
    let full = ApprovalMode::resolve_auto_approve(mode, global.unwrap_or(false));
    match mode {
        None => p.text_muted,
        _ if full => p.warning,
        _ => p.success,
    }
}

/// The menu: a heading, one line per choice with a check on the picked one,
/// and a hint for the current choice.
fn menu(pill: &ApprovalPill, current: Option<ApprovalMode>) -> PopupMenu {
    let hint = match current {
        Some(ApprovalMode::FullAccess) => "Tool calls run without asking.",
        Some(ApprovalMode::AskFirst) => "Calls that need approval wait for you.",
        None => "Follows Settings > Permissions.",
    };
    let mut m = PopupMenu::new().item(PopupItem::new("heading", "Access").enabled(false));
    for mode in CHOICES {
        // The global row says what the switch resolves to right now.
        let title = match mode {
            None => format!("{} · {}", label(mode), pill.effective_label(None)),
            Some(_) => label(mode).to_string(),
        };
        m = m.item(PopupItem::new(key(mode), title).checked(current == mode));
    }
    m.separator()
        .item(PopupItem::new("hint", hint).enabled(false))
}

/// The pill after the model pill; nothing until the profile is known.
pub fn pill(s: &State, chat: &ChatState, _k: &Kit) -> El {
    let p = &s.palette;
    let a = &chat.approval;
    let Some(profile) = &a.profile else {
        return rune::el();
    };
    let current = profile.settings.approval_mode;
    let label_text = a.effective_label(current);
    let full = ApprovalMode::resolve_auto_approve(current, a.global.unwrap_or(false));
    let mark = if full {
        Some(ApprovalMode::FullAccess)
    } else {
        Some(ApprovalMode::AskFirst)
    };
    let id = chat.thread_id.clone();
    let mut pill = row()
        .key("approval-pill")
        .role("button")
        .label(format!("Approvals: {label_text}"))
        .gap(6.)
        .px(8.)
        .h(28.)
        .center_y()
        .radius(design::CONTROL_RADIUS)
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .child(icon(mode_icon(mark), 13.).color(tint(mark, a.global, p)));
    if !a.compact {
        pill = pill.child(
            text(label_text)
                .font(flow::type_at(s, 13., 18., 400.))
                .color(p.text_muted),
        );
    }
    if a.busy {
        pill = pill.child(rune_kit::Spinner::new().size(12.).build(_k));
    } else if !a.compact {
        pill = pill.child(icon(&icons::CHEVRON_DOWN, 12.).color(p.text_muted));
    }
    if a.compact {
        pill = pill.help(format!("Approvals: {label_text}"));
    }
    pill.on_press_tx(move |s: &mut State, tx: &mut Tx| {
        let Some(c) = chat_ref(s, &id) else { return };
        let menu = menu(&c.approval, current);
        // Re-read, so an edit on the Profiles page shows next time.
        load(s, &id);
        let thread = id.clone();
        tx.popup_menu(menu, "approval-pill", move |s: &mut State, picked, _| {
            if let Some(mode) = picked.and_then(from_key) {
                pick(s, &thread, mode);
            }
        });
    })
}
