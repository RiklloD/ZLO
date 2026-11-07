//! An inbox row's right-click menu and its snooze presets (GPUI
//! `workspace/inbox.rs`).

use chrono::{Datelike, Duration as Span, Local, TimeZone};
use ironclaw_gpui_client::inbox::{InboxAction, InboxRow, InboxState, InboxStatus};
use rune::{PopupItem, PopupMenu, Tx};

use super::{mark, settle, title_of};
use crate::state::State;

/// The right-click menu of an inbox row (a native menu). Every mark goes
/// through the same calls as the hover buttons; "unread" is the local set.
pub(in crate::workspace) fn open_row_menu(s: &State, thread_id: &str, at: rune::PointerPress, tx: &mut Tx) {
    let Some(row) = s.workspace.inbox.row(thread_id) else {
        return;
    };
    let state = row.state;
    let blocker = settle_blocker(row);
    let unread = s.workspace.inbox.unread.contains(thread_id);
    let mut menu = PopupMenu::new().item(PopupItem::new("open", "Open"));
    menu = match state {
        InboxState::Snoozed => menu.item(PopupItem::new("wake", "Wake now")),
        InboxState::Settled => menu.item(PopupItem::new("unsettle", "Un-settle")),
        _ => {
            let pin = if state == InboxState::Pinned {
                PopupItem::new("unpin", "Unpin")
            } else {
                PopupItem::new("pin", "Pin")
            };
            let snooze = snooze_presets()
                .into_iter()
                .fold(PopupMenu::new(), |sub, (label, until)| {
                    sub.item(PopupItem::new(format!("snooze:{until}"), label))
                });
            menu.item(pin)
                .submenu("Snooze", snooze)
                .item(PopupItem::new("settle", "Settle").enabled(blocker.is_none()))
        }
    };
    let menu = menu
        .item(PopupItem::new(
            "unread",
            if unread { "Mark as read" } else { "Mark as unread" },
        ))
        .separator()
        .item(PopupItem::new("copy-title", "Copy title"))
        .item(PopupItem::new("copy-id", "Copy thread ID"))
        .separator()
        .item(PopupItem::new("delete", "Delete").enabled(state != InboxState::Working));
    let id = thread_id.to_string();
    let title = title_of(row).to_string();
    tx.popup_menu(menu, at, move |s: &mut State, picked, tx| {
        let Some(picked) = picked else { return };
        let id = id.clone();
        match picked {
            "open" => crate::workspace::open_thread(s, id),
            "wake" => mark(s, id, InboxAction::Wake, None),
            "unsettle" => mark(s, id, InboxAction::Unsettle, None),
            "pin" => mark(s, id, InboxAction::Pin, None),
            "unpin" => mark(s, id, InboxAction::Unpin, None),
            "settle" => settle(s, id),
            "unread" => {
                let set = &mut s.workspace.inbox.unread;
                if !set.remove(&id) {
                    set.insert(id);
                }
            }
            "copy-title" => tx.copy(title.clone()),
            "copy-id" => tx.copy(id),
            "delete" => s.workspace.confirm_delete = Some(id),
            other => {
                if let Some(until) = other
                    .strip_prefix("snooze:")
                    .and_then(|u| u.parse::<i64>().ok())
                {
                    mark(s, id, InboxAction::Snooze, Some(until));
                }
            }
        }
    });
}

/// Why a row can't be settled (t3code refuses live work), or None.
pub(in crate::workspace) fn settle_blocker(row: &InboxRow) -> Option<&'static str> {
    match row.status {
        Some(InboxStatus::Working) => Some("is working"),
        Some(InboxStatus::NeedsInput) => Some("is waiting on you"),
        Some(InboxStatus::Background) => Some("has subagents running"),
        _ => None,
    }
}

/// T3's presets: in 1 hour, in 3 hours, this evening (if over an hour
/// away), tomorrow 09:00, next Monday 09:00.
pub(in crate::workspace) fn snooze_presets() -> Vec<(String, i64)> {
    let now = Local::now();
    let at = |days: i64, hour: u32| {
        (now.date_naive() + Span::days(days))
            .and_hms_opt(hour, 0, 0)
            .and_then(|t| Local.from_local_datetime(&t).single())
    };
    let mut out = vec![
        ("In 1 hour".to_string(), now + Span::hours(1)),
        ("In 3 hours".to_string(), now + Span::hours(3)),
    ];
    if let Some(evening) = at(0, 18).filter(|t| *t - now > Span::hours(1)) {
        out.push(("This evening, 18:00".into(), evening));
    }
    if let Some(tomorrow) = at(1, 9) {
        out.push(("Tomorrow, 09:00".into(), tomorrow));
    }
    let to_monday = (7 - now.weekday().num_days_from_monday() as i64) % 7;
    let to_monday = if to_monday == 0 { 7 } else { to_monday };
    if to_monday != 1
        && let Some(monday) = at(to_monday, 9)
    {
        out.push(("Next week, Mon 09:00".into(), monday));
    }
    out.into_iter()
        .map(|(label, t)| (label, t.timestamp_millis()))
        .collect()
}
