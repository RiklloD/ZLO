//! Run a gateway request off the UI thread and hand the result back to a view.

use std::future::Future;

use chrono::{DateTime, Local, Utc};
use ironclaw_gpui_client::{Backend, Client, ClientError};

use crate::ui::*;

pub fn flatten<T>(
    joined: Result<Result<T, ClientError>, tokio::task::JoinError>,
) -> Result<T, ClientError> {
    joined.map_err(|e| ClientError::Config(format!("background task failed: {e}")))?
}

/// Every failed gateway request lands in the desktop log, named by its view.
fn logged<V, T>(result: Result<T, ClientError>) -> Result<T, ClientError> {
    if let Err(e) = &result {
        let view = std::any::type_name::<V>()
            .rsplit("::")
            .next()
            .unwrap_or("view");
        tracing::warn!(view, error = %e, "gateway request failed");
    }
    result
}

/// `f` runs on the network runtime; `done` runs on the UI thread if the view
/// still exists. The task is detached: it holds only a weak handle, and
/// dropping it would cancel a request already in flight.
pub fn run<V, F, Fut, T>(
    backend: &Backend,
    cx: &mut Context<V>,
    f: F,
    done: impl FnOnce(&mut V, Result<T, ClientError>, &mut Context<V>) + 'static,
) where
    V: 'static,
    F: FnOnce(Client) -> Fut,
    Fut: Future<Output = Result<T, ClientError>> + Send + 'static,
    T: Send + 'static,
{
    let job = backend.spawn(f);
    cx.spawn(async move |this, cx| {
        let result = logged::<V, _>(flatten(job.await));
        let _ = this.update(cx, |this, cx| done(this, result, cx));
    })
    .detach();
}

/// Like [`run`], for results that need the window (building inputs, focus).
pub fn run_in<V, F, Fut, T>(
    backend: &Backend,
    window: &mut Window,
    cx: &mut Context<V>,
    f: F,
    done: impl FnOnce(&mut V, Result<T, ClientError>, &mut Window, &mut Context<V>) + 'static,
) where
    V: 'static,
    F: FnOnce(Client) -> Fut,
    Fut: Future<Output = Result<T, ClientError>> + Send + 'static,
    T: Send + 'static,
{
    let job = backend.spawn(f);
    cx.spawn_in(window, async move |this, cx| {
        let result = logged::<V, _>(flatten(job.await));
        let _ = this.update_in(cx, |this, window, cx| done(this, result, window, cx));
    })
    .detach();
}

/// "just now", "5 min ago", "3 h ago", then a local date.
pub fn ago(at: DateTime<Utc>) -> String {
    let secs = (Utc::now() - at).num_seconds();
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        _ => when(at),
    }
}

/// Local wall-clock time, e.g. "23.10. 14:05".
pub fn when(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%d.%m. %H:%M").to_string()
}

pub fn size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.),
        b => format!("{:.1} MB", b as f64 / (1024. * 1024.)),
    }
}
