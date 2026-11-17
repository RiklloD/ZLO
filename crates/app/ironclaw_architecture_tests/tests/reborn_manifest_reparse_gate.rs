//! REC-1 gate: raw manifest TOML is reparsed only by the **compiler** and
//! **bundled-asset** paths — never by a projection/loader
//! path that reads an installed extension. An installed extension projects
//! from its persisted resolved record (`ExtensionManifestRecord::from_resolved`
//! / `resolved.to_internal`), so a NEW `ExtensionManifest::parse` /
//! `ExtensionManifestRecord::from_toml` call — especially in a projection path
//! such as the WebUI services projection or the generic host loader — fails
//! this gate until its file and category are enumerated below.
//!
//! Pattern mirrors `reborn_extension_specificity.rs`: scan production Rust
//! (`ratchet_support::production_rust_files`, test files decided by cfg
//! gating) with `#[cfg(test)]` blocks stripped, collect the reparse call
//! sites, and hold them to an EXACT, categorized allowlist. Adding a reparse
//! (new file, or one more call in an existing file) fails until the allowlist
//! is updated with a justification; removing one — moving a projection onto the
//! resolved record — fails the stale entry. The list can only shrink.

#![deny(clippy::let_underscore_must_use)]
#[allow(dead_code)]
mod ratchet_support;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ratchet_support::{
    in_test_support_root, production_rust_files, resolve_crate_relative,
    strip_line_anchored_cfg_test_items, workspace_root,
};

/// Why a reparse site is *not* a projection reparse. Every allowlisted entry
/// names one of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReparseCategory {
    /// One-time compile of a manifest into a resolved record at install/ingest.
    Compiler,
    /// Compile of a host-bundled static manifest asset — there is no installed
    /// resolved record for a pure bundled descriptor.
    BundledAsset,
}

/// `(path, call_count, category, justification)`. Every production reparse site
/// lives here; the call count is pinned so a new reparse in an existing
/// compiler module still forces a conscious allowlist edit.
const ALLOWLIST: &[(&str, usize, ReparseCategory, &str)] = &[
    (
        "crates/ironclaw_extension_registry/src/lib.rs",
        1,
        ReparseCategory::Compiler,
        "the canonical manifest-file loader/parser in the manifest-owning crate (load_package_entry)",
    ),
    (
        "crates/ironclaw_host_runtime/src/memory_native_extension.rs",
        1,
        ReparseCategory::BundledAsset,
        "native_memory_first_party_package — compiles the bundled ironclaw.memory manifest asset (include_str! of crates/extensions/packages/memory-native/manifest.toml since the WS2 colocation, #7037); a host-bundled descriptor has no installed resolved record to project from",
    ),
];

/// Count reparse calls in a production source file, ignoring comment lines so a
/// doc-comment mention of the API is not a false positive.
fn count_reparse_calls(source: &str) -> usize {
    let stripped = strip_line_anchored_cfg_test_items(source);
    stripped
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("//") && !trimmed.starts_with("/*") && !trimmed.starts_with('*')
        })
        .map(|line| {
            line.matches("ExtensionManifest::parse(").count()
                + line.matches("ExtensionManifestRecord::from_toml(").count()
        })
        .sum()
}

fn read_failure(path: &Path, error: std::io::Error) -> String {
    format!(
        "failed to read {} — an unreadable path must fail this gate, not vanish from the \
         scan: {error}",
        path.display()
    )
}

/// `(production files scanned, workspace-relative path -> reparse calls)`
/// under `scan_root`, paths relative to `root`.
fn production_reparse_counts(root: &Path, scan_root: &Path) -> (usize, BTreeMap<String, usize>) {
    // `production_rust_files` fails the gate on an unreadable directory (a
    // subtree that vanished would report zero reparse sites for files it never
    // opened, CHECKLIST WS0 / #6963) and classifies test code by cfg gating:
    // until 2026-11-17 this walk dropped files by NAME (`tests.rs`,
    // `*_tests.rs`, any `tests/` or `test_support/` directory), so an ungated
    // `*_tests.rs` could reparse unseen (gate audit 2026-08 §9). The
    // feature-gated `src/test_support` root stays out, as in the LOC counters.
    let files: Vec<PathBuf> = production_rust_files(scan_root)
        .into_iter()
        .filter(|path| !in_test_support_root(path))
        .collect();
    let mut found = BTreeMap::new();
    for path in &files {
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("{}", read_failure(path, error)));
        let count = count_reparse_calls(&source);
        if count > 0 {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            found.insert(rel, count);
        }
    }
    (files.len(), found)
}

