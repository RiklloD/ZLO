//! Background connections to every saved computer other than the focused one
//! (plus This Mac when its backend may run and it is not focused).
//!
//! Each peer holds a warm [`Backend`], its [`SessionInfo`], its last
//! [`InboxListResponse`] and its unread notifications page. Focusing a ready
//! peer hands that warm connection over to the focused slot in one UI-thread
//! step with no network requests; the old focused connection becomes a peer.
//!
//! Energy rule (`README.md` rule 4): no standing timers. A peer's inbox and
//! notifications are read when it connects, on [`refresh`] events, and on a
//! 5 s poll only while one of its rows is working and the window is in front.
//! Retries stop while the battery saver holds or the window is hidden and
//! resume on focus.

use std::future::Future;
use std::time::{Duration, Instant};

use rune::Tx;
use zlo_client::computer_use::ComputerUseReadiness;
use zlo_client::environment_access::EnvironmentDescriptor;
use zlo_client::environments::{Active, THIS_MAC};
use zlo_client::inbox::{InboxListResponse, InboxStatus};
use zlo_client::notification_inbox::ProductNotification;
use zlo_client::wire::SessionInfo;
use zlo_client::{Backend, Client, ClientError};

use super::connect::{self, Mode};
use crate::jobs::Generation;
use crate::onboarding::gateway::Computer;
use crate::state::{Conn, State};

#[path = "peers_io.rs"]
mod io;

/// Working-row poll period (matches `workspace::inbox::POLL`).
const POLL: Duration = Duration::from_secs(5);

/// Live state of one background peer connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerConn {
    Connecting,
    Ready,
    Asleep,
    Failed(String),
}

/// One non-focused computer kept warm in the background.
pub struct Peer {
    pub computer: Computer,
    pub conn: PeerConn,
    pub session: Option<SessionInfo>,
    pub inbox: Option<InboxListResponse>,
    pub notifications: Vec<ProductNotification>,
    /// Cached computer-use readiness, probed on demand (never on a timer).
    pub computer_readiness: Option<ComputerUseReadiness>,
    computer_readiness_probed: bool,
    backend: Option<Backend>,
    /// Held on the UI thread while initial session/inbox read is in flight,
    /// so its runtime is never dropped on one of its own worker threads.
    pending_backend: Option<Backend>,
    descriptor: Option<EnvironmentDescriptor>,
    tunnel: Option<String>,
    route_rank: usize,
    generation: Generation,
    retry_attempt: usize,
    retry_at: Option<Instant>,
    loading: bool,
    stale: bool,
    poll_at: Option<Instant>,
}

impl Peer {
    fn new(computer: Computer, conn: PeerConn) -> Self {
        Self {
            computer,
            conn,
            session: None,
            inbox: None,
            notifications: Vec::new(),
            computer_readiness: None,
            computer_readiness_probed: false,
            backend: None,
            pending_backend: None,
            descriptor: None,
            tunnel: None,
            route_rank: 0,
            generation: Generation::unique(),
            retry_attempt: 0,
            retry_at: None,
            loading: false,
            stale: false,
            poll_at: None,
        }
    }

    /// A peer with no live backend, for headless scenes and fixtures.
    pub fn fixture(computer: Computer, conn: PeerConn) -> Self {
        Self::new(computer, conn)
    }

    fn key(&self) -> String {
        self.computer.key()
    }
}

/// A computer option for menus (the focused computer first, then each peer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComputerChoice {
    pub key: String,
    pub label: String,
    pub conn: PeerConn,
    pub ready: bool,
    pub this_mac: bool,
}

/// All background peers, ordered as the registry lists them (This Mac first
/// when it is a peer, then `registry.environments` in registry order).
#[derive(Default)]
pub struct Peers {
    entries: Vec<Peer>,
}

impl Peers {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Insert or replace a peer directly (for headless scenes and fixtures).
    pub fn insert_fixture(&mut self, peer: Peer) {
        let key = peer.key();
        if let Some(slot) = self.entries.iter_mut().find(|p| p.key() == key) {
            *slot = peer;
        } else {
            self.entries.push(peer);
        }
    }

