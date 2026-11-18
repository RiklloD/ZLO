//! v3 `[[hooks]]` and `[[settings]]` parsing.
//!
//! Kept beside, not inside, `v3.rs` so the manifest's other sections can grow
//! independently. Both sections are package surfaces the host executes on the
//! package's behalf:
//!
//! - `[[hooks]]` uses the hook crate's own entry vocabulary
//!   (`ironclaw_hooks::manifest::HookManifestEntry`: `id`, `kind`, `scope`,
//!   `phase`, `priority`, `description`, `requires_grant`, `body`) plus one
//!   package-level key, `module`: the package-relative WASM asset that
//!   implements a `body.mode = "wasm"` hook. This crate does not interpret the
//!   hook vocabulary; it validates the structure the v2 parser always has
//!   (table, non-empty unique `id`, size and count bounds), checks the
//!   `module`/`body.mode` pairing, and leaves typed projection to the
//!   composition loader.
//! - `[[settings]]` is the typed user-settings declaration owned by
//!   [`crate::settings`].

use ironclaw_extension_contracts::runtime::ExtensionAssetPath;

use crate::settings::{ExtensionSettingDecl, validate_setting_declarations};
use crate::v2::{HookSectionEntryV2, MAX_MANIFEST_HOOKS, validate_hook_entries};
use crate::v3::ManifestV3Error;

/// Validate the raw `[[hooks]]` array of a v3 manifest.
pub(crate) fn parse_v3_hooks(
    raw_hooks: Vec<toml::Value>,
) -> Result<Vec<HookSectionEntryV2>, ManifestV3Error> {
    if raw_hooks.len() > MAX_MANIFEST_HOOKS {
        return Err(invalid_hooks(format!(
            "manifest declares {} hooks, exceeding the maximum of {MAX_MANIFEST_HOOKS}",
            raw_hooks.len()
        )));
    }
    let mut modules = Vec::with_capacity(raw_hooks.len());
    let mut stripped = Vec::with_capacity(raw_hooks.len());
    for (index, raw) in raw_hooks.into_iter().enumerate() {
        let toml::Value::Table(mut table) = raw else {
            return Err(invalid_hooks(format!(
                "[[hooks]] entry {index} must be a table"
            )));
        };
        let label = table
            .get("id")
            .and_then(toml::Value::as_str)
            .map(|id| format!("`{id}`"))
            .unwrap_or_else(|| format!("entry {index}"));
        let module = match table.remove("module") {
            None => None,
            Some(toml::Value::String(module)) => {
                let path = ExtensionAssetPath::new(module).map_err(|error| {
                    invalid_hooks(format!("hook {label} `module` is invalid: {error}"))
                })?;
                Some(path.as_str().to_string())
            }
            Some(_) => {
                return Err(invalid_hooks(format!(
                    "hook {label} `module` must be a package-relative path string"
                )));
            }
        };
        let mode = table
            .get("body")
            .and_then(toml::Value::as_table)
            .and_then(|body| body.get("mode"))
            .and_then(toml::Value::as_str);
        match (mode, module.is_some()) {
            (Some("wasm"), false) => {
                return Err(invalid_hooks(format!(
                    "hook {label} has a wasm body and must name its `module` asset"
                )));
            }
            (Some(mode), true) if mode != "wasm" => {
                return Err(invalid_hooks(format!(
                    "hook {label} declares `module` but its body mode is `{mode}`; only \
                     `mode = \"wasm\"` hooks load a module"
                )));
            }
            _ => {}
        }
        modules.push(module);
        stripped.push(toml::Value::Table(table));
    }
    let mut entries =
        validate_hook_entries(stripped).map_err(|error| invalid_hooks(error.to_string()))?;
    for (entry, module) in entries.iter_mut().zip(modules) {
        entry.module = module;
    }
    Ok(entries)
}

/// Validate the `[[settings]]` array of a v3 manifest.
pub(crate) fn parse_v3_settings(
    settings: Vec<ExtensionSettingDecl>,
) -> Result<Vec<ExtensionSettingDecl>, ManifestV3Error> {
    validate_setting_declarations(&settings).map_err(|error| ManifestV3Error::InvalidSettings {
        reason: error.to_string(),
    })?;
    Ok(settings)
}

fn invalid_hooks(reason: String) -> ManifestV3Error {
    ManifestV3Error::InvalidHooks { reason }
}
