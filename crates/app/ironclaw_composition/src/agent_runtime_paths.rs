//! Agent runtime folders as their owner (`ironclaw_cli_providers`) defines
//! them, for the binary's triage context, which may not link that crate
//! (`reborn_cli_binary_crate_stays_separate_from_v1_root`). Paths only:
//! nothing here starts or reads a runtime.

use std::path::{Path, PathBuf};

use ironclaw_cli_providers::{antigravity, codex, muse};

/// Codex's own folder (`CODEX_HOME`, else `~/.codex`; holds `auth.json`).
pub fn codex_home(user_home: &Path) -> PathBuf {
    codex::codex_home(user_home)
}

/// ZLO's Antigravity folder.
pub fn antigravity_state_dir(user_home: &Path) -> PathBuf {
    antigravity::state_dir(user_home)
}

/// The Antigravity profile ZLO runs with (Hermes's when it signed one in).
pub fn antigravity_profile_dir(user_home: &Path) -> PathBuf {
    antigravity::profile_dir(user_home)
}

/// Where Antigravity keeps its sign-in under `profile_dir`.
pub fn antigravity_sign_in_dir(profile_dir: &Path) -> PathBuf {
    antigravity::sign_in_dir(profile_dir)
}

/// ZLO's Muse Code folder.
pub fn muse_state_dir(user_home: &Path) -> PathBuf {
    muse::state_dir(user_home)
}
