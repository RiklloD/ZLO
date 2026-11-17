//! Positive + negative fixtures for the **shared** source scanners in
//! `ratchet_support` (CHECKLIST WS10, the scanner-consolidation row).
//!
//! Why this binary exists. The row that ordered the consolidation ends with the
//! rule that makes it safe: *"the consolidated helpers land with their own
//! positive/negative fixtures, since every gate's non-vacuity then depends on
//! them."* Before consolidation a syntax shape each copy mishandled cost one
//! gate; now it costs every gate at once, so the shared bodies need fixtures
//! that are **not** owned by any single gate — a gate can be deleted, and its
//! fixtures with it, long after twenty other gates started depending on the
//! helper it was proving.
//!
//! `ratchet_support` itself cannot hold them: it is a `mod` compiled into ~37
//! separate integration-test binaries, so a `#[test]` there would be collected
//! and run 37 times and would report a helper regression 37 times over.
//!
//! (An earlier version of this comment claimed the `reborn_*` FILE name puts
//! this binary in `code_style.yml`'s `cargo test -p ironclaw_architecture_tests
//! reborn` smoke lane. It does not: that argument is a TEST-NAME filter — the
//! sibling `reborn_contracts_vendor_census.rs` documents the measurement — and
//! none of this file's `#[test]` fns carries the substring, so the smoke lane
//! runs 0 of them. These fixtures run in the reborn-tests full plan, which is
//! the lane that matters; noted here so nobody trusts the file name for lane
//! coverage again.)
//!
//! **Two `#[cfg(test)]` strippers, deliberately.** The measurement that
//! motivated splitting them is in `ratchet_support`'s own module comment; what
//! this file adds is the fixture proving each family behaves the way its
//! callers depend on, *including* the shape the other family gets wrong.

#![deny(clippy::let_underscore_must_use)]
#[allow(dead_code)]
mod ratchet_support;

use std::path::Path;

use ratchet_support::{
    balanced_angle_close, cfg_test_only_files, implemented_trait_names, is_rust_identifier,
    production_line_count, production_rust_files, strip_cfg_test_blocks,
    strip_comments_and_strings, strip_line_anchored_cfg_test_items, try_nested_workspace_roots,
    unresolved_module_edges,
};

// ---------------------------------------------------------------------------
// strip_cfg_test_blocks — the byte-scanning family
// ---------------------------------------------------------------------------

#[test]
fn byte_scanning_stripper_removes_gated_items_and_keeps_production() {
    let source = "\
pub struct Before;
#[cfg(test)]
mod tests {
    pub struct Gated;
    fn nested() { let _ = 1; }
}
pub struct After;
";
    let stripped = strip_cfg_test_blocks(source);
    assert!(
        stripped.contains("Before") && stripped.contains("After"),
        "production items on both sides of a gated block must survive: {stripped:?}"
    );
    assert!(
        !stripped.contains("Gated"),
        "a `#[cfg(test)]` item must not reach a production scan: {stripped:?}"
    );
}

/// The guard `reborn_registration_pipeline_boundary.rs`'s private copy shipped
/// without: a bodiless gated item must end at its own `;`, not brace-balance
/// from the next item's block and eat it.
#[test]
fn byte_scanning_stripper_ends_a_bodiless_gated_item_at_its_semicolon() {
    for bodiless in [
        "pub struct Before;\n#[cfg(test)]\nmod tests;\npub struct After { field: u8 }\n",
        "pub struct Before;\n#[cfg(test)]\nuse std::fmt::Debug;\npub struct After { field: u8 }\n",
    ] {
        let stripped = strip_cfg_test_blocks(bodiless);
        assert!(
            stripped.contains("After"),
            "a gated item with no block must not swallow the item after it: {stripped:?}"
        );
        assert!(
            !stripped.contains("mod tests;") && !stripped.contains("use std::fmt::Debug;"),
            "the gated declaration itself must still be removed: {stripped:?}"
        );
    }
}