    fn get(&self, key: &str) -> Option<&Peer> {
        self.entries.iter().find(|p| p.key() == key)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut Peer> {
        self.entries.iter_mut().find(|p| p.key() == key)
    }

    fn take(&mut self, key: &str) -> Option<Peer> {
        let pos = self.entries.iter().position(|p| p.key() == key)?;
        Some(self.entries.remove(pos))
    }

    fn upsert(&mut self, peer: Peer) {
        let key = peer.key();
        if let Some(slot) = self.entries.iter_mut().find(|p| p.key() == key) {
            *slot = peer;
        } else {
            self.entries.push(peer);
        }
    }

    pub(super) fn tunnel_keys(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter_map(|p| p.tunnel.as_deref())
            .collect()
    }
}

// ── public API ───────────────────────────────────────────────────────────

/// Reconcile the peer set with the registry and the focus: one peer per saved
/// computer that is not focused, plus This Mac when its backend may run and
/// it is not focused. Starts missing connections and drops removed ones on
/// the UI thread.
pub fn sync(s: &mut State) {
    let desired = desired_peers(s);
    let mut existing = std::mem::take(&mut s.remote.peers.entries);
    let mut next = Vec::with_capacity(desired.len());
    let mut to_connect: Vec<String> = Vec::new();

    for computer in desired {
        let key = computer.key();
        let is_asleep = super::asleep(s, &key);
        if let Some(pos) = existing.iter().position(|p| p.key() == key) {
            let mut peer = existing.swap_remove(pos);
            // Follow relabels from the registry while keeping the connection.
            peer.computer = computer;
            if is_asleep && peer.conn != PeerConn::Ready {
                if peer.conn != PeerConn::Asleep {
                    peer.generation.bump();
                    peer.conn = PeerConn::Asleep;
                    peer.pending_backend = None;
                    peer.retry_at = None;
                }
            } else if !is_asleep && peer.conn == PeerConn::Asleep {
                peer.conn = PeerConn::Connecting;
                peer.retry_attempt = 0;
                peer.retry_at = None;
                to_connect.push(key);
            }
            next.push(peer);
        } else {
            let conn = if is_asleep {
                PeerConn::Asleep
            } else {
                PeerConn::Connecting
            };
            if conn == PeerConn::Connecting {
                to_connect.push(key);
            }
            next.push(Peer::new(computer, conn));
        }
    }

    let dropped_any = !existing.is_empty();
    // Removed peers (and their `Backend` handles) drop here on the UI thread.
    drop(existing);
    s.remote.peers.entries = next;
    if dropped_any {
        super::retain_tunnels(s);
    }
    crate::workspace::sync_unread(s);

    if s.fixture {
        return;
    }
    for key in to_connect {
        io::connect_peer(s, &key);
    }
}

/// Re-read every ready peer's inbox and notifications now.
pub fn refresh(s: &mut State) {
    if s.fixture || s.remote.peers.is_empty() {
        return;
    }
    let keys: Vec<String> = s
        .remote
        .peers
        .entries
        .iter()
        .filter(|p| p.conn == PeerConn::Ready && p.backend.is_some())
        .map(Peer::key)
        .collect();
    for key in keys {
        io::refresh_peer(s, &key);
    }
}

/// Re-read one ready peer's inbox and notifications now.
pub fn refresh_one(s: &mut State, key: &str) {
    if s.fixture {
        return;
    }
    io::refresh_peer(s, key);
}

/// Apply an updated inbox list returned by a mark on `key` and re-arm its
/// working-row poll when needed.
pub fn apply_inbox(s: &mut State, key: &str, list: InboxListResponse, tx: &mut Tx) {
    let arm_poll = s.workspace.window_active && !crate::design::look::saving(s);
    let Some(peer) = s.remote.peers.get_mut(key) else {
        return;
    };
    peer.inbox = Some(list);
    if arm_poll && peer_needs_poll(peer) {
        if peer.poll_at.is_none() {
            peer.poll_at = Some(Instant::now() + POLL);
            tx.wake_after(POLL);
        }
    } else {
        peer.poll_at = None;
    }
}

/// Remove a deleted thread from `key`'s cached inbox list.
pub fn remove_row(s: &mut State, key: &str, thread_id: &str) {
    if let Some(peer) = s.remote.peers.get_mut(key)
        && let Some(inbox) = peer.inbox.as_mut()
    {
        inbox.rows.retain(|r| r.thread_id != thread_id);
    }
}

/// The module's share of the app poll: retries and the working-row poll.
/// Returns `true` when peer state changed.
pub fn poll(s: &mut State, tx: &mut Tx) -> bool {
    if s.fixture || s.remote.peers.is_empty() {
        return false;
    }
    let now = Instant::now();
    let saving = crate::design::look::saving(s);
    let active = s.workspace.window_active;

    let mut to_retry: Vec<String> = Vec::new();
    let mut to_refresh: Vec<String> = Vec::new();
    let mut next_wake: Option<Instant> = None;

    for peer in &mut s.remote.peers.entries {
        // Retries stop while the battery saver holds or the window is hidden,
        // and resume on focus.
        if let Some(at) = peer.retry_at
            && active
            && !saving
        {
            if at <= now {
                peer.retry_at = None;
                to_retry.push(peer.key());
            } else {
                next_wake = Some(next_wake.map_or(at, |n| n.min(at)));
            }
        }

        // Working-row poll: 5 s only while a row is working and the window is
        // in front (and not battery-saving).
        if peer.conn == PeerConn::Ready && peer.backend.is_some() {
            let needs = active && !saving && peer_needs_poll(peer);
            if !needs {
                peer.poll_at = None;
            } else {
                match peer.poll_at {
                    None => {
                        let at = now + POLL;
                        peer.poll_at = Some(at);
                        next_wake = Some(next_wake.map_or(at, |n| n.min(at)));
                    }
                    Some(at) if at <= now => {
                        let next = now + POLL;
                        peer.poll_at = Some(next);
                        next_wake = Some(next_wake.map_or(next, |n| n.min(next)));
                        to_refresh.push(peer.key());
                    }
                    Some(at) => {
                        next_wake = Some(next_wake.map_or(at, |n| n.min(at)));
                    }
                }
            }
        } else {
            peer.poll_at = None;
        }
    }

    let changed = !to_retry.is_empty();
    for key in to_retry {
        io::connect_peer(s, &key);
    }
    for key in to_refresh {
        io::refresh_peer(s, &key);
    }

    if let Some(next) = next_wake {
        tx.wake_after(
            next.saturating_duration_since(now)
                .max(Duration::from_millis(5)),
        );
    }
    changed
}

/// Peers in registry order.
pub fn list(s: &State) -> Vec<&Peer> {
    s.remote.peers.entries.iter().collect()
}

/// One peer by computer key (`this-mac` or an `environment_id`).
pub fn get<'a>(s: &'a State, key: &str) -> Option<&'a Peer> {
    s.remote.peers.get(key)
}

