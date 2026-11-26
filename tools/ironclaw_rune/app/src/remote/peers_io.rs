//! Async network lifecycle for background peers: resolving routes (reusing
//! `connect::resolve`), reading initial session/inbox/notifications, handling
//! failures/backoff, and refreshing ready peers.

use std::time::{Duration, Instant};

use rune::Tx;
use zlo_client::environments::THIS_MAC;
use zlo_client::{Backend, Client, ClientError};

use super::{POLL, PeerConn, peer_needs_poll};
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
    let key_owned = key.to_string();
    let tokens = s.remote.tokens.clone();
    let rt = s.rt.clone();
    net.spawn(async move {
        let resolved = connect::resolve(target, tokens, own_backend).await;
        rt.deliver(move |s, tx| {
            if s.fixture
                || !s
                    .remote
                    .peers
                    .get(&key_owned)
                    .is_some_and(|p| p.generation == generation)
            {
                if let Ok(r) = &resolved
                    && r.tunnel.is_some()
                {
                    crate::remote::retain_tunnels(s);
                }
                return false;
            }
            match resolved {
                Ok(r) => open_peer(s, &key_owned, generation, r, tx),
                Err(p) => peer_failed(s, &key_owned, p, tx),
            }
            true
        });
    });
}

fn open_peer(s: &mut State, key: &str, generation: Generation, r: Resolved, tx: &mut Tx) {
    let backend = match Client::new(r.conn).and_then(Backend::new) {
        Ok(b) => b,
        Err(e) => return peer_failed(s, key, Problem::Blocked(e.to_string()), tx),
    };
    let Some(peer) = s.remote.peers.get_mut(key) else {
        return;
    };
    peer.pending_backend = Some(backend.clone());
    let key_owned = key.to_string();
    let rt = s.rt.clone();
    let computer = r.computer;
    let descriptor = r.descriptor;
    let tunnel = r.tunnel;
    let route_rank = r.route_rank;
    let job = backend.spawn(|c| async move {
        let session = c.session().await?;
        let inbox = c.inbox().await?;
        let notifications = match c.list_notifications(ALERT_PAGE).await {
            Ok(page) => page.notifications,
            Err(e) => {
                tracing::debug!(error = %e, "peer notifications read failed");
                Vec::new()
            }
        };
        Ok::<_, ClientError>((session, inbox, notifications))
    });
    backend.spawn(move |_| async move {
        let result = crate::jobs::flatten(job.await);
        rt.deliver(move |s, tx| {
            if s.fixture
                || !s
                    .remote
                    .peers
                    .get(&key_owned)
                    .is_some_and(|p| p.generation == generation)
            {
                if tunnel.is_some() {
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
                return false;
            };
            match result {
                Ok((session, inbox, notifications)) => {
                    if let (Computer::Remote(r), Some(d)) = (&computer, descriptor.as_ref())
                        && s.remote
                            .registry
                            .learn_routes(&r.environment_id, &d.addresses)
                        && let Err(e) = crate::remote::save_registry(s)
                    {
                        tracing::warn!(error = %e, "learned routes not saved");
                    }
                    let arm_poll = s.workspace.window_active && !crate::design::look::saving(s);
                    if let Some(peer) = s.remote.peers.get_mut(&key_owned) {
                        peer.computer = computer;
                        peer.conn = PeerConn::Ready;
                        peer.session = Some(session);
                        peer.inbox = Some(inbox);
                        peer.notifications = notifications;
                        peer.backend = Some(backend);
                        peer.descriptor = descriptor;
                        peer.tunnel = tunnel;
                        peer.route_rank = route_rank;
                        peer.retry_attempt = 0;
                        peer.retry_at = None;
                        peer.loading = false;
                        peer.stale = false;
                        if arm_poll && peer_needs_poll(peer) {
                            peer.poll_at = Some(Instant::now() + POLL);
                            tx.wake_after(POLL);
                        } else {
                            peer.poll_at = None;
                        }
                    }
                    tracing::debug!(computer = %key_owned, "peer connected");
                    crate::remote::retain_tunnels(s);
                }
                Err(e) => {
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
    let is_asleep = crate::remote::asleep(s, key);
    let can_wake = s.workspace.window_active && !crate::design::look::saving(s);
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
                return;
            }
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
            let delay = Duration::from_secs(secs);
            peer.retry_at = Some(Instant::now() + delay);
            if can_wake {
                tx.wake_after(delay);
            }
        }
    }
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
    let rt = s.rt.clone();
    let job = backend.spawn(|c| async move {
        let inbox = c.inbox().await?;
        let notifications = match c.list_notifications(ALERT_PAGE).await {
            Ok(page) => Some(page.notifications),
            Err(e) => {
                tracing::debug!(error = %e, "peer notifications refresh failed");
                None
            }
        };
        Ok::<_, ClientError>((inbox, notifications))
    });
    backend.spawn(move |_| async move {
        let result = crate::jobs::flatten(job.await);
        rt.deliver(move |s, tx| {
            crate::energy::hit(crate::energy::C::UpdateJob);
            if s.fixture {
                return false;
            }
            let arm_poll = s.workspace.window_active && !crate::design::look::saving(s);
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
                    peer.inbox = Some(inbox);
                    if let Some(notifications) = notifications {
                        peer.notifications = notifications;
                    }
                    if arm_poll && peer_needs_poll(peer) {
                        if peer.poll_at.is_none() {
                            peer.poll_at = Some(Instant::now() + POLL);
                            tx.wake_after(POLL);
                        }
                    } else {
                        peer.poll_at = None;
                    }
                }
                Err(e) if e.is_unauthorized() => {
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