/// `#[cfg(feature = "test-support")]` compiles into real builds (CI runs
/// `--all-features`), so an item behind it is a genuine edge and must survive.
#[test]
fn byte_scanning_stripper_keeps_feature_gated_items() {
    let source = "#[cfg(feature = \"test-support\")]\npub struct FeatureGated;\n";
    assert!(
        strip_cfg_test_blocks(source).contains("FeatureGated"),
        "only `#[cfg(test)]` is invisible to a shipped artifact"
    );
}

/// NEGATIVE: the documented precondition. Fed raw source, the byte scanner
/// starts a brace walk from text inside a comment or a string literal. Pinned
/// rather than fixed, because the two callers that pass raw source do so on
/// purpose (`ratchet_support` names them) and closing it would change what they
/// count. If this test ever goes green the hazard changed shape — re-derive the
/// callers' safety argument, do not delete the test.
#[test]
fn byte_scanning_stripper_is_unsound_on_raw_source_which_is_why_callers_pre_strip() {
    let source = "\
#[cfg(test)]
mod tests {
    /// an unbalanced brace in a doc comment: {
    const S: &str = \"{{{\";
}
pub struct Production;
";
    assert!(
        !strip_cfg_test_blocks(source).contains("Production"),
        "the raw-source hazard is the reason every sound caller composes this with \
         strip_comments_and_strings first — if raw input now works, the composition rule in \
         ratchet_support needs rewriting, not deleting"
    );
    assert!(
        strip_cfg_test_blocks(&strip_comments_and_strings(source)).contains("Production"),
        "the documented composition must survive braces hidden in comments and strings"
    );
}

// ---------------------------------------------------------------------------
// strip_line_anchored_cfg_test_items — the raw-source family
// ---------------------------------------------------------------------------

#[test]
fn line_anchored_stripper_removes_gated_items_and_keeps_production() {
    let source = "\
pub struct Before;
#[cfg(test)]
mod tests {
    pub struct Gated;
}
pub struct After;
";
    let stripped = strip_line_anchored_cfg_test_items(source);
    assert!(stripped.contains("Before") && stripped.contains("After"));
    assert!(!stripped.contains("Gated"));
}

/// The property its two callers depend on and the byte-scanning family cannot
/// provide: fed **raw** source, a `#[cfg(test)]` written inside a comment or a
/// string literal must not strip anything. `reborn_extension_specificity`
/// treats a vendor name in a comment as a violation and so cannot pre-strip;
/// if this regressed, a vendor name could be hidden from that gate by writing
/// `#[cfg(test)]` in a comment above it.
#[test]
fn line_anchored_stripper_ignores_a_marker_inside_a_comment_or_string() {
    let source = "\
// #[cfg(test)]
pub struct AfterCommentMarker;
const S: &str = \"#[cfg(test)]\";
pub struct AfterStringMarker;
";
    let stripped = strip_line_anchored_cfg_test_items(source);
    assert!(
        stripped.contains("AfterCommentMarker") && stripped.contains("AfterStringMarker"),
        "a marker that is not the first token of its line is not an attribute: {stripped:?}"
    );
}

/// NEGATIVE, and the reason the families are not merged: the byte scanner gets
/// the same raw input wrong, in the fail-open direction (it deletes production
/// code the specificity gate is supposed to scan).
#[test]
fn the_two_strippers_disagree_on_raw_source_and_that_is_the_split() {
    let source = "\
// #[cfg(test)]
mod not_a_gated_module {
    pub struct VendorNameLivesHere;
}
";
    assert!(
        strip_line_anchored_cfg_test_items(source).contains("VendorNameLivesHere"),
        "the raw-source family must keep it"
    );
    assert!(
        !strip_cfg_test_blocks(source).contains("VendorNameLivesHere"),
        "the byte-scanning family must be shown to lose it — if this fires, the two families \
         have converged and the split in ratchet_support can be revisited deliberately"
    );
}

