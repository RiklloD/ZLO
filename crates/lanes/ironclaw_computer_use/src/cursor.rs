//! The viewer's cursor (the Hermes plugin's `_pointer_before` and
//! `_CursorWatch`). Coordinate input is published before the call, where the
//! model said it lands. While an element action runs, the native cursor's
//! destination is followed from the session's observation file (the app agent
//! writes it, `OPEN_COMPUTER_USE_VISUAL_CURSOR_OBSERVATION_FILE`), so the
//! viewer's cursor moves with it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ironclaw_product_contracts::computer_use::ComputerUseSession;
use serde_json::Value;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::live::Live;
use crate::target::TargetState;
use crate::windows::main_display_height;

const FOLLOW_EVERY: Duration = Duration::from_millis(120);

/// The kind of pointer an action shows; `None` for actions without one.
pub(crate) fn pointer_kind(tool: &str, args: &Value) -> Option<&'static str> {
    match tool {
        "click" => Some(click_kind(args)),
        "perform_secondary_action" => Some("click"),
        "scroll" => Some("scroll"),
        "drag" => Some("drag"),
        "set_value" | "type_text" | "press_key" | "select_text" => Some("focus"),
        _ => None,
    }
}

fn click_kind(args: &Value) -> &'static str {
    if args.get("mouse_button").and_then(Value::as_str) == Some("right") {
        return "right_click";
    }
    let clicks = args
        .get("click_count")
        .and_then(|count| count.as_u64().or_else(|| count.as_str()?.parse().ok()))
        .unwrap_or(1);
    if clicks >= 2 { "double_click" } else { "click" }
}

/// Coordinate input lands where the model said: publish it before the call.
pub(crate) fn pointer_before(
    live: &Live,
    key: &ComputerUseSession,
    target: &TargetState,
    tool: &str,
    args: &Value,
) {
    let (Some(pid), Some(window_id)) = (target.pid, target.window_id) else {
        return;
    };
    if !target.same_app(args.get("app")) {
        return;
    }
    match tool {
        "click" => {
            if let Some(at) = target.shot_fraction(args.get("x"), args.get("y")) {
                live.pointer(key, click_kind(args), at, None, pid, window_id);
            }
        }
        "drag" => {
            if let Some(from) = target.shot_fraction(args.get("from_x"), args.get("from_y")) {
                let to = target.shot_fraction(args.get("to_x"), args.get("to_y"));
                live.pointer(key, "drag", from, to, pid, window_id);
            }
        }
        _ => {}
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Observation {
    timestamp: Option<Value>,
    tip: Option<(f64, f64)>,
}

async fn read_observation(path: &Path) -> Option<Observation> {
    let bytes = tokio::fs::read(path).await.ok()?; // silent-ok: no observation yet
    let value: Value = serde_json::from_slice(&bytes).ok()?; // silent-ok: a half-written file is read again next tick
    let point = value
        .get("restingTipPosition")
        .filter(|point| point.is_object())
        .or_else(|| value.get("tipPosition"));
    let tip = point.and_then(|point| Some((point.get("x")?.as_f64()?, point.get("y")?.as_f64()?)));
    Some(Observation {
        timestamp: value.get("timestamp").cloned(),
        tip,
    })
}

/// The native cursor's tip (Cocoa screen points, bottom-left origin) →
/// fractions of the target window.
fn cursor_fraction(live: &Live, pid: i64, window_id: u32, tip: (f64, f64)) -> Option<(f64, f64)> {
    let (x0, y0, width, height) = live.window_bounds(pid, window_id)?;
    let display = main_display_height()?;
    let (x, y) = (tip.0, display - tip.1);
    let (fx, fy) = ((x - x0) / width, (y - y0) / height);
    ((-0.05..=1.05).contains(&fx) && (-0.05..=1.05).contains(&fy)).then_some((fx, fy))
}

/// Follows the cursor until finished.
pub(crate) struct CursorWatch {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

struct Follow {
    live: Arc<Live>,
    key: ComputerUseSession,
    pid: i64,
    window_id: u32,
    kind: &'static str,
    path: PathBuf,
    before: Option<Value>,
}

impl Follow {
    /// Publishes the latest position; returns it.
    async fn publish(&self, last: Option<(f64, f64)>, last_final: bool) -> Option<(f64, f64)> {
        let observation = read_observation(&self.path).await?;
        if observation.timestamp == self.before {
            return last;
        }
        let at = cursor_fraction(&self.live, self.pid, self.window_id, observation.tip?)?;
        if Some(at) == last && !last_final {
            return last;
        }
        let kind = if last_final { self.kind } else { "move" };
        self.live
            .pointer(&self.key, kind, at, None, self.pid, self.window_id);
        Some(at)
    }
}

impl CursorWatch {
    pub(crate) async fn start(
        live: Arc<Live>,
        key: ComputerUseSession,
        target: &TargetState,
        kind: &'static str,
        path: PathBuf,
    ) -> Option<Self> {
        let (pid, window_id) = (target.pid?, target.window_id?);
        let before = read_observation(&path).await.and_then(|o| o.timestamp);
        let follow = Follow {
            live,
            key,
            pid,
            window_id,
            kind,
            path,
            before,
        };
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut last = None;
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    () = tokio::time::sleep(FOLLOW_EVERY) => {
                        last = follow.publish(last, false).await.or(last);
                    }
                }
            }
            follow.publish(last, true).await;
        });
        Some(Self {
            stop: Some(stop),
            task,
        })
    }

    pub(crate) async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            // silent-ok: the task may have ended already.
            let _ = stop.send(());
        }
        if tokio::time::timeout(Duration::from_millis(500), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }
}
