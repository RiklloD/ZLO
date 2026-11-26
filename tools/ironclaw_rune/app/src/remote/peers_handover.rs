//! Moving focus between computers: warm handover on the UI thread when the
//! target peer is already ready, or falling back to `connect::switch_to`.

use std::time::Instant;

use rune::Tx;

use super::io;
use super::{Peer, PeerConn, active_of_key, peer_needs_poll, peer_poll_period, prune_sent};
use crate::remote::PendingThread;
use crate::remote::connect::{self, Mode};
use crate::state::{Conn, State};

/// Move the focus to `key`. Ready peer: instant handover on the UI thread
/// with no network requests. Otherwise the existing `connect::switch_to`.
/// When leaving a Failed/Connecting screen without a `Tx`, defers handover
/// through `Rt::deliver` so `workspace::connected`'s crossfade runs on a live
/// `Tx`.
pub fn focus(s: &mut State, key: &str) {
    s.remote.pending_thread = None;
    if is_ready_focused(s, key) {
        return;
    }
    if can_handover(s, key) {
        if !matches!(s.conn, Conn::Ready) {
            let key_owned = key.to_string();
            s.rt.deliver(move |s, tx| {
                focus_with_tx(s, &key_owned, tx);
                true
            });
            return;
        }
        let mut tx = Tx::default();
        handover(s, key, &mut tx);
        return;
    }
    connect::switch_to(s, active_of_key(key));
}

/// [`focus`] when the caller already holds a live `Tx`.
fn focus_with_tx(s: &mut State, key: &str, tx: &mut Tx) {
    s.remote.pending_thread = None;
    if is_ready_focused(s, key) {
        return;
    }
    if can_handover(s, key) {
        handover(s, key, tx);
        return;
    }
    connect::switch_to(s, active_of_key(key));
}

/// Focus `key`, then open `thread_id` once the focus has landed (immediately
/// on a handover or if `key` is already focused).
pub fn focus_thread(s: &mut State, key: &str, thread_id: String) {
    if is_ready_focused(s, key) {
        s.remote.pending_thread = None;
        crate::workspace::open_thread(s, thread_id);
        return;
    }
    s.remote.pending_thread = Some(PendingThread {
        computer: key.to_string(),
        thread_id: thread_id.clone(),
    });
    s.remote.last_thread.insert(key.to_string(), thread_id);
    if can_handover(s, key) {
        if !matches!(s.conn, Conn::Ready) {
            let key_owned = key.to_string();
            s.rt.deliver(move |s, tx| {
                if can_handover(s, &key_owned) {
                    handover(s, &key_owned, tx);
                } else if !is_ready_focused(s, &key_owned) {
                    connect::switch_to(s, active_of_key(&key_owned));
                }
                true
            });
            return;
        }
        let mut tx = Tx::default();
        handover(s, key, &mut tx);
        return;
    }
    connect::switch_to(s, active_of_key(key));
}

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

fn handover(s: &mut State, key: &str, tx: &mut Tx) {
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
        s.remote.last_thread.insert(old_key.clone(), id.to_string());
    }

    // 2. The old focused connection becomes a ready peer (kept by `sync` if
    //    it belongs in the peer set). Carry any unconfirmed sent prompts with
    //    it so it refreshes and polls until the backend confirms them.
    let old_backend = s.rt.backend.take();
    let old_session = s.session.take();
    let old_inbox = s.workspace.inbox.list.clone();
    let mut old_sent = std::mem::take(&mut s.workspace.inbox.sent);
    old_sent.retain(|_, sent| sent.live());
    let old_computer = s.onboarding.computer.clone();
    let old_descriptor = s.remote.descriptor.take();
    let old_tunnel = s.remote.tunnel.take();
    let old_route_rank = s.remote.route_rank;
    let old_readiness = s.remote.focused_computer_readiness.take();
    let peer_readiness = peer.computer_readiness.take();
    let mut peer_sent = std::mem::take(&mut peer.sent);
    peer_sent.retain(|_, sent| sent.live());
    let mut demoted_sent_key = None;
    if matches!(s.conn, Conn::Ready)
        && let (Some(b), Some(sess)) = (old_backend, old_session)
    {
        let mut demoted = Peer::new(old_computer, PeerConn::Ready);
        demoted.backend = Some(b);
        demoted.session = Some(sess);
        demoted.inbox = Some(old_inbox);
        demoted.sent = old_sent;
        demoted.computer_readiness = old_readiness;
        demoted.computer_readiness_probed = demoted.computer_readiness.is_some();
        demoted.descriptor = old_descriptor;
        demoted.tunnel = old_tunnel;
        demoted.route_rank = old_route_rank;
        if !demoted.sent.is_empty() {
            demoted_sent_key = Some(demoted.key());
        }
        if s.workspace.window_active && !crate::design::look::saving(s) && peer_needs_poll(&demoted)
        {
            let now = Instant::now();
            let at = now + peer_poll_period(&demoted);
            demoted.poll_at = Some(at);
            s.remote.peers.request_wake(at, now, tx);
        }
        s.remote.peers.upsert(demoted);
    }

    // 3. Install the peer's connection through `connect::switched` (`Mode::Switch`).
    s.remote.registry.active = active_of_key(key);
    s.remote.legacy_active = false;
    s.remote.tunnel = peer.tunnel.take();
    s.remote.route_rank = peer.route_rank;
    if let Err(e) = crate::remote::save_registry(s) {
        tracing::warn!(error = %e, "computer pick not saved");
    }
    connect::switched(
        s,
        Mode::Switch,
        peer_backend,
        peer.computer,
        peer.descriptor.take(),
        peer_session,
        peer_inbox,
        tx,
    );
    s.remote.focused_computer_readiness = peer_readiness;
    // Restore any still-unconfirmed sent prompts for the newly focused computer.
    if !peer_sent.is_empty() {
        prune_sent(&mut peer_sent, &s.workspace.inbox.list);
        s.workspace.inbox.sent.extend(peer_sent);
    }
    // 4. Refresh a demoted computer immediately when it has unconfirmed sent work.
    if let Some(demoted_key) = demoted_sent_key {
        io::refresh_peer(s, &demoted_key);
    }
}
