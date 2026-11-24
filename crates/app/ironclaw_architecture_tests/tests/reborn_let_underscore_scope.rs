//! Pins the scope of `clippy::let_underscore_must_use`: the whole workspace.
//!
//! `let _ = <must_use>` silently swallows a `Result` or other must-use value.
//! Every discard in the workspace was triaged (fixed, or kept behind a
//! statement-level `#[expect(..., reason = "...")]`), and the lint is denied by
//! `[workspace.lints.clippy]` in the root `Cargo.toml`. That entry binds only
//! packages that opt in with `[lints] workspace = true`. The members that do
//! not (over `unreachable_pub` noise — see the root `Cargo.toml`) deny it
//! themselves, in one of two ways:
//!
//! - a package-level `[lints.clippy] let_underscore_must_use = "deny"` table
//!   (the root integration-suite package), which binds all of its targets; or
//! - an inner attribute in **every target root** (`ironclaw_assistant`,
//!   `ironclaw_webui`), since a crate-root attribute binds only the target it
//!   heads and `tests/*.rs` are separate crates from `src/lib.rs`.
//!
//! This gate fails when the workspace entry is weakened or removed, when a
//! member stops inheriting workspace lints without denying the lint itself, or
//! when an attribute-scoped member gains a target root without the deny — so
//! the enforced scope cannot shrink silently.

#![deny(clippy::let_underscore_must_use)]
mod ratchet_support;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use ratchet_support::workspace_root;
use syn::parse::Parser;
use syn::{AttrStyle, Meta, Token};

const LINT: &str = "let_underscore_must_use";
const DENY_ATTRIBUTE: &str = "#![deny(clippy::let_underscore_must_use)]";

/// Workspace members that do not inherit `[lints] workspace = true` and deny
/// the lint with their own package-level `[lints.clippy]` entry (paths relative
/// to the workspace root). Widening this list is a review decision.
const PACKAGE_SCOPED_MEMBERS: &[&str] = &["."];

/// Workspace members that do not inherit `[lints] workspace = true` and carry
/// `DENY_ATTRIBUTE` in every target root instead. Widening this list is a
/// review decision, not a side effect.
const ATTRIBUTE_SCOPED_MEMBERS: &[&str] = &[
    "crates/product/ironclaw_assistant",
    "crates/product/ironclaw_webui",
];

