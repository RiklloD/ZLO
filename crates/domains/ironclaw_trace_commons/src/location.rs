//! Where Trace Commons keeps its local state (queue, policy, device keys,
//! credentials, submissions): `<base>/trace_contributions/`.
//!
//! The base defaults to `crate::location::base_dir()`
//! (`IRONCLAW_BASE_DIR`, else `~/.ironclaw`). A host that owns a data folder
//! sets it once at process start with [`set_base_dir`] — the `zlo` binary sets
//! its resolved `ZLO_HOME`, so a ZLO process never writes outside its home.
//! Every default-path helper in this crate resolves through [`base_dir`];
//! the `*_at` variants still take an explicit base for tests and tools.

use std::path::PathBuf;
use std::sync::OnceLock;

static BASE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Set the base directory for this process. First call wins; returns
/// `Err(existing)` when a different base was already set, so a host never
/// silently splits its state across two folders.
pub fn set_base_dir(base: PathBuf) -> Result<(), PathBuf> {
    let current = BASE_DIR.get_or_init(|| base.clone());
    if *current == base {
        Ok(())
    } else {
        Err(current.clone())
    }
}

/// The base directory default-path helpers use.
pub fn base_dir() -> PathBuf {
    BASE_DIR
        .get()
        .cloned()
        .unwrap_or_else(ironclaw_common::paths::ironclaw_base_dir)
}