#[test]
fn line_anchored_stripper_ends_a_bodiless_gated_item_at_its_semicolon() {
    let stripped = strip_line_anchored_cfg_test_items(
        "pub struct A;\n#[cfg(test)]\nmod tests;\npub struct B;\n",
    );
    assert!(stripped.contains("A") && stripped.contains("B"));
    assert!(!stripped.contains("mod tests;"));
}

/// Braces inside a test item's string and char literals, and a multiline
/// string line starting `#[cfg(test)]`, must not move the raw-source family:
/// both used to strip every production line after them (gate audit 2026-08
/// §9). Anchor and depth now read the stripped twin of each line.
#[test]
fn line_anchored_stripper_ignores_braces_and_markers_inside_literals() {
    let braces = "#[cfg(test)]\nmod tests {\n    const S: &str = \"{{{{\";\n    \
                  const C: char = '{';\n}\npub struct AfterBraces;\n";
    let stripped = strip_line_anchored_cfg_test_items(braces);
    assert!(stripped.contains("AfterBraces"), "{stripped:?}");
    assert!(
        !stripped.contains("const C"),
        "the test item is still stripped: {stripped:?}"
    );
    let marker_in_string =
        "const DOC: &str = \"\n#[cfg(test)]\nmod x {\";\npub struct AfterString;\n";
    let stripped = strip_line_anchored_cfg_test_items(marker_in_string);
    assert!(stripped.contains("AfterString"), "{stripped:?}");
    // Raw lines are still what it emits (its callers scan comments).
    assert!(
        strip_line_anchored_cfg_test_items("// vendor note\npub struct K;\n")
            .contains("// vendor note")
    );
}

/// A `\<newline>` string continuation still ends a source line: dropping it
/// undercounted lines and shifted every later offender's line number.
#[test]
fn comment_and_string_stripper_keeps_escaped_newlines() {
    let source = "let s = \"a\\\n    b\";\nlet marker = 1;\n";
    assert_eq!(source.lines().count(), 3);
    let stripped = strip_comments_and_strings(source);
    assert_eq!(stripped.lines().count(), 3, "{stripped:?}");
    assert_eq!(stripped.lines().nth(2), Some("let marker = 1;"));
}

// ---------------------------------------------------------------------------
// balanced_angle_close
// ---------------------------------------------------------------------------

#[test]
fn balanced_angle_close_balances_nested_and_rejects_truncated() {
    assert_eq!(balanced_angle_close("<T>"), Some(2));
    // Nested: taking the FIRST `>` would leave `> Port`, not an identifier, and
    // the impl would be skipped — a gate enforcing nothing for it.
    let nested = "<T: Iterator<Item = X>>";
    assert_eq!(balanced_angle_close(nested), Some(nested.len() - 1));
    // A `->` inside a bound is a return arrow, not a close.
    let arrow = "<F: Fn(&str) -> bool>";
    assert_eq!(balanced_angle_close(arrow), Some(arrow.len() - 1));
    // NEGATIVE: never balances.
    assert_eq!(balanced_angle_close("<T: Iterator<Item = X>"), None);
}

// ---------------------------------------------------------------------------
// implemented_trait_names
// ---------------------------------------------------------------------------

#[test]
fn implemented_trait_names_reads_every_impl_shape_and_rejects_non_impls() {
    let source = "\
impl Plain for Thing {}
impl ironclaw_assistant::Qualified<T> for Thing {}
impl<'a, T: Iterator<Item = X>> Generic for Host<T> {}
impl<F: Fn(&str) -> bool> WithArrowBound for Host<F> {}
impl Inherent { fn method() {} }
#[cfg(test)]
mod tests {
    impl GatedOnly for Double {}
}
";
    let found = implemented_trait_names(source);
    for expected in ["Plain", "Qualified", "Generic", "WithArrowBound"] {
        assert!(found.contains(expected), "missed {expected}: {found:?}");
    }
    assert!(
        !found.contains("Inherent"),
        "an inherent impl has no `for` and is not an implemented trait: {found:?}"
    );
    assert!(
        !found.contains("GatedOnly"),
        "a `#[cfg(test)]` impl is not a production edge: {found:?}"
    );
}

