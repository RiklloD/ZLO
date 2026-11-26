//! An inbox row's right-click menu and its snooze presets.

use chrono::{Datelike, Duration as Span, Local, TimeZone};
use rune::{PopupItem, PopupMenu, Tx};
use zlo_client::inbox::{InboxAction, InboxRow, InboxState, InboxStatus};

use super::merged::{self, RowId};
use super::{mark_row, open_row, settle_row, title_of};
use crate::state::State;

/// The right-click menu of an inbox row (a native menu). Every mark goes
/// through the same calls as the hover buttons; "unread" is the local set.
pub(in crate::workspace) fn open_row_menu(
    s: &State,
    id: &RowId,
    at: rune::PointerPress,
    tx: &mut Tx,
) {
    let Some(row) = merged::find_row(s, id) else {
        return;
    };
    let state = row.state;
    let blocker = settle_blocker(row);
    let unread = s.workspace.inbox.unread.contains(id);
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
            if unread {
                "Mark as read"
            } else {
                "Mark as unread"
            },
        ))
        .separator()
        .item(PopupItem::new("copy-title", "Copy title"))
        .item(PopupItem::new("copy-id", "Copy thread ID"))
        .separator()
        .item(PopupItem::new("delete", "Delete").enabled(state != InboxState::Working));
    let row_id = id.clone();
    let title = title_of(row).to_string();
    tx.popup_menu(menu, at, move |s: &mut State, picked, tx| {
        let Some(picked) = picked else { return };
        let row_id = row_id.clone();
        match picked {
            "open" => open_row(s, &row_id),
            "wake" => mark_row(s, row_id, InboxAction::Wake, None),
            "unsettle" => mark_row(s, row_id, InboxAction::Unsettle, None),
            "pin" => mark_row(s, row_id, InboxAction::Pin, None),
            "unpin" => mark_row(s, row_id, InboxAction::Unpin, None),
            "settle" => settle_row(s, row_id),
            "unread" => {
                let set = &mut s.workspace.inbox.unread;
                if !set.remove(&row_id) {
                    set.insert(row_id);
                }
            }
            "copy-title" => tx.copy(title.clone()),
            "copy-id" => tx.copy(row_id.thread_id),
            "delete" => s.workspace.confirm_delete = Some(row_id),
            other => {
                if let Some(until) = other
                    .strip_prefix("snooze:")
                    .and_then(|u| u.parse::<i64>().ok())
                {
                    mark_row(s, row_id, InboxAction::Snooze, Some(until));
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
