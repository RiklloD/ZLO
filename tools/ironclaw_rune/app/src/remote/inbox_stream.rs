//! The inbox push stream (`GET /inbox/events`): one open subscription per
//! connected computer, the focused one and every ready peer, so a run that
//! starts or ends anywhere (another device, a schedule, the CLI) shows at
//! once instead of at the next refresh.
//!
//! A hint carries no content: the computer it came from re-reads its inbox
//! and unread notifications, as any refresh does. Subscriptions follow
//! their connections ([`sync`], from the app's poll): opened when one is
//! ready, closed when it goes (a handover, a demotion, a drop), and all
//! closed while the window is hidden or the battery saver holds; coming
//! back reopens them, and the activation refresh catches what was missed.
//! A backend without the route (an older one) is asked once per connection
//! and then left to the event-driven refreshes it always had.
//!
//! An open stream is a held connection, not a timer: nothing wakes the app
//! until the backend says something changed (README rule 4).

use std::collections::{HashMap, HashSet};

use zlo_client::{Backend, InboxEventStream, InboxStreamMessage};

use super::ConnTag;
use crate::state::{Conn, State};

/// Whose stream: the connection it was opened on. A reply for a
/// connection that has since changed finds no match and is dropped.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Who {
    Focused(ConnTagKey),
    Peer { key: String, generation: u64 },
}

/// A [`ConnTag`] as a hashable key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ConnTagKey {
    environment: String,
    generation: u64,
}

impl From<&ConnTag> for ConnTagKey {
    fn from(tag: &ConnTag) -> Self {
        Self {
            environment: tag.environment.clone(),
            generation: tag.generation.0,
        }
    }
}

/// The open subscriptions (`State::remote.inbox_streams`).
#[derive(Default)]
pub struct InboxStreams {
    /// Dropping one aborts its stream task.
    open: HashMap<Who, InboxEventStream>,
    /// Connections whose backend has no such route: not asked again.
    unsupported: HashSet<Who>,
}

impl InboxStreams {
    /// How many streams are open (tests, diagnostics).
    #[cfg(test)]
    pub(crate) fn open_count(&self) -> usize {
        self.open.len()
    }

    #[cfg(test)]
    pub(crate) fn has_peer(&self, key: &str) -> bool {
        self.open
            .keys()
            .any(|who| matches!(who, Who::Peer { key: k, .. } if k == key))
    }

    #[cfg(test)]
    pub(crate) fn has_focused(&self) -> bool {
        self.open.keys().any(|who| matches!(who, Who::Focused(_)))
    }
}

/// Open the streams that should be open and close the rest. Cheap when
/// nothing changed (a few comparisons); called before every frame.
pub fn sync(s: &mut State) {
    if s.fixture {
        return;
    }
    let listening = s.workspace.window_active && !crate::design::look::saving(s);
    let mut wanted: Vec<(Who, Backend)> = Vec::new();
    if listening {
        if matches!(s.conn, Conn::Ready)
            && let Some(backend) = s.rt.backend.as_ref()
        {
            wanted.push((Who::Focused((&s.remote.tag).into()), backend.clone()));
        }
        for (key, generation, backend) in super::peers::stream_targets(s) {
            wanted.push((
                Who::Peer {
                    key,
                    generation: generation.0,
                },
                backend,
            ));
        }
    }
    let streams = &mut s.remote.inbox_streams;
    // Connections that are gone: their streams close, and what was learnt
    // about them (no such route) is forgotten with them.
    let keep: HashSet<&Who> = wanted.iter().map(|(who, _)| who).collect();
    streams.open.retain(|who, _| keep.contains(who));
    if listening {
        streams.unsupported.retain(|who| keep.contains(who));
    }
    let missing: Vec<(Who, Backend)> = wanted
        .into_iter()
        .filter(|(who, _)| !streams.open.contains_key(who) && !streams.unsupported.contains(who))
        .collect();
    for (who, backend) in missing {
        open(s, who, &backend);
    }
}

fn open(s: &mut State, who: Who, backend: &Backend) {
    let (stream, mut messages) = backend.subscribe_inbox();
    let delivery = s.rt.delivery();
    let asked = who.clone();
    // The forwarder ends with the stream (its sender drops when the task is
    // aborted) and holds no backend.
    backend.spawn(move |_| async move {
        while let Some(message) = messages.recv().await {
            let who = asked.clone();
            match message {
                InboxStreamMessage::Changed(_) => {
                    delivery.deliver(move |s, _| changed(s, &who));
                }
                InboxStreamMessage::Unsupported => {
                    delivery.deliver(move |s, _| {
                        if s.remote.inbox_streams.open.remove(&who).is_some() {
                            tracing::debug!(
                                "this backend has no inbox stream; refreshing on events"
                            );
                            s.remote.inbox_streams.unsupported.insert(who);
                        }
                        false
                    });
                }
                InboxStreamMessage::Closed { reason } => {
                    delivery.deliver(move |s, _| {
                        if s.remote.inbox_streams.open.remove(&who).is_some() {
                            tracing::debug!(%reason, "inbox stream refused");
                            // Not asked again on this connection.
                            s.remote.inbox_streams.unsupported.insert(who);
                        }
                        false
                    });
                }
                // Open (again): what changed between the last read and
                // the subscription is read now.
                InboxStreamMessage::Status(zlo_client::StreamStatus::Live) => {
                    delivery.deliver(move |s, _| changed(s, &who));
                }
                InboxStreamMessage::Status(_) => {}
            }
        }
    });
    s.remote.inbox_streams.open.insert(who, stream);
}

/// Something changed on the computer `who` names: re-read its inbox and
/// unread notifications, if its stream is still the open one.
fn changed(s: &mut State, who: &Who) -> bool {
    if s.fixture || !s.remote.inbox_streams.open.contains_key(who) {
        return false;
    }
    crate::energy::hit(crate::energy::C::UpdateJob);
    match who {
        Who::Focused(_) => {
            crate::workspace::inbox::refresh(s);
            crate::workspace::refresh_unread(s);
        }
        Who::Peer { key, .. } => super::peers::refresh_one(s, key),
    }
    // The refresh's own reply redraws; the hint itself changed nothing.
    false
}

#[cfg(test)]
#[path = "inbox_stream_tests.rs"]
mod tests;