// ---------------------------------------------------------------------------
// is_rust_identifier
// ---------------------------------------------------------------------------

#[test]
fn is_rust_identifier_accepts_identifiers_and_rejects_fragments() {
    for accepted in ["Foo", "_private", "Foo2", "snake_case"] {
        assert!(is_rust_identifier(accepted), "{accepted} is an identifier");
    }
    // The fragments a substring-oriented scan produces when it reads a shape it
    // does not understand. Accepting any of these would let a gate record a
    // non-name as a trait and satisfy a pin with it.
    for rejected in ["", "2Foo", "a::b", "Foo<T>", "Foo ", "&Foo", "-"] {
        assert!(
            !is_rust_identifier(rejected),
            "{rejected:?} must not read as an identifier"
        );
    }
}

// ---------------------------------------------------------------------------
// try_nested_workspace_roots — the walk must fail closed

#[test]
fn nested_workspace_roots_reports_an_unreadable_subtree_instead_of_skipping_it() {
    // Regression: the walk's error used to be discarded with `let _ =`, so an
    // unreadable directory under `crates/` read as "no nested workspace there"
    // and the location scans that subtract these roots ran over it unseen.
    let fixture = tempfile::tempdir().expect("fixture root");
    let crates_root = fixture.path().join("crates");
    let guest = crates_root.join("guest");
    std::fs::create_dir_all(&guest).expect("guest dir");
    std::fs::write(guest.join("Cargo.toml"), "[workspace]\n").expect("guest manifest");
    assert_eq!(
        try_nested_workspace_roots(fixture.path()).expect("readable tree walks"),
        vec!["crates/guest".to_string()],
        "positive control: a readable nested workspace is found"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let locked = crates_root.join("locked");
        std::fs::create_dir_all(&locked).expect("locked dir");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("lock the directory");
        // Root reads a 0000 directory, so the fixture only bites unprivileged.
        let fixture_bites = std::fs::read_dir(&locked).is_err();
        let outcome = try_nested_workspace_roots(fixture.path());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("unlock for cleanup");
        if fixture_bites {
            let error = outcome.expect_err("an unreadable subtree must fail the walk");
            assert!(error.contains("cannot read"), "{error}");
        } else {
            eprintln!("SKIPPED: running as a user that can read a 0000 directory");
        }
    }
}

// ---------------------------------------------------------------------------
// cfg_test_only_files / production_rust_files — the one file classification
// (gate audit 2026-08 §9 residuals 1 and 2)

fn write_tree(root: &Path, files: &[(&str, &str)]) {
    for (relative, contents) in files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("fixture dir");
        }
        std::fs::write(&path, contents).expect("fixture file");
    }
}

