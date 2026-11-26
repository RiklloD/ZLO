//! Async network lifecycle for background peers: resolving routes (reusing
//! `connect::resolve`), reading initial session/inbox/notifications, handling
//! failures/backoff, and refreshing ready peers.

use std::time::{Duration, Instant};

use rune::Tx;
use zlo_client::environments::THIS_MAC;
use zlo_client::{Backend, Client, ClientError};

use super::{PeerConn, peer_needs_poll, peer_poll_period, prune_sent};
use crate::jobs::Generation;
use crate::onboarding::gateway::Computer;
use crate::remote::connect::{self, Problem, Resolved, Target};
use crate::state::State;

/// Unread notifications page size (matches `workspace::ALERT_PAGE`).
const ALERT_PAGE: u32 = 20;
/// Unreachable retry backoff in seconds, capped at 60 s (matches
/// `workspace::CONNECT_RETRY` plus the 60 s cap).
const RETRY_BACKOFF: [u64; 6] = [1, 2, 5, 10, 30, 60];

fn peer_target(s: &State, key: &str) -> Option<(Target, bool)> {
    if key == THIS_MAC {
        if !crate::remote::runs_local_backend(s) {
            return None;
        }
        let own_backend =
            crate::remote::runs_local_backend(s) && crate::service::provisions_token();
        Some((Target::ThisMac, own_backend))
    } else {
        let env = s.remote.registry.get(key)?.clone();
        Some((Target::Environment(env), false))
    }
}

pub(super) fn connect_peer(s: &mut State, key: &str) {
    if s.fixture {
        return;
    }
    let Some((target, own_backend)) = peer_target(s, key) else {
        return;
    };
    let Some(net) = crate::remote::net() else {
        if let Some(peer) = s.remote.peers.get_mut(key) {
            peer.conn = PeerConn::Failed("ZLO could not start its network runtime.".into());
        }
        return;
    };
    let Some(peer) = s.remote.peers.get_mut(key) else {
        return;
    };
    let generation = peer.generation.bump();
    peer.conn = PeerConn::Connecting;
    peer.retry_at = None;
    peer.pending_backend = None;
    peer.computer_readiness = None;
    peer.computer_readiness_probed = false;
    let key_owned = key.to_string();
    let tokens = s.remote.tokens.clone();
    let delivery = s.rt.delivery();
    net.spawn(async move {
        let resolved = connect::resolve(target, tokens, own_backend).await;
        delivery.deliver(move |s, tx| {
            if s.fixture
                || !s
                    .remote
                    .peers
                    .get(&key_owned)
                    .is_some_and(|p| p.generation == generation)
            {
                // Superseded: whatever it opened (a tunnel, also one a failed
                // resolve dropped) has no adopter.
                drop(resolved);
                crate::remote::retain_tunnels(s);
                return false;
            }
            match resolved {
                Ok(r) => open_peer(s, &key_owned, generation, r, tx),
                Err(p) => {
                    peer_failed(s, &key_owned, p, tx);
                    // A tunnel the failed attempt opened has no adopter.
                    crate::remote::retain_tunnels(s);
                }
            }
            true
        });
    });
}

