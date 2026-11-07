//! The bottom status bar: connection on the left; context, speed, cache and
//! the busiest usage window on the right (GPUI `render_status_bar`,
//! `render_metrics`, `start_status`, `refresh_status` and
//! `refresh_telemetry` of `workspace.rs`). Polls every `STATUS_POLL` while
//! the bar shows (or Usage is open) and the battery saver allows.

use std::time::Instant;

use rune::{El, el, row};
use rune_kit::Kit;

use super::ui::{self, XS, t};
use super::{STATUS_POLL, active_id, context};
use crate::design;
use crate::jobs;
use crate::state::{Conn, Page, State};

/// After connecting: the first refresh and the poll timer.
pub(super) fn start(s: &mut State) {
    refresh_status(s);
    s.workspace.timers.status = Some(Instant::now() + STATUS_POLL);
}

/// One status refresh: skipped while the battery saver holds (background
/// or idle window) or the bar is hidden.
pub(super) fn refresh_status(s: &mut State) {
    if crate::design::look::saving(s) {
        return;
    }
    let usage_visible = s.page == Page::Usage;
    if s.prefs.status_bar_hidden && !usage_visible {
        return;
    }
    crate::pages::usage::refresh_report(s, false);
    refresh_telemetry(s);
}

pub(super) fn refresh_telemetry(s: &mut State) {
    let Some(id) = active_id(s).map(str::to_string) else {
        return;
    };
    let asked = id.clone();
    jobs::run(
        s,
        "telemetry",
        move |c| async move { c.thread_telemetry(&id).await },
        move |s, result, _| {
            // The user switched threads while this was in flight.
            if active_id(s) != Some(asked.as_str()) {
                return;
            }
            // silent-ok: the bar shows "—" for anything unknown.
            let telemetry = result.ok().flatten();
            if s.workspace.telemetry != telemetry {
                s.workspace.telemetry = telemetry;
                context::telemetry_changed(s);
            }
        },
    );
}

/// The bottom strip: connection on the left, metrics on the right.
pub(super) fn bar(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let live = matches!(s.conn, Conn::Connecting);
    let (dot, label) = match &s.conn {
        Conn::Connecting => (p.warning, "Connecting".to_string()),
        Conn::Failed { .. } => (p.danger, "Disconnected".to_string()),
        Conn::Ready => {
            let host =
                s.rt.backend
                    .as_ref()
                    .map(|b| b.client().connection().base_url.clone())
                    .unwrap_or_else(|| "127.0.0.1".into());
            let host = host
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .trim_end_matches('/')
                .to_string();
            (p.success, format!("Connected · {host}"))
        }
    };
    let mut status_dot = ui::dot(dot, 6.);
    if live {
        status_dot = status_dot.cycle(rune::Cycle {
            period: 1.8,
            phase: 0.,
            stops: &[(0.0, 1.0), (0.5, 0.35), (1.0, 1.0)],
            still: 1.0,
            fps: 30.0,
        });
    }
    let mut bar = row()
        .key("status-bar")
        .w_full()
        .h(design::STATUS_BAR_HEIGHT)
        .px(12.)
        .gap(8.)
        .center_y()
        .child(status_dot)
        .child(t(label, XS, p.text_faint))
        .child(el().grow(1.0));
    if matches!(s.conn, Conn::Ready) {
        bar = bar.child(metrics(s, k, layer));
    }
    bar
}

/// Right side of the bar: context, speed, cache, then the busiest usage
/// window (a click opens the Usage page).
fn metrics(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let tel = s.workspace.telemetry.as_ref();
    let dash = || "—".to_string();
    // IronClaw's own loop reports no window size; show the fill alone then.
    let context = match tel.map(|t| (t.context_used, t.context_max)) {
        Some((Some(used), Some(max))) => {
            let pct = tel
                .and_then(|t| t.context_pct)
                .unwrap_or(used as f64 * 100. / max.max(1) as f64);
            format!("{} / {} · {pct:.0}%", tokens(used), tokens(max))
        }
        Some((Some(used), None)) => tokens(used),
        _ => dash(),
    };
    let tps = tel
        .and_then(|t| t.tps)
        .map(|v| format!("{v:.0} tok/s"))
        .unwrap_or_else(dash);
    let cache = tel
        .and_then(|t| t.cache_hit_rate)
        .map(|r| format!("{:.0}%", r * 100.))
        .unwrap_or_else(dash);
    let metric = |name: &str, value: String| {
        row()
            .gap(4.)
            .child(t(name, XS, p.text_faint))
            .child(t(value, XS, p.text_faint))
    };
    let mut out = row()
        .gap(12.)
        .center_y()
        .child(context::meter(s, context, k, layer))
        .child(metric("Speed", tps))
        .child(metric("Cache", cache));
    if let Some((provider, window)) = crate::pages::usage::peak(s) {
        let resets = window
            .resets_at
            .and_then(crate::pages::usage::short_countdown)
            .map(|r| format!(" · {r}"))
            .unwrap_or_default();
        let color = crate::pages::usage::level_color(p, window.used_percent);
        out = out.child(
            row()
                .key("status-usage")
                .role("button")
                .gap(4.)
                .center_y()
                .cursor(rune::Cursor::Pointer)
                .hover_group()
                .child(ui::dot(color, 6.))
                .child(
                    t(
                        format!(
                            "{} {:.0}% {}{resets}",
                            provider.label, window.used_percent, window.label
                        ),
                        XS,
                        p.text_faint,
                    )
                    .group_color(p.text),
                )
                .on_press(|s: &mut State| super::show(s, Page::Usage)),
        );
    }
    out
}

/// 51200 -> "51.2k", 1000000 -> "1M".
pub(super) fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{}M", trim(n as f64 / 1e6)),
        n if n >= 1_000 => format!("{}k", trim(n as f64 / 1e3)),
        n => n.to_string(),
    }
}

fn trim(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::tokens;

    #[test]
    fn token_counts_read_short() {
        assert_eq!(tokens(512), "512");
        assert_eq!(tokens(51_200), "51.2k");
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(2_000), "2k");
    }
}
