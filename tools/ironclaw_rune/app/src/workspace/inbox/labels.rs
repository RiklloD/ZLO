//! Short labels the inbox rows draw: ages, wake times, branch names.

use chrono::{Local, TimeZone};

/// "now", "5m", "3h", "2d", then a date.
pub(in crate::workspace) fn short_age(at: i64, now: i64) -> String {
    let secs = (now - at).max(0) / 1000;
    match secs {
        s if s < 60 => "now".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s if s < 7 * 86_400 => format!("{}d", s / 86_400),
        _ => Local
            .timestamp_millis_opt(at)
            .single()
            .map_or_else(String::new, |t| t.format("%d.%m.").to_string()),
    }
}

/// "14:05" today, "Tue 09:00" later.
pub(in crate::workspace) fn wake_label(at: i64) -> String {
    let Some(t) = Local.timestamp_millis_opt(at).single() else {
        return String::new();
    };
    if t.date_naive() == Local::now().date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%a %H:%M").to_string()
    }
}

/// A long branch keeps its last path segment, the part that names it:
/// `feature/team/sidebar-pass` reads `…/sidebar-pass`.
pub(in crate::workspace) fn branch_tail(branch: &str) -> String {
    match branch.rsplit_once('/') {
        Some((_, tail)) if branch.chars().count() > 14 && !tail.is_empty() => format!("…/{tail}"),
        _ => branch.to_string(),
    }
}