/// Residual 1: a `src/**/tests/` directory was test-only BY DIRECTORY, so a
/// `pub mod tests;` directory module that ships was dropped from every count.
/// A module directory is classified like any other file — by its declarations
/// — and only a crate-level `tests/` / `examples/` / `benches/` directory (the
/// one beside a `Cargo.toml`, which cargo compiles as separate targets) is
/// excluded by directory.
#[test]
fn src_tests_directory_is_classified_by_declaration_not_by_directory() {
    let tree = tempfile::tempdir().expect("temp tree");
    let root = tree.path();
    write_tree(
        root,
        &[
            ("Cargo.toml", "[package]\nname = \"fixture\"\n"),
            ("build.rs", "fn main() {}\n"),
            ("src/lib.rs", "pub mod tests;\n#[cfg(test)]\nmod gated;\n"),
            // Ungated directory module: ships, so it and its child count.
            ("src/tests/mod.rs", "pub mod shipped;\n"),
            ("src/tests/shipped.rs", "pub fn shipped() {}\n"),
            // Gated directory module: test-only, transitively.
            ("src/gated/mod.rs", "mod tests;\n"),
            ("src/gated/tests/mod.rs", "mod helpers;\n"),
            ("src/gated/tests/helpers.rs", "pub fn helper() {}\n"),
            // Crate-level targets: never production, whatever they declare.
            ("tests/integration.rs", "mod common;\n"),
            ("tests/common/mod.rs", "pub fn shared() {}\n"),
            ("examples/demo.rs", "fn main() {}\n"),
            ("benches/bench.rs", "fn main() {}\n"),
        ],
    );
    let src = root.join("src");

    for (walk_root, label) in [(src.as_path(), "src"), (root, "crate dir")] {
        let production = production_rust_files(walk_root);
        for shipped in ["src/lib.rs", "src/tests/mod.rs", "src/tests/shipped.rs"] {
            assert!(
                production.contains(&root.join(shipped)),
                "{shipped} is reachable through an ungated `pub mod tests;` and ships; the \
                 {label} walk dropped it: {production:?}"
            );
        }
        for test_only in [
            "src/gated/mod.rs",
            "src/gated/tests/mod.rs",
            "src/gated/tests/helpers.rs",
        ] {
            assert!(
                !production.contains(&root.join(test_only)),
                "{test_only} is reachable only through `#[cfg(test)] mod gated;`; the {label} \
                 walk counted it: {production:?}"
            );
            assert!(
                cfg_test_only_files(walk_root).contains(&root.join(test_only)),
                "{test_only} must be in the test-only set of the {label} walk"
            );
        }
    }

    // From the crate directory: crate-level targets are excluded by directory,
    // the build script is compiled code and counts.
    let production = production_rust_files(root);
    for target in [
        "tests/integration.rs",
        "tests/common/mod.rs",
        "examples/demo.rs",
        "benches/bench.rs",
    ] {
        assert!(
            !production.contains(&root.join(target)),
            "{target} is a separate cargo target, not production: {production:?}"
        );
    }
    assert!(
        production.contains(&root.join("build.rs")),
        "{production:?}"
    );
}

/// Residual 2: `#[cfg(test)] mod x_tests;` written on ONE line was read as
/// ungated — the tracker took one attribute per line — so the file counted as
/// production. A leading attribute run on the item's own line gates it exactly
/// as the same attributes on lines above would.
#[test]
fn single_line_gated_declaration_is_test_only() {
    let tree = tempfile::tempdir().expect("temp tree");
    let src = tree.path().join("src");
    write_tree(
        &src,
        &[
            (
                "lib.rs",
                "#[cfg(test)] mod one_tests;\n\
                 #[cfg(test)] pub(crate) mod helper;\n\
                 #[allow(dead_code)] #[cfg(all(test, unix))] mod stacked;\n\
                 #[cfg(test)] // a trailing note\nmod noted;\n\
                 #[cfg(not(test))] mod shipped_tests;\n\
                 #[allow(dead_code)] mod also_shipped;\n\
                 #[cfg(any(test, feature = \"test-support\"))] pub mod fixtures_tests;\n",
            ),
            ("one_tests.rs", "pub fn t() {}\n"),
            ("helper.rs", "pub fn h() {}\n"),
            ("stacked.rs", "pub fn s() {}\n"),
            ("noted.rs", "pub fn n() {}\n"),
            ("shipped_tests.rs", "pub fn shipped() {}\n"),
            ("also_shipped.rs", "pub fn shipped() {}\n"),
            ("fixtures_tests.rs", "pub fn shared() {}\n"),
        ],
    );
    let test_only = cfg_test_only_files(&src);
    let production = production_rust_files(&src);
    for gated in ["one_tests.rs", "helper.rs", "stacked.rs", "noted.rs"] {
        assert!(
            test_only.contains(&src.join(gated)) && !production.contains(&src.join(gated)),
            "{gated} is declared only behind a same-line test-only cfg: test-only \
             {test_only:?}, production {production:?}"
        );
    }
    // Negative: a cfg a release build satisfies, a non-cfg attribute, and the
    // strict `test-support` reading (a real build) all leave the file shipped.
    for shipped in [
        "shipped_tests.rs",
        "also_shipped.rs",
        "fixtures_tests.rs",
        "lib.rs",
    ] {
        assert!(
            production.contains(&src.join(shipped)),
            "{shipped} ships and must count: {production:?}"
        );
    }
}