fn read_manifest(path: &Path) -> toml::Table {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    toml::from_str(&text).unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

/// True when a Cargo lint level (`"deny"` or `{ level = "deny", ... }`) is
/// `deny` or `forbid`.
fn level_denies(value: &toml::Value) -> bool {
    let level = match value {
        toml::Value::String(level) => Some(level.as_str()),
        toml::Value::Table(table) => table.get("level").and_then(toml::Value::as_str),
        _ => None,
    };
    matches!(level, Some("deny" | "forbid"))
}

/// The workspace members, as directories relative to the workspace root.
fn workspace_members(root_manifest: &toml::Table) -> Vec<String> {
    root_manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
        .expect("root Cargo.toml lists [workspace] members")
        .iter()
        .map(|member| {
            let member = member.as_str().expect("workspace member is a string");
            assert!(
                !member.contains('*'),
                "workspace member `{member}` is a glob; extend this gate to expand it"
            );
            member.trim_end_matches('/').to_string()
        })
        .collect()
}

/// True when the package's own `[lints.clippy]` table denies the lint.
fn package_lints_deny(manifest: &toml::Table) -> bool {
    manifest
        .get("lints")
        .and_then(|lints| lints.get("clippy"))
        .and_then(|clippy| clippy.get(LINT))
        .is_some_and(level_denies)
}

fn inherits_workspace_lints(manifest: &toml::Table) -> bool {
    manifest
        .get("lints")
        .and_then(|lints| lints.get("workspace"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false)
}

/// Every compilation-target root Cargo would build for the crate: the
/// auto-discovered layout, the build script, and any target an explicit
/// `path = "..."` (or `[package] build = "..."`) names.
fn target_roots(crate_dir: &Path) -> BTreeSet<PathBuf> {
    let mut roots = BTreeSet::new();
    for fixed in ["src/lib.rs", "src/main.rs"] {
        let path = crate_dir.join(fixed);
        if path.is_file() {
            roots.insert(path);
        }
    }
    for directory in ["src/bin", "tests", "benches", "examples"] {
        let Ok(entries) = fs::read_dir(crate_dir.join(directory)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
                roots.insert(path);
            } else if path.join("main.rs").is_file() {
                roots.insert(path.join("main.rs"));
            }
        }
    }

    let manifest_path = crate_dir.join("Cargo.toml");
    let manifest_text = fs::read_to_string(&manifest_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", manifest_path.display()));
    let manifest: toml::Table = toml::from_str(&manifest_text)
        .unwrap_or_else(|error| panic!("parse {}: {error}", manifest_path.display()));
    let explicit_path = |target: &toml::Value| {
        target
            .get("path")
            .and_then(toml::Value::as_str)
            .map(|path| crate_dir.join(path))
    };
    // Build script: `build = false` disables it, `build = "x.rs"` relocates it,
    // otherwise Cargo auto-discovers `build.rs`.
    match manifest
        .get("package")
        .and_then(|package| package.get("build"))
    {
        Some(toml::Value::Boolean(false)) => {}
        Some(toml::Value::String(path)) => {
            roots.insert(crate_dir.join(path));
        }
        _ => {
            let path = crate_dir.join("build.rs");
            if path.is_file() {
                roots.insert(path);
            }
        }
    }
    if let Some(path) = manifest.get("lib").and_then(explicit_path) {
        roots.insert(path);
    }
    for kind in ["bin", "test", "bench", "example"] {
        if let Some(targets) = manifest.get(kind).and_then(toml::Value::as_array) {
            roots.extend(targets.iter().filter_map(explicit_path));
        }
    }
    roots
}

/// True when the file's crate-level (inner) attributes include an
/// unconditional `deny`/`forbid` naming `clippy::let_underscore_must_use`.
/// Parsed with `syn`, so text in comments or string literals never counts and
/// multi-line or multi-lint forms do. `cfg_attr` is conditional and does not
/// count. A file that does not parse never counts.
fn carries_deny(source: &str) -> bool {
    let Ok(file) = syn::parse_file(source) else {
        return false;
    };
    file.attrs
        .iter()
        .filter(|attribute| matches!(attribute.style, AttrStyle::Inner(_)))
        .filter_map(|attribute| match &attribute.meta {
            Meta::List(list) if list.path.is_ident("deny") || list.path.is_ident("forbid") => {
                syn::punctuated::Punctuated::<syn::Path, Token![,]>::parse_terminated
                    .parse2(list.tokens.clone())
                    .ok()
            }
            _ => None,
        })
        .flatten()
        .any(|lint| {
            let segments: Vec<String> = lint
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();
            segments == ["clippy", "let_underscore_must_use"]
        })
}

#[test]
fn workspace_lints_deny_let_underscore_must_use() {
    let manifest = read_manifest(&workspace_root().join("Cargo.toml"));
    let entry = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("lints"))
        .and_then(|lints| lints.get("clippy"))
        .and_then(|clippy| clippy.get(LINT));
    assert!(
        entry.is_some_and(level_denies),
        "[workspace.lints.clippy] must set `{LINT} = \"deny\"` (found {entry:?}); every \
         workspace member was triaged against it (docs/internal/lint-backlog.md)"
    );
}

#[test]
fn every_workspace_member_denies_let_underscore_must_use() {
    let root = workspace_root();
    let members = workspace_members(&read_manifest(&root.join("Cargo.toml")));
    // Non-vacuity: the root package plus the ~70 crates and tools.
    assert!(
        members.len() > 60,
        "non-vacuity: only {} workspace members",
        members.len()
    );

    let mut package_scoped = Vec::new();
    let mut attribute_scoped = Vec::new();
    let mut missing = Vec::new();
    for member in &members {
        let directory = root.join(member);
        let manifest = read_manifest(&directory.join("Cargo.toml"));
        if inherits_workspace_lints(&manifest) {
            continue;
        }
        if package_lints_deny(&manifest) {
            package_scoped.push(member.clone());
            continue;
        }
        attribute_scoped.push(member.clone());
        let roots = target_roots(&directory);
        assert!(
            !roots.is_empty(),
            "{member}: opts out of workspace lints but the scan found no target roots"
        );
        for path in roots {
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            if !carries_deny(&source) {
                missing.push(
                    path.strip_prefix(&root)
                        .unwrap_or(&path)
                        .display()
                        .to_string(),
                );
            }
        }
    }

    assert_eq!(
        package_scoped, PACKAGE_SCOPED_MEMBERS,
        "workspace members that deny clippy::{LINT} in their own `[lints.clippy]` \
         table instead of inheriting workspace lints; list them in PACKAGE_SCOPED_MEMBERS"
    );
    assert_eq!(
        attribute_scoped, ATTRIBUTE_SCOPED_MEMBERS,
        "workspace members that neither set `[lints] workspace = true` nor deny \
         clippy::{LINT} in their own `[lints.clippy]` table; such a member must be \
         listed in ATTRIBUTE_SCOPED_MEMBERS and carry `{DENY_ATTRIBUTE}` in every \
         target root"
    );
    assert!(
        missing.is_empty(),
        "these target roots of members that opt out of workspace lints lack \
         `{DENY_ATTRIBUTE}`; add it as a crate-level attribute (see the root \
         Cargo.toml [workspace.lints] notes):\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn lint_level_parsing_accepts_only_deny_and_forbid() {
    assert!(level_denies(&toml::Value::String("deny".into())));
    assert!(level_denies(&toml::Value::String("forbid".into())));
    assert!(!level_denies(&toml::Value::String("warn".into())));
    assert!(!level_denies(&toml::Value::String("allow".into())));
    let table: toml::Table =
        toml::from_str("x = { level = \"deny\", priority = 1 }").expect("toml");
    assert!(level_denies(&table["x"]));
    let table: toml::Table = toml::from_str("x = { level = \"warn\" }").expect("toml");
    assert!(!level_denies(&table["x"]));

    let manifest: toml::Table =
        toml::from_str("[lints.clippy]\nlet_underscore_must_use = \"deny\"\n").expect("toml");
    assert!(package_lints_deny(&manifest) && !inherits_workspace_lints(&manifest));
    let manifest: toml::Table = toml::from_str("[lints]\nworkspace = true\n").expect("toml");
    assert!(inherits_workspace_lints(&manifest) && !package_lints_deny(&manifest));
}

#[test]
fn deny_detection_ignores_commented_out_attributes() {
    assert!(carries_deny(
        "//! docs\n#![deny(clippy::let_underscore_must_use)]\n"
    ));
    assert!(!carries_deny(
        "// #![deny(clippy::let_underscore_must_use)]\n"
    ));
    assert!(!carries_deny(
        "#![allow(clippy::let_underscore_must_use)]\n"
    ));
    // A block comment or a raw string around the deny must not satisfy the gate.
    assert!(!carries_deny(
        "/*\n#![deny(clippy::let_underscore_must_use)]\n*/\nfn f() {}\n"
    ));
    assert!(!carries_deny(
        "const S: &str = r#\"\n#![deny(clippy::let_underscore_must_use)]\n\"#;\n"
    ));
    // Conditional: only applies under the cfg, so it is not the enforced scope.
    assert!(!carries_deny(
        "#![cfg_attr(test, deny(clippy::let_underscore_must_use))]\n"
    ));
    // Equivalent unconditional forms do count.
    assert!(carries_deny(
        "#![deny(\n    clippy::let_underscore_must_use,\n    unused_must_use\n)]\n"
    ));
    assert!(carries_deny(
        "#![forbid(clippy::let_underscore_must_use)]\n"
    ));
}

#[test]
fn target_root_scan_finds_auto_and_explicit_targets() {
    // Sabotage check: a scan that missed a target kind would read as clean.
    let dir = tempfile::tempdir().expect("tempdir");
    let crate_dir = dir.path();
    for file in [
        "src/lib.rs",
        "src/bin/tool.rs",
        "tests/a.rs",
        "tests/suite/main.rs",
        "benches/b.rs",
        "examples/e.rs",
        "custom/root.rs",
        "build.rs",
    ] {
        let path = crate_dir.join(file);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, "fn main() {}\n").expect("write");
    }
    fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"x\"\n\n[[test]]\nname = \"custom\"\npath = \"custom/root.rs\"\n",
    )
    .expect("write manifest");
    let roots = target_roots(crate_dir);
    assert_eq!(roots.len(), 8, "{roots:?}");
    assert!(roots.contains(&crate_dir.join("build.rs")), "{roots:?}");
    assert!(
        roots
            .iter()
            .all(|path| !carries_deny(&fs::read_to_string(path).expect("read")))
    );

    // `[package] build = "..."` relocates the build script; `build = false` drops it.
    fs::write(crate_dir.join("gen.rs"), "fn main() {}\n").expect("write");
    fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"x\"\nbuild = \"gen.rs\"\n",
    )
    .expect("write manifest");
    let roots = target_roots(crate_dir);
    assert!(roots.contains(&crate_dir.join("gen.rs")), "{roots:?}");
    assert!(!roots.contains(&crate_dir.join("build.rs")), "{roots:?}");
    fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"x\"\nbuild = false\n",
    )
    .expect("write manifest");
    let roots = target_roots(crate_dir);
    assert!(!roots.contains(&crate_dir.join("build.rs")), "{roots:?}");
    assert!(!roots.contains(&crate_dir.join("gen.rs")), "{roots:?}");
}
