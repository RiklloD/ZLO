//! Background work and the way back to the UI thread.
//!
//! Rune runs every handler on the UI thread with `&mut State`. Anything slow
//! (gateway requests, file reads, timers) runs elsewhere and lands through
//! the app's [`AppHandle`], whose `update` queues a closure that runs on the
//! UI thread with `&mut State` before the next frame.
//!
//! This replaces the GPUI app's `jobs::run(backend, cx, f, done)`: there is
//! no per-view entity any more, so `done` receives the whole `State` and must
//! find its own sub-state again (and check it is still the one that asked,
//! e.g. the same thread id; see [`Generation`]).

use std::future::Future;
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use ironclaw_gpui_client::{Backend, Client, ClientError};
use rune::{AppHandle, Tx};

use crate::state::State;

/// The runtime services every handler can reach through `State::rt`.
#[derive(Clone)]
pub struct Rt {
    /// Queue work onto the UI thread from any thread.
    pub handle: AppHandle<State>,
    /// The gateway, once connected.
    pub backend: Option<Backend>,
}

impl Rt {
    pub fn new(handle: AppHandle<State>) -> Self {
        Self {
            handle,
            backend: None,
        }
    }
}

pub fn flatten<T>(
    joined: Result<Result<T, ClientError>, tokio::task::JoinError>,
) -> Result<T, ClientError> {
    joined.map_err(|e| ClientError::Config(format!("background task failed: {e}")))?
}

/// `f` runs on the network runtime; `done` runs on the UI thread with the
/// whole state. `label` names the caller in the log when the request fails.
/// Without a backend (not connected yet) `done` gets a `Config` error at once,
/// on the next frame.
pub fn run<F, Fut, T>(
    s: &State,
    label: &'static str,
    f: F,
    done: impl FnOnce(&mut State, Result<T, ClientError>, &mut Tx) + Send + 'static,
) where
    F: FnOnce(Client) -> Fut,
    Fut: Future<Output = Result<T, ClientError>> + Send + 'static,
    T: Send + 'static,
{
    let handle = s.rt.handle.clone();
    let Some(backend) = s.rt.backend.as_ref() else {
        handle.update(move |s, tx| {
            if s.fixture {
                return;
            }
            done(
                s,
                Err(ClientError::Config("not connected to the gateway".into())),
                tx,
            )
        });
        return;
    };
    let job = backend.spawn(f);
    backend.spawn(move |_| async move {
        let result = flatten(job.await);
        if let Err(e) = &result {
            tracing::warn!(view = label, error = %e, "gateway request failed");
        }
        handle.update(move |s, tx| {
            if !s.fixture {
                done(s, result, tx)
            }
        });
    });
}

/// Run blocking work (disk, decoding) on a plain thread, then land it.
pub fn blocking<T: Send + 'static>(
    s: &State,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&mut State, T, &mut Tx) + Send + 'static,
) {
    let handle = s.rt.handle.clone();
    std::thread::spawn(move || {
        let out = work();
        handle.update(move |s, tx| {
            if !s.fixture {
                done(s, out, tx)
            }
        });
    });
}

/// Run `f` on the UI thread after `delay`. For polls and retries; check in
/// `f` that the state still wants it (a [`Generation`] does that cheaply).
/// For per-frame or debounce timing prefer `tx.wake_after` plus `poll`.
pub fn after(
    s: &State,
    delay: Duration,
    f: impl FnOnce(&mut State, &mut Tx) + Send + 'static,
) {
    let handle = s.rt.handle.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        handle.update(move |s, tx| {
            if !s.fixture {
                f(s, tx)
            }
        });
    });
}

/// A counter that invalidates earlier async replies: bump it when the thing
/// that asked goes away (thread switched, page closed, query changed), and
/// compare the captured value in `done`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Generation(pub u64);

impl Generation {
    pub fn bump(&mut self) -> Self {
        self.0 += 1;
        *self
    }
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