/// The same single-line forms, at line granularity: the item and its body are
/// test context, a non-test attribute on the line changes nothing.
#[test]
fn single_line_test_attribute_gates_its_item_in_the_line_count() {
    assert_eq!(production_line_count("#[cfg(test)] fn f() {}\n"), 0);
    assert_eq!(
        production_line_count("#[cfg(test)] mod t {\n    fn x() {}\n}\npub fn kept() {}\n"),
        1
    );
    assert_eq!(
        production_line_count("#[test] fn t() {\n    body();\n}\n"),
        0
    );
    assert_eq!(production_line_count("#[cfg(test)] use std::fmt;\n"), 0);
    assert_eq!(
        production_line_count("#[cfg(test)] // why\nfn gated() {\n}\n"),
        0,
        "a trailing comment does not un-gate the attribute line"
    );
    // Negative: production items behind ordinary attributes still count.
    assert_eq!(production_line_count("#[inline] pub fn g() {}\n"), 1);
    assert_eq!(
        production_line_count("#[cfg(not(test))] fn real() {\n}\n#[derive(Debug)] struct S;\n"),
        3
    );
    // A `]` inside a string in the attribute does not end it early.
    assert_eq!(
        production_line_count("#[doc = \"]\"] #[cfg(test)] fn f() {}\n"),
        0
    );
}

/// A declaration inside an inline module resolves under that module's
/// directory, as rustc does: `#[cfg(test)] mod tests { mod deep; }` in
/// `tests.rs` names `tests/tests/deep.rs`. The resolver used to look beside the
/// file only, found nothing, and counted the gated child as production (the
/// old `tests/`-directory rule hid it; found by the Rust/Python parity test on
/// `ironclaw_composition`'s `capability_host/tests/tests/display_preview.rs`).
#[test]
fn declaration_inside_an_inline_module_resolves_under_its_directory() {
    let tree = tempfile::tempdir().expect("temp tree");
    let src = tree.path().join("src");
    write_tree(
        &src,
        &[
            ("lib.rs", "pub mod host;\n"),
            (
                "host.rs",
                "#[cfg(test)]\nmod tests;\npub mod outer {\n    pub mod shipped;\n}\n",
            ),
            (
                "host/tests.rs",
                "#[cfg(test)]\nmod tests {\n    mod deep;\n}\n",
            ),
            ("host/tests/tests/deep.rs", "pub fn deep() {}\n"),
            ("host/outer/shipped.rs", "pub fn shipped() {}\n"),
            // Decoy at the flat spelling: must not be taken as the target.
            ("host/tests/deep.rs", "pub fn orphan() {}\n"),
        ],
    );
    let test_only = cfg_test_only_files(&src);
    assert!(
        test_only.contains(&src.join("host/tests/tests/deep.rs")),
        "the gated inline module's child is test-only: {test_only:?}"
    );
    let production = production_rust_files(&src);
    assert!(
        production.contains(&src.join("host/outer/shipped.rs")),
        "an ungated inline module's child ships: {production:?}"
    );
    assert!(
        production.contains(&src.join("host/tests/deep.rs")),
        "nothing declares the flat-spelled decoy, so it is not test-only: {production:?}"
    );
}

