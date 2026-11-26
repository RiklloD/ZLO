//! Computer use when the backend runs on another computer: its engine and
//! macOS permissions are that computer's, so ZLO reads them from the
//! backend's readiness view and never checks or prompts this Mac's. The line
//! sits where the local permission card is.

use std::time::{Duration, Instant};

use zlo_client::computer_use::{
    ComputerUseEngineState, ComputerUsePermissionState, ComputerUsePlatform, ComputerUseReadiness,
};

use super::{Generation, same};
use crate::jobs;
use crate::state::State;

/// A backend that is not ready yet is asked again this often (the user may
/// be granting the permission on it right now).
const RECHECK: Duration = Duration::from_secs(30);

/// What the backend answered, and when.
#[derive(Debug, Clone)]
pub(super) struct Remote {
    pub(super) readiness: ComputerUseReadiness,
    pub(super) at: Instant,
}

/// The short line for `computer`'s readiness.
pub fn line(computer: &str, r: &ComputerUseReadiness) -> String {
    if r.platform == ComputerUsePlatform::Unsupported {
        return format!("Computer use isn't available on {computer} (it needs macOS)");
    }
    match r.engine {
        ComputerUseEngineState::Missing => {
            return format!("Computer use on {computer}: no engine installed");
        }
        ComputerUseEngineState::Disabled => {
            return format!("Computer use on {computer}: turned off");
        }
        ComputerUseEngineState::Present => {}
    }
    let missing: Vec<&str> = [
        (r.accessibility, "Accessibility"),
        (r.screen_recording, "Screen Recording"),
    ]
    .into_iter()
    .filter(|(state, _)| *state == ComputerUsePermissionState::Missing)
    .map(|(_, name)| name)
    .collect();
    if !missing.is_empty() {
        return format!(
            "Computer use on {computer}: needs {} — grant it on {computer}",
            missing.join(" and ")
        );
    }
    if r.ready() {
        format!("Computer use on {computer}: ready")
    } else {
        format!("Computer use on {computer}: permissions unknown — check them on {computer}")
    }
}

/// Whether to ask the backend (again): never asked, or not ready and the
/// last answer is old.
pub(super) fn due(remote: Option<&Remote>, now: Instant) -> bool {
    remote.is_none_or(|r| !r.readiness.ready() && now.duration_since(r.at) >= RECHECK)
}

/// Ask the backend for its readiness; the answer lands on this chat's share.
pub(super) fn fetch(s: &mut State, thread_id: String, generation: Generation) {
    jobs::run(
        s,
        "computer use readiness",
        move |c| async move { c.computer_readiness().await },
        move |s, result, _| {
            let Ok(readiness) = result else {
                // silent-ok: an older backend has no readiness route, or this
                // device is not an operator there; no line is shown then.
                if let Err(e) = result {
                    tracing::debug!(error = %e, "computer-use readiness unavailable");
                }
                return;
            };
            s.remote.focused_computer_readiness = Some(readiness.clone());
            let unavailable = !readiness.ready();
            if let Some(cu) = same(s, &thread_id, generation) {
                cu.remote = Some(Remote {
                    readiness,
                    at: Instant::now(),
                });
            }
            if unavailable {
                probe_peers(s);
            }
        },
    );
}

/// Probe ready peers' computer-use readiness on demand (never on a timer) so
/// the "not available" note can name a computer that can run computer use.
pub(crate) fn probe_peers(s: &mut State) {
    let keys: Vec<String> = crate::remote::peers::list(s)
        .into_iter()
        .filter(|p| p.conn == crate::remote::peers::PeerConn::Ready)
        .map(|p| p.computer.key())
        .collect();
    for key in keys {
        if !crate::remote::peers::claim_computer_readiness_probe(s, &key) {
            continue;
        }
        let target = key.clone();
        crate::remote::peers::run(
            s,
            &key,
            "computer use readiness",
            |c| async move { c.computer_readiness().await },
            move |s, result, _| match result {
                Ok(readiness) => {
                    crate::remote::peers::set_computer_readiness(s, &target, readiness);
                }
                Err(e) => {
                    tracing::debug!(
                        computer = %target,
                        error = %e,
                        "peer computer-use readiness unavailable"
                    );
                }
            },
        );
    }
}

/// The first ready peer whose backend is known to be ready for computer use:
/// `(computer_key, display_label)`.
pub(crate) fn capable_peer(s: &State) -> Option<(String, String)> {
    crate::remote::peers::list(s)
        .into_iter()
        .find(|p| {
            p.conn == crate::remote::peers::PeerConn::Ready
                && p.computer_readiness.as_ref().is_some_and(|r| r.ready())
        })
        .map(|p| (p.computer.key(), p.computer.label()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ComputerUsePermissionState::{Granted, Missing, Unknown};

    fn readiness(
        accessibility: ComputerUsePermissionState,
        screen_recording: ComputerUsePermissionState,
    ) -> ComputerUseReadiness {
        ComputerUseReadiness {
            platform: ComputerUsePlatform::Macos,
            engine: ComputerUseEngineState::Present,
            accessibility,
            screen_recording,
        }
    }

    #[test]
    fn the_line_names_the_backend_computer_and_what_it_lacks() {
        assert_eq!(
            line("Studio", &readiness(Granted, Granted)),
            "Computer use on Studio: ready"
        );
        assert_eq!(
            line("Studio", &readiness(Granted, Missing)),
            "Computer use on Studio: needs Screen Recording — grant it on Studio"
        );
        assert_eq!(
            line("Studio", &readiness(Missing, Missing)),
            "Computer use on Studio: needs Accessibility and Screen Recording — grant it on Studio"
        );
        assert_eq!(
            line("Studio", &readiness(Unknown, Granted)),
            "Computer use on Studio: permissions unknown — check them on Studio"
        );
        let no_engine = ComputerUseReadiness {
            engine: ComputerUseEngineState::Missing,
            ..readiness(Granted, Granted)
        };
        assert_eq!(
            line("Studio", &no_engine),
            "Computer use on Studio: no engine installed"
        );
        let linux = ComputerUseReadiness {
            platform: ComputerUsePlatform::Unsupported,
            accessibility: ComputerUsePermissionState::NotApplicable,
            screen_recording: ComputerUsePermissionState::NotApplicable,
            ..readiness(Granted, Granted)
        };
        assert_eq!(
            line("Box", &linux),
            "Computer use isn't available on Box (it needs macOS)"
        );
    }

    #[test]
    fn a_ready_backend_is_not_asked_again_and_a_waiting_one_is_later() {
        let now = Instant::now();
        assert!(due(None, now));
        let ready = Remote {
            readiness: readiness(Granted, Granted),
            at: now,
        };
        assert!(!due(Some(&ready), now + RECHECK * 4));
        let waiting = Remote {
            readiness: readiness(Granted, Missing),
            at: now,
        };
        assert!(!due(Some(&waiting), now + Duration::from_secs(1)));
        assert!(due(Some(&waiting), now + RECHECK));
    }
}