/// Mark `key` as having had its computer-use readiness probed for the current
/// connection. Returns `true` the first time for a ready peer so callers probe
/// at most once per peer connection.
pub fn claim_computer_readiness_probe(s: &mut State, key: &str) -> bool {
    let Some(peer) = s.remote.peers.get_mut(key) else {
        return false;
    };
    if peer.conn != PeerConn::Ready || peer.computer_readiness_probed {
        return false;
    }
    peer.computer_readiness_probed = true;
    true
}

/// Record a peer's cached computer-use readiness.
pub fn set_computer_readiness(s: &mut State, key: &str, readiness: ComputerUseReadiness) {
    if let Some(peer) = s.remote.peers.get_mut(key) {
        peer.computer_readiness = Some(readiness);
        peer.computer_readiness_probed = true;
    }
}

/// Run a gateway request on a peer's client (`jobs::run` for a peer).
/// `done` is dropped if that peer's connection has changed since (generation).
pub fn run<F, Fut, T>(
    s: &State,
    key: &str,
    label: &'static str,
    f: F,
    done: impl FnOnce(&mut State, Result<T, ClientError>, &mut Tx) + Send + 'static,
) where
    F: FnOnce(Client) -> Fut,
    Fut: Future<Output = Result<T, ClientError>> + Send + 'static,
    T: Send + 'static,
{
    if s.fixture {
        return;
    }
    let rt = s.rt.clone();
    let key_owned = key.to_string();
    let Some(peer) = s.remote.peers.get(key) else {
        rt.deliver(move |s, tx| {
            if s.fixture || s.remote.peers.get(&key_owned).is_some() {
                return false;
            }
            done(
                s,
                Err(ClientError::Config("peer is not connected".into())),
                tx,
            );
            true
        });
        return;
    };
    let generation = peer.generation;
    let Some(backend) = peer.backend.as_ref() else {
        rt.deliver(move |s, tx| {
            if s.fixture
                || !s
                    .remote
                    .peers
                    .get(&key_owned)
                    .is_some_and(|p| p.generation == generation)
            {
                return false;
            }
            done(
                s,
                Err(ClientError::Config("peer is not connected".into())),
                tx,
            );
            true
        });
        return;
    };
    let job = backend.spawn(f);
    let land = rt.clone();
    backend.spawn(move |_| async move {
        let result = crate::jobs::flatten(job.await);
        if let Err(e) = &result {
            tracing::warn!(view = label, computer = %key_owned, error = %e, "peer gateway request failed");
        }
        land.deliver(move |s, tx| {
            crate::energy::hit(crate::energy::C::UpdateJob);
            if s.fixture {
                return false;
            }
            if s.remote
                .peers
                .get(&key_owned)
                .is_some_and(|p| p.generation == generation)
            {
                done(s, result, tx);
                true
            } else {
                tracing::debug!(
                    view = label,
                    computer = %key_owned,
                    "reply from an earlier peer connection dropped"
                );
                false
            }
        });
    });
}

