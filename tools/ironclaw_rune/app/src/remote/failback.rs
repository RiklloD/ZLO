//! Moving back to a better route (T3 Code's connection supervisor): while
//! the computer in use answers over one of its later routes, the earlier
//! ones are checked every minute; as soon as one answers as that computer
//! *and* accepts this Mac's token, the connection quietly moves to it
//! (`connect::refresh`). A route that answers but refuses the token never
//! costs the working connection.

use std::time::Duration;

use zlo_client::environments::{
    Active, RouteKind, check_descriptor, check_learned_transport, check_transport, preferred_routes,
};
use zlo_client::{Client, Connection, Token};

use crate::jobs;
use crate::state::{Conn, State};

/// How often earlier routes are checked.
const EVERY: Duration = Duration::from_secs(60);

/// Check again in a minute, while the computer in use answers over a
/// later route. Bound to the connection: a newer one stops the chain.
pub(crate) fn schedule(s: &State) {
    if s.remote.route_rank == 0 || s.fixture {
        return;
    }
    let tag = s.remote.tag.clone();
    jobs::after(s, EVERY, move |s, _| {
        if s.remote.tag == tag {
            check(s);
        }
    });
}

fn check(s: &mut State) {
    if !matches!(s.conn, Conn::Ready) || s.remote.switching.is_some() {
        return schedule(s);
    }
    let Active::Environment(id) = s.remote.registry.active.clone() else {
        return;
    };
    let Some(env) = s.remote.registry.get(&id).cloned() else {
        return;
    };
    let rank = s.remote.route_rank;
    let Some(net) = super::net() else {
        return schedule(s);
    };
    let tokens = s.remote.tokens.clone();
    let tag = s.remote.tag.clone();
    let handle = s.rt.handle.clone();
    net.spawn(async move {
        let token = {
            let id = id.clone();
            tokio::task::spawn_blocking(move || tokens.get(&id))
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
        };
        let mut better = false;
        if let Some(token) = token.and_then(|t| Token::new(t).ok()) {
            let peers = tokio::task::spawn_blocking(zlo_client::tailnet::peer_addresses)
                .await
                .unwrap_or_default();
            for route in preferred_routes(&env.routes).into_iter().take(rank) {
                if route.kind == RouteKind::Ssh {
                    continue;
                }
                let origin = if route.learned {
                    check_learned_transport(&route.url, &peers).ok()
                } else {
                    check_transport(&route.url).ok()
                };
                let Some(origin) = origin else { continue };
                let Ok(descriptor) = zlo_client::environment_calls::fetch_descriptor(
                    &origin,
                    super::DESCRIPTOR_TIMEOUT,
                )
                .await
                else {
                    continue;
                };
                if check_descriptor(&descriptor, Some((&env.environment_id, &env.label))).is_err() {
                    continue;
                }
                // Authorized too: a route that refuses this Mac is no better.
                let client = Client::new(Connection {
                    base_url: origin,
                    token: token.clone(),
                    token_source: "Keychain".into(),
                });
                if let Ok(client) = client
                    && client.session().await.is_ok()
                {
                    better = true;
                    break;
                }
            }
        }
        handle.update(move |s, _| {
            if s.remote.tag != tag {
                return;
            }
            if better {
                tracing::debug!("an earlier route answers again; moving back to it");
                super::connect::refresh(s);
            } else {
                schedule(s);
            }
        });
    });
}