/// Fail closed on every edge the resolver cannot prove test-only (GPT review
/// of the §9 sweep): a production edge it misses would classify a shipped file
/// as test code.
///
/// 1. **Every `mod` item on a line**, not only the first: `mod first; mod
///    hidden;` declares `hidden` ungated even though another line gates it.
/// 2. **`include!` / `include_str!` of a `.rs` file** is a production edge to
///    the file, whatever the surrounding cfg.
/// 3. **`#[path]` inside `cfg_attr`** is a production edge to that path.
/// 4. **An edge the resolver cannot resolve** (`include!(concat!(env!(…)))`)
///    makes the whole containing module production, with the reason recorded.
#[test]
fn unprovable_module_edges_fail_closed() {
    let tree = tempfile::tempdir().expect("temp tree");
    let src = tree.path().join("src");
    write_tree(
        &src,
        &[
            (
                "lib.rs",
                "#[cfg(test)]\nmod hidden;\nmod first; mod hidden;\n\
                 #[cfg(test)] mod gated_a; pub mod shipped_b; #[cfg(test)] mod gated_c;\n\
                 #[cfg(test)]\nmod included;\ninclude!(\"included.rs\");\n\
                 #[cfg(test)]\nmod read_as_text;\n\
                 #[cfg(test)]\nmod t {\n    const SRC: &str = include_str!(\"read_as_text.rs\");\n}\n\
                 #[cfg(test)]\nmod swapped;\n\
                 #[cfg_attr(not(test), path = \"swapped.rs\")]\nmod real;\n\
                 pub mod generated;\n\
                 const PROMPT: &[u8] = include_bytes!(concat!(\"prompts/\", \"op\", \".md\"));\n",
            ),
            ("hidden.rs", "pub fn f() {}\n"),
            ("first.rs", "pub fn f() {}\n"),
            ("gated_a.rs", "pub fn f() {}\n"),
            ("shipped_b.rs", "pub fn f() {}\n"),
            ("gated_c.rs", "pub fn f() {}\n"),
            ("included.rs", "pub fn f() {}\n"),
            ("read_as_text.rs", "pub fn f() {}\n"),
            ("swapped.rs", "pub fn f() {}\n"),
            ("real.rs", "pub fn f() {}\n"),
            (
                "generated.rs",
                "include!(concat!(env!(\"OUT_DIR\"), \"/gen.rs\"));\n#[cfg(test)]\nmod helper;\n",
            ),
            ("generated/helper.rs", "pub fn f() {}\n"),
        ],
    );
    let test_only = cfg_test_only_files(&src);
    let production = production_rust_files(&src);
    for shipped in [
        "hidden.rs",
        "first.rs",
        "shipped_b.rs",
        "included.rs",
        "read_as_text.rs",
        "swapped.rs",
        "real.rs",
        "generated.rs",
        "generated/helper.rs",
    ] {
        assert!(
            production.contains(&src.join(shipped)) && !test_only.contains(&src.join(shipped)),
            "{shipped} has an edge the resolver cannot prove test-only; it must count as \
             production: test-only {test_only:?}"
        );
    }
    // Negative: the gated items sharing a line with ungated ones stay test-only.
    for gated in ["gated_a.rs", "gated_c.rs"] {
        assert!(
            test_only.contains(&src.join(gated)),
            "{gated} is declared only behind `#[cfg(test)]`: {test_only:?}"
        );
    }
    // Negative: a non-literal inclusion of a known non-Rust file is no edge.
    let unresolved = unresolved_module_edges(&src);
    assert_eq!(unresolved.len(), 1, "{unresolved:?}");
    assert!(
        unresolved[0].0 == src.join("generated.rs") && unresolved[0].1.contains("include!"),
        "the unresolvable edge is recorded with its file and construct: {unresolved:?}"
    );
}