fn open_peer(s: &mut State, key: &str, generation: Generation, r: Resolved, tx: &mut Tx) {
    let backend = match Client::new(r.conn).and_then(Backend::new) {
        Ok(b) => b,
        Err(e) => {
            drop(r.tunnel);
            peer_failed(s, key, Problem::Blocked(e.to_string()), tx);
            crate::remote::retain_tunnels(s);
            return;
        }
    };
    let Some(peer) = s.remote.peers.get_mut(key) else {
        drop(r.tunnel);
        crate::remote::retain_tunnels(s);
        return;
    };
    peer.pending_backend = Some(backend.clone());
    let key_owned = key.to_string();
    let delivery = s.rt.delivery();
    let computer = r.computer;
    let descriptor = r.descriptor;
    let tunnel = r.tunnel;
    let route_rank = r.route_rank;
    let job = backend.spawn(|c| async move {
        let session = c.session().await?;
        let inbox = c.inbox().await?;
        let (notifications, unread_count) = match c.list_notifications(ALERT_PAGE).await {
            Ok(page) => (page.notifications, page.unread_count),
            Err(e) => {
                tracing::debug!(error = %e, "peer notifications read failed");
                (Vec::new(), 0)
            }
        };
        Ok::<_, ClientError>((session, inbox, notifications, unread_count))
    });
    backend.spawn(move |_| async move {
        let result = crate::jobs::flatten(job.await);
        delivery.deliver(move |s, tx| {
            if s.fixture
                || !s
                    .remote
                    .peers
                    .get(&key_owned)
                    .is_some_and(|p| p.generation == generation)
            {
                let had_tunnel = tunnel.is_some();
                drop(tunnel);
                if had_tunnel {
                    crate::remote::retain_tunnels(s);
                }
                return false;
            }
            let Some(backend) = s
                .remote
                .peers
                .get_mut(&key_owned)
                .and_then(|p| p.pending_backend.take())
            else {
                let had_tunnel = tunnel.is_some();
                drop(tunnel);
                if had_tunnel {
                    crate::remote::retain_tunnels(s);
                }
                return false;
            };
            match result {
                Ok((session, inbox, notifications, unread_count)) => {
                    if let (Computer::Remote(r), Some(d)) = (&computer, descriptor.as_ref())
                        && s.remote
                            .registry
                            .learn_routes(&r.environment_id, &d.addresses)
                        && let Err(e) = crate::remote::save_registry(s)
                    {
                        tracing::warn!(error = %e, "learned routes not saved");
                    }
                    let arm_poll = s.workspace.window_active && !crate::design::look::saving(s);
                    let now = Instant::now();
                    let mut wake_at = None;
                    if let Some(peer) = s.remote.peers.get_mut(&key_owned) {
                        prune_sent(&mut peer.sent, &inbox);
                        peer.computer = computer;
                        peer.conn = PeerConn::Ready;
                        peer.session = Some(session);
                        peer.inbox = Some(inbox);
                        peer.notifications = notifications;
                        peer.unread_count = unread_count;
                        peer.backend = Some(backend);
                        peer.descriptor = descriptor;
                        peer.tunnel = tunnel.as_ref().map(|l| l.key().to_string());
                        peer.route_rank = route_rank;
                        peer.retry_attempt = 0;
                        peer.retry_at = None;
                        peer.loading = false;
                        peer.stale = false;
                        if arm_poll && peer_needs_poll(peer) {
                            let at = now + peer_poll_period(peer);
                            peer.poll_at = Some(at);
                            wake_at = Some(at);
                        } else {
                            peer.poll_at = None;
                        }
                    }
                    drop(tunnel);
                    if let Some(at) = wake_at {
                        s.remote.peers.request_wake(at, now, tx);
                    }
                    tracing::debug!(computer = %key_owned, "peer connected");
                    crate::remote::retain_tunnels(s);
                    crate::workspace::peer_notifications_landed(s, &key_owned);
                }
                Err(e) => {
                    drop(tunnel);
                    let problem = connect::problem_of(&computer, &e);
                    peer_failed(s, &key_owned, problem, tx);
                    crate::remote::retain_tunnels(s);
                }
            }
            true
        });
    });
}

