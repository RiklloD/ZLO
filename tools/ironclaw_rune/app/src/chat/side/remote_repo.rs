//! Where the Changes, History and Console tabs run when the backend is on
//! another computer: its own git and console routes
//! (`zlo_client::repo_calls`), on the thread's repository on that machine.
//! Same UI and confirmations as the local path; the backend picks the folder.
//!
//! A backend that predates those routes, or runs no console (no host shell
//! there), answers "unsupported" once: the tab then keeps its
//! "not available on <computer>" line for this thread.

use std::path::PathBuf;

use zlo_client::ClientError;
use zlo_client::repo_calls::{self, GitBranch, GitChange, GitCommit};

use super::{SideState, Tab, git};
use crate::state::State;

/// What the backend said it can do for this thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Support {
    #[default]
    Unknown,
    Yes,
    No,
}

#[derive(Debug, Default)]
pub(crate) struct RemoteRepo {
    pub(crate) git: Support,
    pub(crate) console: Support,
    /// The backend holds a checkpoint of the last turn.
    pub(crate) checkpoint: bool,
}

/// The tabs run on the backend's machine rather than this Mac.
pub(super) fn remote(s: &State) -> bool {
    crate::remote::backend_is_remote(s)
}

/// `tab` can run remotely here (not known to be unsupported).
pub(super) fn offered(x: &SideState, tab: Tab) -> bool {
    match tab {
        Tab::Changes | Tab::History => x.remote_repo.git != Support::No,
        Tab::Console => x.remote_repo.console != Support::No,
        _ => true,
    }
}

/// Record the backend's answer for git (`console` false) or the console.
/// Returns true when it was "unsupported" (the caller then stops).
pub(super) fn note(x: &mut SideState, console: bool, result: Result<(), &ClientError>) -> bool {
    let support = match result {
        Ok(()) => Support::Yes,
        Err(e) if repo_calls::remote_unsupported(e) => Support::No,
        Err(_) => return false,
    };
    if console {
        x.remote_repo.console = support;
    } else {
        x.remote_repo.git = support;
    }
    support == Support::No
}

/// The backend's repository root, as the coding tabs' `repo`.
pub(super) fn adopt_root(x: &mut SideState, root: &str) {
    x.repo = Some(PathBuf::from(root));
}

pub(super) fn change(c: GitChange) -> git::Change {
    git::Change {
        path: c.path,
        from: c.from,
        index: c.index,
        worktree: c.worktree,
    }
}

pub(super) fn branch(b: GitBranch) -> git::Branch {
    git::Branch {
        name: b.name,
        upstream: b.upstream,
        ahead: b.ahead,
        behind: b.behind,
    }
}

pub(super) fn commit(c: GitCommit) -> git::Commit {
    git::Commit {
        sha: c.sha,
        parents: c.parents,
        author: c.author,
        time: c.time,
        refs: c.refs,
        subject: c.subject,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(status: u16) -> ClientError {
        ClientError::Api {
            status,
            message: "x".into(),
            body: Box::default(),
        }
    }

    /// An older backend (no route) turns the tab back into its
    /// "not available" line; a real failure does not.
    #[test]
    fn only_unsupported_answers_switch_a_tab_off() {
        let mut x = SideState::default();
        assert!(offered(&x, Tab::Changes) && offered(&x, Tab::Console));
        assert!(!note(&mut x, false, Err(&api(500))));
        assert_eq!(x.remote_repo.git, Support::Unknown);
        assert!(note(&mut x, false, Err(&api(404))));
        assert!(!offered(&x, Tab::Changes) && !offered(&x, Tab::History));
        assert!(offered(&x, Tab::Console), "git and console are told apart");
        let refused = ClientError::Refused {
            code: "console_unavailable".into(),
            message: "no shell".into(),
        };
        assert!(note(&mut x, true, Err(&refused)));
        assert!(!offered(&x, Tab::Console));
        assert!(!note(&mut x, true, Ok(())));
        assert!(offered(&x, Tab::Console));
    }
}