/// Move the focus to `key`. Ready peer: instant handover on the UI thread
/// with no network requests. Otherwise the existing `connect::switch_to`.
pub fn focus(s: &mut State, key: &str) {
    if is_ready_focused(s, key) {
        return;
    }
    if can_handover(s, key) {
        handover(s, key);
        return;
    }
    connect::switch_to(s, active_of_key(key));
}

/// Focus `key`, then open `thread_id` once the focus has landed (immediately
/// on a handover or if `key` is already focused).
pub fn focus_thread(s: &mut State, key: &str, thread_id: String) {
    if is_ready_focused(s, key) {
        crate::workspace::open_thread(s, thread_id);
        return;
    }
    s.remote
        .last_thread
        .insert(key.to_string(), thread_id.clone());
    if can_handover(s, key) {
        handover(s, key);
        if crate::workspace::active_id(s) != Some(thread_id.as_str()) {
            crate::workspace::open_thread(s, thread_id);
        }
        return;
    }
    connect::switch_to(s, active_of_key(key));
}

/// The focused computer first, then every peer in registry order. For menus.
pub fn usable(s: &State) -> Vec<ComputerChoice> {
    let focused_conn = match &s.conn {
        Conn::Ready => PeerConn::Ready,
        Conn::Connecting => PeerConn::Connecting,
        Conn::Failed { message, .. } => PeerConn::Failed(super::first_line(message)),
    };
    let mut out = Vec::with_capacity(1 + s.remote.peers.len());
    out.push(ComputerChoice {
        key: s.onboarding.computer.key(),
        label: s.onboarding.computer.label(),
        ready: focused_conn == PeerConn::Ready,
        conn: focused_conn,
        this_mac: matches!(s.onboarding.computer, Computer::ThisMac),
    });
    for peer in &s.remote.peers.entries {
        out.push(ComputerChoice {
            key: peer.key(),
            label: peer.computer.label(),
            ready: peer.conn == PeerConn::Ready,
            conn: peer.conn.clone(),
            this_mac: matches!(peer.computer, Computer::ThisMac),
        });
    }
    out
}

// ── handover ─────────────────────────────────────────────────────────────

fn is_ready_focused(s: &State, key: &str) -> bool {
    s.onboarding.computer.key() == key
        && s.remote.switching.is_none()
        && matches!(s.conn, Conn::Ready)
}

fn can_handover(s: &State, key: &str) -> bool {
    s.remote.peers.get(key).is_some_and(|p| {
        p.conn == PeerConn::Ready && p.backend.is_some() && p.session.is_some() && p.inbox.is_some()
    })
}

