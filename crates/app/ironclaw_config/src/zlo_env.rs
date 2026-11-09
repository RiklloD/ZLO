//! ZLO-named environment variables and the pre-ZLO names they replace.
//!
//! The user-facing knobs were renamed from `IRONCLAW_REBORN_*` to `ZLO_*`.
//! The old names keep working: each lookup reads the new name first and
//! falls back to the old one, so existing service units, shell profiles and
//! scripts need no edit. Deployment-only knobs (Postgres URLs, runner
//! limits, hosting, channels, …) keep their `IRONCLAW_REBORN_*` names.

use std::{env, ffi::OsString};

/// Env var naming the WebUI bearer token, absent a `[webui].env_token_var`.
pub const WEBUI_TOKEN_ENV: &str = "ZLO_WEBUI_TOKEN";
/// Pre-ZLO name of [`WEBUI_TOKEN_ENV`].
pub const LEGACY_WEBUI_TOKEN_ENV: &str = "IRONCLAW_REBORN_WEBUI_TOKEN";
/// Env var naming the WebUI user id, absent a `[webui].env_user_id_var`.
pub const WEBUI_USER_ID_ENV: &str = "ZLO_WEBUI_USER_ID";
/// Pre-ZLO name of [`WEBUI_USER_ID_ENV`].
pub const LEGACY_WEBUI_USER_ID_ENV: &str = "IRONCLAW_REBORN_WEBUI_USER_ID";

/// `name`'s value, else `legacy`'s. An empty `name` still wins, so a
/// deliberately blanked new variable is not overridden by a stale old one.
pub fn var_os_with_legacy(name: &str, legacy: &str) -> Option<OsString> {
    env::var_os(name).or_else(|| env::var_os(legacy))
}

/// The variable to read: `name`, unless only `legacy` is set (non-empty).
/// For call sites that report *which* variable supplied a value.
pub fn active_env_name(name: &'static str, legacy: &'static str) -> &'static str {
    let set = |var: &str| env::var_os(var).is_some_and(|value| !value.is_empty());
    if !set(name) && set(legacy) {
        legacy
    } else {
        name
    }
}
