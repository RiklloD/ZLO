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
/// When leaving a Failed/Connecting screen, the handover is deferred through
/// `Rt::deliver` so `workspace::connected`'s crossfade runs on a live `Tx`.
pub fn focus(s: &mut State, key: &str) {
    s.remote.pending_thread = None;
    if is_ready_focused(s, key) {
        return;
    }
    if !can_handover(s, key) {
        connect::switch_to(s, active_of_key(key));
        return;
    }
    if matches!(s.conn, Conn::Ready) {
        warm(s, key, None);
        return;
    }
    // Deferred: a navigation or connect attempt started meanwhile wins.
    let (key, asked) = (key.to_string(), s.workspace.connect_gen);
    s.rt.deliver(move |s, tx| {
        if s.workspace.connect_gen != asked {
            return false;
        }
        if can_handover(s, &key) {
            handover(s, &key, tx);
        } else if !is_ready_focused(s, &key) {
            connect::switch_to(s, active_of_key(&key));
        }
        true
    });
}

/// Focus `key`, then open `thread_id` once the focus has landed (in the
/// same step on a handover or when `key` is already focused). The intent is
/// bound to the connect attempt it starts: a failure or a newer attempt
/// drops it (`PendingThread::live`).
pub fn focus_thread(s: &mut State, key: &str, thread_id: String) {
    s.remote.pending_thread = None;
    if is_ready_focused(s, key) {
        crate::workspace::open_thread(s, thread_id);
        return;
    }
    if !can_handover(s, key) {
        cold_thread(s, key, thread_id);
        return;
    }
    if matches!(s.conn, Conn::Ready) {
        warm(s, key, Some(thread_id));
        return;
    }
    let (key, asked) = (key.to_string(), s.workspace.connect_gen);
    s.rt.deliver(move |s, tx| {
        if s.workspace.connect_gen != asked {
            return false;
        }
        if can_handover(s, &key) {
            s.remote.pending_thread = Some(PendingThread {
                computer: key.clone(),
                thread_id,
                attempt: None,
            });
            handover(s, &key, tx);
            s.remote.pending_thread = None;
        } else if is_ready_focused(s, &key) {
            crate::workspace::open_thread(s, thread_id);
        } else {
            cold_thread(s, &key, thread_id);
        }
        true
    });
}

/// A cold switch carrying a thread to open: the intent rides on the attempt
/// the switch starts, and is not set when the switch ended at once.
fn cold_thread(s: &mut State, key: &str, thread_id: String) {
    connect::switch_to(s, active_of_key(key));
    if s.remote.switching.is_some() || !matches!(s.conn, Conn::Ready) {
        s.remote.pending_thread = Some(PendingThread {
            computer: key.to_string(),
            thread_id,
            attempt: Some(s.workspace.connect_gen),
        });
    }
}

/// A handover from a working screen, in this step. It has no live `Tx`
/// (callers are plain handlers), so the wake its demoted peer asked for
/// went nowhere: forget it, and the next poll asks again.
fn warm(s: &mut State, key: &str, thread_id: Option<String>) {
    s.remote.pending_thread = thread_id.map(|thread_id| PendingThread {
        computer: key.to_string(),
        thread_id,
        attempt: None,
    });
    let mut tx = Tx::default();
    handover(s, key, &mut tx);
    s.remote.pending_thread = None;
    s.remote.peers.forget_wake();
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
    let old_unread = s.workspace.focused_unread;
    let peer_unread = peer.unread_count;
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
        demoted.unread_count = old_unread;
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
    crate::workspace::set_unread(s, peer_unread);
    // Restore any still-unconfirmed sent prompts for the newly focused computer.
    if !peer_sent.is_empty() {
        prune_sent(&mut peer_sent, &s.workspace.inbox.list);
        s.workspace.inbox.sent.extend(peer_sent);
        // `inbox::apply` armed its poll before these were back.
        crate::workspace::inbox::sent_restored(s);
    }
    // 4. Refresh a demoted computer immediately when it has unconfirmed sent work.
    if let Some(demoted_key) = demoted_sent_key {
        io::refresh_peer(s, &demoted_key);
    }
}
