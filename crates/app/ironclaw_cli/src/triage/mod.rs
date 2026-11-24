//! Triage: the user's own coding agent investigates a ZLO problem on this
//! machine, reads its logs and state, checks the source of the exact build,
//! and helps file a good GitHub issue — asking before it applies a fix or
//! posts anything (`docs/using/triage.mdx`; playbook
//! `.github/triage/PLAYBOOK.md`).
//!
//! One module for both entry points: `zlo triage` (`commands::triage`)
//! writes a session and launches an agent CLI in a terminal, and the
//! backend's [`service::BackendTriage`] implements the WebUI's
//! `TriageService` port for the app's "Triage an issue". Both write the
//! same session files through [`session::write_session`].

pub(crate) mod prompt;
pub(crate) mod service;
pub(crate) mod session;
pub(crate) mod source;

use std::path::PathBuf;

/// The user's home folder (`HOME`, else Windows' `USERPROFILE`), for the
/// per-user folders outside the ZLO home (agent CLI state, the desktop app
/// log). `None` when neither is set.
pub(crate) fn user_home() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .find_map(|name| std::env::var_os(name).filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

/// `1.2.0 (commit <full hash>)`, or the version alone. The full hash: it
/// names the source cache folder and is what the agent checks out.
pub(crate) fn describe_build(build: &ironclaw_product_contracts::self_update::BuildInfo) -> String {
    match &build.commit {
        Some(commit) => format!("{} (commit {commit})", build.version),
        None => build.version.clone(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_build_keeps_its_full_commit() {
        let commit = "94172fa26c1e0b6a9c3d2e1f0a9b8c7d6e5f4a3b";
        let described =
            super::describe_build(&ironclaw_product_contracts::self_update::BuildInfo {
                version: "1.2.0".into(),
                commit: Some(commit.into()),
            });
        assert_eq!(described, format!("1.2.0 (commit {commit})"));
    }
}