fn handover(s: &mut State, key: &str) {
    // Cancel any connect or switch attempt in flight.
    s.workspace.connect_gen.bump();
    s.remote.page.pending_backend = None;
    s.remote.switching = None;

    let Some(mut peer) = s.remote.peers.take(key) else {
        return;
    };
    let (Some(peer_backend), Some(peer_session), Some(peer_inbox)) =
        (peer.backend.take(), peer.session.take(), peer.inbox.take())
    else {
        return;
    };

    // 1. Remember the open thread for the old computer.
    let old_key = s.onboarding.computer.key();
    if let Some(id) = crate::workspace::active_id(s) {
        s.remote.last_thread.insert(old_key, id.to_string());
    }

    // 2. The old focused connection becomes a ready peer (kept by `sync` if
    //    it belongs in the peer set).
    let old_backend = s.rt.backend.take();
    let old_session = s.session.take();
    let old_inbox = s.workspace.inbox.list.clone();
    let old_computer = s.onboarding.computer.clone();
    let old_descriptor = s.remote.descriptor.take();
    let old_tunnel = s.remote.tunnel.take();
    let old_route_rank = s.remote.route_rank;
    let old_readiness = s.remote.focused_computer_readiness.take();
    let peer_readiness = peer.computer_readiness.take();
    if matches!(s.conn, Conn::Ready)
        && let (Some(b), Some(sess)) = (old_backend, old_session)
    {
        let mut demoted = Peer::new(old_computer, PeerConn::Ready);
        demoted.backend = Some(b);
        demoted.session = Some(sess);
        demoted.inbox = Some(old_inbox);
        demoted.computer_readiness = old_readiness;
        demoted.computer_readiness_probed = demoted.computer_readiness.is_some();
        demoted.descriptor = old_descriptor;
        demoted.tunnel = old_tunnel;
        demoted.route_rank = old_route_rank;
        if s.workspace.window_active && !crate::design::look::saving(s) && peer_needs_poll(&demoted)
        {
            demoted.poll_at = Some(Instant::now() + POLL);
        }
        s.remote.peers.upsert(demoted);
    }

    // 3. Install the peer's connection through `connect::switched` (`Mode::Switch`).
    s.remote.registry.active = active_of_key(key);
    s.remote.legacy_active = false;
    s.remote.tunnel = peer.tunnel.take();
    s.remote.route_rank = peer.route_rank;
    if let Err(e) = super::save_registry(s) {
        tracing::warn!(error = %e, "computer pick not saved");
    }
    let mut tx = Tx::default();
    connect::switched(
        s,
        Mode::Switch,
        peer_backend,
        peer.computer,
        peer.descriptor.take(),
        peer_session,
        peer_inbox,
        &mut tx,
    );
    s.remote.focused_computer_readiness = peer_readiness;
    // 4. `connect::switched` runs `peers::sync(s)` and reopens `last_thread`.
}

// ── connecting and refreshing peers ──────────────────────────────────────

fn desired_peers(s: &State) -> Vec<Computer> {
    if s.remote.registry_error.is_some() || s.remote.legacy_active || super::env_override() {
        return Vec::new();
    }
    let focused = focused_key(s);
    let mut out = Vec::new();
    if focused != THIS_MAC && super::runs_local_backend(s) {
        out.push(Computer::ThisMac);
    }
    for env in &s.remote.registry.environments {
        if env.environment_id != focused {
            out.push(Computer::remote(&env.environment_id, &env.label));
        }
    }
    out
}

fn focused_key(s: &State) -> String {
    if let Some(sw) = &s.remote.switching {
        super::active_key(&sw.previous)
    } else {
        super::active_key(&s.remote.registry.active)
    }
}

fn active_of_key(key: &str) -> Active {
    if key == THIS_MAC {
        Active::ThisMac
    } else {
        Active::Environment(key.to_string())
    }
}

fn peer_needs_poll(peer: &Peer) -> bool {
    peer.inbox.as_ref().is_some_and(|list| {
        list.rows.iter().any(|r| {
            r.title_pending
                || matches!(
                    r.status,
                    Some(InboxStatus::Working | InboxStatus::Background | InboxStatus::NeedsInput)
                )
        })
    })
}

#[cfg(test)]
#[path = "peers_tests.rs"]
mod tests;