/// Test code is decided by cfg gating, not by file name (gate audit 2026-08
/// §9): an ungated `probe_tests.rs` ships and its reparse counts; a
/// production-named `helpers.rs` reachable only through `#[cfg(test)]` is test
/// code. The feature-gated `src/test_support` root stays out, as in the LOC
/// counters, and crate-level `tests/` is a separate cargo target.
#[test]
fn reparse_scan_classifies_test_files_by_cfg_not_by_name() {
    let tree = tempfile::tempdir().expect("temp tree");
    let crate_dir = tree.path().join("crates/fixture");
    let src = crate_dir.join("src");
    std::fs::create_dir_all(src.join("test_support")).expect("src");
    std::fs::create_dir_all(crate_dir.join("tests")).expect("tests");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\n",
    )
    .expect("manifest");
    std::fs::write(
        src.join("lib.rs"),
        "pub mod probe_tests;\n#[cfg(test)]\nmod helpers;\n\
         #[cfg(feature = \"test-support\")]\npub mod test_support;\n",
    )
    .expect("lib.rs");
    let reparse = "pub fn f(t: &str) { let _m = ExtensionManifest::parse(t); }\n";
    for name in [
        "src/probe_tests.rs",
        "src/helpers.rs",
        "src/test_support/mod.rs",
        "tests/it.rs",
    ] {
        std::fs::write(crate_dir.join(name), reparse).expect("fixture file");
    }
    let (_, found) = production_reparse_counts(tree.path(), &tree.path().join("crates"));
    assert_eq!(
        found.keys().cloned().collect::<Vec<_>>(),
        vec!["crates/fixture/src/probe_tests.rs".to_string()],
        "only the ungated `probe_tests.rs` ships a reparse"
    );
}

/// The walk refuses an unreadable path rather than shrinking around it. The
/// floor cannot cover this: one unreadable crate tree leaves the file count far
/// above 500 while the reparse census silently loses everything under it.
#[test]
#[should_panic(expected = "read_dir")]
fn an_unreadable_path_fails_the_walk_rather_than_disappearing_from_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let not_a_directory = temp.path().join("regular-file");
    std::fs::write(&not_a_directory, "// fixture\n").expect("fixture file");
    let _ = production_reparse_counts(temp.path(), &not_a_directory);
}

/// Generated trees stay excluded, so a `pnpm install` cannot inflate the scan
/// with vendored sources.
#[test]
fn the_walk_skips_generated_trees() {
    let temp = tempfile::tempdir().expect("tempdir");
    let reparse = "pub fn f(t: &str) { let _m = ExtensionManifest::parse(t); }\n";
    for generated in ["target", "node_modules", ".git"] {
        let vendored = temp.path().join(format!("tree/{generated}/pkg"));
        std::fs::create_dir_all(&vendored).expect("vendored tree");
        std::fs::write(vendored.join("lib.rs"), reparse).expect("vendored source");
    }
    std::fs::write(temp.path().join("tree/real.rs"), reparse).expect("real source");
    let (scanned, found) = production_reparse_counts(temp.path(), &temp.path().join("tree"));
    assert_eq!(scanned, 1, "the walk must skip generated trees: {found:?}");
    assert_eq!(
        found.keys().cloned().collect::<Vec<_>>(),
        vec!["tree/real.rs".to_string()]
    );
}

#[test]
fn manifest_reparse_stays_within_the_compiler_and_bundled_paths() {
    let root = workspace_root();
    // The scan used to cover `["crates", "src"]`; `src` was the v1 monolith and
    // is long gone. The old walk skipped a missing directory silently, so
    // the dead entry was invisible — and the same silence is what a wrong root
    // produces. The scan root must exist (CHECKLIST WS0, #6963).
    let crates_dir = root.join("crates");
    assert!(
        crates_dir.is_dir(),
        "manifest-reparse scan root {} does not exist — repoint it rather than \
         scanning nothing",
        crates_dir.display()
    );
    let (scanned, found) = production_reparse_counts(&root, &crates_dir);
    assert!(
        scanned >= 500,
        "manifest-reparse scan collected only {scanned} file(s) — the walk is broken, not the \
         workspace; an empty scan reports zero reparse sites and passes."
    );

    // ALLOWLIST paths keep their readable flat `crates/<crate>/…` spelling and
    // are RESOLVED through the crate inventory before being matched against a
    // scanned file. Without that, the family move (PROPOSAL §5) makes every
    // entry stale AND every real site look new (CHECKLIST WS10).
    let allow: BTreeMap<String, usize> = ALLOWLIST
        .iter()
        .map(|(path, count, _, _)| (resolve_crate_relative(&root, path), *count))
        .collect();

    let mut problems = Vec::new();
    for (path, count) in &found {
        match allow.get(path) {
            None => problems.push(format!(
                "NEW manifest reparse in `{path}` ({count} call(s)) — a projection/loader path must \
                 read the persisted resolved record (from_resolved / resolved.to_internal), not \
                 reparse raw TOML. If this is a compiler/bundled-asset site, add it to \
                 ALLOWLIST with its category and justification."
            )),
            Some(expected) if expected != count => problems.push(format!(
                "manifest reparse count changed in `{path}`: allowlist says {expected}, found \
                 {count}. Update the ALLOWLIST count and confirm the new call is not a projection \
                 reparse."
            )),
            Some(_) => {}
        }
    }
    for path in allow.keys() {
        if !found.contains_key(path) {
            problems.push(format!(
                "STALE allowlist entry `{path}` — no manifest reparse found there anymore. Remove \
                 the entry (the list is shrink-only)."
            ));
        }
    }

    assert!(
        problems.is_empty(),
        "REC-1 manifest-reparse gate:\n  - {}",
        problems.join("\n  - ")
    );
}
