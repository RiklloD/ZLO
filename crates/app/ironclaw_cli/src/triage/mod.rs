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

/// `1.2.0 (commit 94172fa26c1e)`, or the version alone.
pub(crate) fn describe_build(build: &ironclaw_product_contracts::self_update::BuildInfo) -> String {
    match &build.commit {
        Some(commit) => format!(
            "{} (commit {})",
            build.version,
            commit.chars().take(12).collect::<String>()
        ),
        None => build.version.clone(),
    }
}
