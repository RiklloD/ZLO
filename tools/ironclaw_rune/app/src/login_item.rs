//! Open at login (GPUI `login_item.rs`). Area F.
//!
//! PORT-STUB(area F): created by area D for Settings → General; the
//! platform port replaces the bodies.

use std::path::PathBuf;

/// The running `.app` bundle, if ZLO runs from one.
pub fn bundle_path() -> Option<PathBuf> {
    None
}

pub fn is_enabled() -> bool {
    false
}

pub fn set_enabled(_enabled: bool) -> Result<(), String> {
    Err("Open at login is not ported yet.".into())
}