fn peer_failed(s: &mut State, key: &str, problem: Problem, tx: &mut Tx) {
    crate::pages::notifications::peer_connection_ended(s, key);
    let is_asleep = crate::remote::asleep(s, key);
    let can_wake = s.workspace.window_active && !crate::design::look::saving(s);
    let now = Instant::now();
    let mut wake_at = None;
    let Some(peer) = s.remote.peers.get_mut(key) else {
        return;
    };
    peer.backend = None;
    peer.pending_backend = None;
    peer.tunnel = None;
    peer.poll_at = None;
    match problem {
        Problem::Blocked(reason) => {
            tracing::warn!(computer = %key, error = %reason, "peer blocked");
            peer.conn = PeerConn::Failed(reason);
            peer.retry_at = None;
        }
        Problem::Unreachable(reason) => {
            if is_asleep {
                tracing::debug!(computer = %key, "peer asleep");
                peer.conn = PeerConn::Asleep;
                peer.retry_at = None;
            } else {
                let secs = RETRY_BACKOFF[peer.retry_attempt.min(RETRY_BACKOFF.len() - 1)];
                tracing::debug!(
                    computer = %key,
                    error = %reason,
                    attempt = peer.retry_attempt,
                    retry_in_secs = secs,
                    "peer unreachable; retrying"
                );
                peer.conn = PeerConn::Connecting;
                peer.retry_attempt += 1;
                let at = now + Duration::from_secs(secs);
                peer.retry_at = Some(at);
                if can_wake {
                    wake_at = Some(at);
                }
            }
        }
    }
    if let Some(at) = wake_at {
        s.remote.peers.request_wake(at, now, tx);
    }
    crate::workspace::sync_unread(s);
}

pub(super) fn refresh_peer(s: &mut State, key: &str) {
    if s.fixture {
        return;
    }
    let Some(peer) = s.remote.peers.get_mut(key) else {
        return;
    };
    if peer.conn != PeerConn::Ready || peer.backend.is_none() {
        return;
    }
    if peer.loading {
        peer.stale = true;
        return;
    }
    peer.loading = true;
    let generation = peer.generation;
    let Some(backend) = peer.backend.as_ref() else {
        return;
    };
    let key_owned = key.to_string();
    let delivery = s.rt.delivery();
    let job = backend.spawn(|c| async move {
        let inbox = c.inbox().await?;
        let notifications = match c.list_notifications(ALERT_PAGE).await {
            Ok(page) => Some((page.notifications, page.unread_count)),
            Err(e) => {
                tracing::debug!(error = %e, "peer notifications refresh failed");
                None
            }
        };
        Ok::<_, ClientError>((inbox, notifications))
    });
    backend.spawn(move |_| async move {
        let result = crate::jobs::flatten(job.await);
        delivery.deliver(move |s, tx| {
            crate::energy::hit(crate::energy::C::UpdateJob);
            if s.fixture {
                return false;
            }
            let arm_poll = s.workspace.window_active && !crate::design::look::saving(s);
            let now = Instant::now();
            let mut wake_at = None;
            let Some(peer) = s.remote.peers.get_mut(&key_owned) else {
                return false;
            };
            if peer.generation != generation {
                return false;
            }
            peer.loading = false;
            let again = std::mem::take(&mut peer.stale);
            match result {
                Ok((inbox, notifications)) => {
                    prune_sent(&mut peer.sent, &inbox);
                    peer.inbox = Some(inbox);
                    let listed = notifications.is_some();
                    if let Some((notifications, unread_count)) = notifications {
                        peer.notifications = notifications;
                        peer.unread_count = unread_count;
                    }
                    if arm_poll && peer_needs_poll(peer) {
                        let period = peer_poll_period(peer);
                        let target = now + period;
                        let needs_update = peer.poll_at.is_none_or(|at| at > target);
                        if needs_update {
                            peer.poll_at = Some(target);
                            wake_at = Some(target);
                        }
                    } else {
                        peer.poll_at = None;
                    }
                    if let Some(at) = wake_at {
                        s.remote.peers.request_wake(at, now, tx);
                    }
                    // Only a notifications listing that landed says what
                    // the gateway holds; without one the marks stay.
                    if listed {
                        crate::workspace::peer_notifications_landed(s, &key_owned);
                    }
                }
                Err(e) if e.is_unauthorized() || matches!(e, ClientError::Network(_)) => {
                    let problem = connect::problem_of(&peer.computer, &e);
                    peer_failed(s, &key_owned, problem, tx);
                    crate::remote::retain_tunnels(s);
                    return true;
                }
                Err(e) => {
                    tracing::debug!(computer = %key_owned, error = %e, "peer inbox refresh failed");
                }
            }
            if again {
                refresh_peer(s, &key_owned);
            }
            true
        });
    });
}
