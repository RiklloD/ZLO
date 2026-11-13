//! Pins the scope of `clippy::let_underscore_must_use`.
//!
//! `let _ = <must_use>` silently swallows a `Result` or other must-use value.
//! The discards were triaged in [`TRIAGED_CRATES`] only: each kept discard
//! there carries a statement-level `#[expect(..., reason = "...")]`. The lint is
//! not a `[workspace.lints]` entry, because those bind every crate that opts in
//! with `[lints] workspace = true` (most of the workspace, almost all of it
//! untriaged), and Cargo forbids a per-crate `[lints]` table beside
//! `workspace = true`. So each triaged crate denies it with an inner attribute
//! in **every target root** — a crate-root attribute binds only the target it
//! heads, and `tests/*.rs` are separate crates from `src/lib.rs`.
//!
//! This gate is the one place the list lives. It fails when a target root of a
//! triaged crate loses the deny (or a new target lands without it), so the
//! enforced scope cannot shrink silently. To widen it: triage a crate, add the
//! deny to its target roots, add the crate here. Backlog:
//! `docs/internal/lint-backlog.md`.

mod ratchet_support;

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use ratchet_support::{crate_dir, workspace_root};
use syn::parse::Parser;
use syn::{AttrStyle, Meta, Token};

/// Crates whose `let _ = <must_use>` discards were triaged (fix/let-underscore-must-use).
const TRIAGED_CRATES: &[&str] = &[
    "ironclaw_approvals",
    "ironclaw_attachments",
    "ironclaw_composition",
    "ironclaw_documents",
    "ironclaw_event_log",
    "ironclaw_hooks",
    "ironclaw_host_runtime",
    "ironclaw_identity",
    "ironclaw_notifications",
    "ironclaw_openai_compat",
    "ironclaw_operator",
    "ironclaw_turn_runner",
];

const DENY_ATTRIBUTE: &str = "#![deny(clippy::let_underscore_must_use)]";

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
fn triaged_crates_deny_let_underscore_must_use_in_every_target_root() {
    let root = workspace_root();
    let mut checked = 0usize;
    let mut missing = Vec::new();
    for name in TRIAGED_CRATES {
        let directory = crate_dir(&root, name);
        let roots = target_roots(&directory);
        assert!(
            roots.iter().any(|path| path.ends_with("src/lib.rs")),
            "{name}: expected a src/lib.rs target root; the scan found {roots:?}"
        );
        for path in roots {
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            checked += 1;
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
    assert!(
        checked >= TRIAGED_CRATES.len(),
        "non-vacuity: scanned {checked} target roots for {} crates",
        TRIAGED_CRATES.len()
    );
    assert!(
        missing.is_empty(),
        "these target roots of triaged crates lack `{DENY_ATTRIBUTE}`; add it as a \
         crate-level attribute (see Cargo.toml [workspace.lints] notes and \
         docs/internal/lint-backlog.md):\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn let_underscore_must_use_stays_out_of_workspace_lints() {
    // A workspace-wide entry would bind ~70 crates whose discards were never
    // triaged — the regression this crate-scoped opt-in replaced.
    let manifest_path = workspace_root().join("Cargo.toml");
    let manifest: toml::Table = toml::from_str(
        &fs::read_to_string(&manifest_path)
            .unwrap_or_else(|error| panic!("read {}: {error}", manifest_path.display())),
    )
    .unwrap_or_else(|error| panic!("parse {}: {error}", manifest_path.display()));
    let entry = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("lints"))
        .and_then(|lints| lints.get("clippy"))
        .and_then(|clippy| clippy.get("let_underscore_must_use"));
    assert!(
        entry.is_none(),
        "clippy::let_underscore_must_use is scoped per crate by \
         `{DENY_ATTRIBUTE}` in the triaged crates' target roots; do not add it to \
         [workspace.lints.clippy] until every opted-in crate is triaged \
         (docs/internal/lint-backlog.md)"
    );
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
