//! Anti-slippage ratchet for the deployment-mode *branching* axis (§4.4 / §10
//! of `docs/internal/reborn/contracts/runtime-profiles.md`).
//!
//! Its two siblings own deployment mode as a **type name**
//! (`reborn_standalone_typename_ratchet`, `reborn_deployment_mode_typename_ratchet`).
//! This one owns the behaviour those names were a symptom of: **code that
//! reads a deployment mode to decide what to do.**
//!
//! §4.4 is explicit that mode and lane get opposite treatment, and why:
//!
//! > A deployment mode must be branched on in exactly **zero** places past the
//! > composition edge — that is the whole §2.1 thesis — so giving it an enum
//! > would hand every crate an invitation to `match` on it (which is precisely
//! > how the 66-identifier `Standalone*` family grew).
//!
//! `RebornCompositionProfile` *is* such an enum. It survives as the CLI/env
//! parse artifact and as a display label; what must not survive is consumers
//! reading its variants to select behaviour. `DeploymentConfig` (§5.6) is where
//! a profile becomes data — substrate, traffic policy, readiness contract,
//! storage shape — and everything downstream reads those fields.
//!
//! ## What this freezes
//!
//! The **set** of production files under `crates/app/ironclaw_composition/src`
//! that name a `RebornCompositionProfile` variant. Set membership, not a count,
//! per §10: a count lets a new violation silently replace a retired one; only
//! set membership catches a *swap*. A file entering the set fails; a file
//! leaving it must be removed from the allowlist in the same PR, so the debt
//! can only shrink.
//!
//! This is deliberately coarser than "detect a `match`": variant paths are what
//! branching needs, and a line-based scan cannot reliably tell
//! `match p { Profile::X => .. }` from `if p == Profile::X` from
//! `matches!(p, Profile::X | ..)` — all three are the same debt.
//!
//! ## Definition of done
//!
//! The allowlist reaches `{deployment.rs}` — `DeploymentConfig::for_profile`,
//! the one place a profile name becomes deployment data. The remaining entries
//! and why each is still here are documented on the allowlist itself.
//!
//! **Owner:** the #6274 driver (Illia Polosukhin) — the person driving this
//! allowlist to `{deployment.rs}`. §10 requires every ratchet to name one; an
//! unowned ratchet is telemetry, not a gate.
//!
//! Scanner semantics: comments and string literals are stripped before
//! matching, so this file's own doc comment and fixtures do not self-trip.
//! Scans `ratchet_support::production_rust_files`: files reachable only
//! through test-gated `mod` declarations are skipped whatever their name —
//! test fixtures naming a profile are not production branching, and a shipped
//! `*_tests.rs` is.

#![deny(clippy::let_underscore_must_use)]
#[allow(dead_code)]
mod ratchet_support;

use std::collections::BTreeSet;
use std::path::Path;

// Crate paths are spelled flat (`crates/ironclaw_x/...`) and RESOLVED through
// the crate inventory, so the family move (PROPOSAL section 5) repoints them
// without editing the literals. Identity on today's tree - pinned by
// `reborn_crate_inventory.rs` (CHECKLIST WS10).
use ratchet_support::{
    crate_path, production_rust_files, strip_comments_and_strings, workspace_root,
};

/// Production files under composition `src/` allowed to name a
/// `RebornCompositionProfile` variant, each with the reason it is still here.
///
/// Sorted; entries are `src/`-relative with `/` separators.
const ALLOWLIST: &[(&str, &str)] = &[
    (
        "deployment.rs",
        "TARGET STATE — `DeploymentConfig::for_profile` is the one place a \
         profile name becomes deployment data (§4.4). This entry stays.",
    ),
    (
        "memory_binding.rs",
        "Maps the composition profile to a typed `MemoryDeploymentProfile` for \
         the fail-closed memory profile-binding certification policy (#3537): \
         production rejects unverified third-party bindings absent an admin \
         override; standalone permits them. The branch produces a typed \
         memory-deployment axis, not a raw label. Retires into `DeploymentConfig` \
         when it grows a memory-binding axis (#5264).",
    ),
];

/// `src`-relative paths of the production files under `src` that name a
/// `RebornCompositionProfile` variant. Production is
/// `ratchet_support::production_rust_files` — a file is test code only when
/// every `mod` declaration reaching it is test-gated, whatever its name; until
/// 2026-11-17 this walker skipped `*tests.rs` names and `*_tests/` directories
/// instead (gate audit 2026-08 §9).
fn branching_files(src: &Path) -> BTreeSet<String> {
    production_rust_files(src)
        .into_iter()
        .filter(|path| {
            let source = std::fs::read_to_string(path)
                .unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
            strip_comments_and_strings(&source).contains("RebornCompositionProfile::")
        })
        .map(|path| {
            path.strip_prefix(src)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

#[test]
fn deployment_mode_branching_allowlist_is_frozen_and_only_shrinks() {
    let root = crate_path(&workspace_root(), "crates/ironclaw_composition/src");
    let found = branching_files(&root);

    let allowed: BTreeSet<String> = ALLOWLIST
        .iter()
        .map(|(path, _)| (*path).to_string())
        .collect();

    let new_debt: Vec<&String> = found.difference(&allowed).collect();
    assert!(
        new_debt.is_empty(),
        "new deployment-mode branching in composition: {new_debt:?}\n\
         A `RebornCompositionProfile` variant in a production file means code is \
         reading a deployment mode to decide what to do (§4.4). Add the axis to \
         `DeploymentConfig` and read that field instead. If the reference is a \
         display label with no behaviour attached, add the file to ALLOWLIST \
         with that justification."
    );

    let retired: Vec<&String> = allowed.difference(&found).collect();
    assert!(
        retired.is_empty(),
        "ALLOWLIST names files that no longer reference a composition profile: \
         {retired:?}\n\
         The ratchet may only shrink: delete these entries in the same PR that \
         retired them, so the allowlist keeps meaning what it says."
    );
}

#[test]
fn deployment_rs_is_the_target_state_entry() {
    // The definition of done is `{deployment.rs}`. Pin that the target entry is
    // present and documented as terminal, so a future cleanup does not
    // accidentally drive the allowlist to empty and delete the one place a
    // profile is *supposed* to become data.
    let target = ALLOWLIST
        .iter()
        .find(|(path, _)| *path == "deployment.rs")
        .expect("deployment.rs must stay on the allowlist as the target state");
    assert!(
        target.1.contains("TARGET STATE"),
        "deployment.rs's allowlist reason must mark it terminal, got: {}",
        target.1
    );
}

#[test]
fn allowlist_is_sorted_and_unique() {
    let paths: Vec<&str> = ALLOWLIST.iter().map(|(path, _)| *path).collect();
    let mut sorted = paths.clone();
    sorted.sort_unstable();
    assert_eq!(
        paths, sorted,
        "ALLOWLIST must stay sorted for reviewability"
    );
    let unique: BTreeSet<&str> = paths.iter().copied().collect();
    assert_eq!(unique.len(), paths.len(), "ALLOWLIST has duplicate entries");
}

#[test]
fn scanner_strips_comments_and_strings() {
    // Self-test (§10: every check ships with its own self-test). Without
    // stripping, this ratchet's own doc comment would put it on the list.
    let source = r#"
        // RebornCompositionProfile::Standalone in a line comment
        /* RebornCompositionProfile::Production in a block comment */
        let label = "RebornCompositionProfile::Disabled";
    "#;
    let stripped = strip_comments_and_strings(source);
    assert!(
        !stripped.contains("RebornCompositionProfile::"),
        "stripped source still contains a variant path: {stripped}"
    );

    let real = "match profile { RebornCompositionProfile::Standalone => 1, _ => 0 }";
    assert!(
        strip_comments_and_strings(real).contains("RebornCompositionProfile::"),
        "real branching must survive stripping"
    );

    // Regression (2026-07-19 gemini review): a char literal containing `"` must
    // not open a string and swallow a following branch. Before the char-literal
    // handling, `'"'` flipped in_string and the DeploymentMode match after it
    // was hidden from the scan — a silent ratchet false negative.
    let with_char_literal = r#"
        let quote = '"';
        match profile { RebornCompositionProfile::Standalone => 1, _ => 0 }
    "#;
    assert!(
        strip_comments_and_strings(with_char_literal).contains("RebornCompositionProfile::"),
        "a `'\"'` char literal must not hide the branch after it"
    );
    // The char literal itself is dropped (it is not a lifetime), and a real
    // lifetime is preserved.
    assert!(!strip_comments_and_strings("let c = '\"';").contains('"'));
    assert!(strip_comments_and_strings("fn f<'a>(x: &'a str) {}").contains("'a"));
}

/// Test code is decided by cfg gating, not by file name (gate audit 2026-08
/// §9): an ungated `probe_tests.rs` ships and its branching counts; a
/// production-named `helpers.rs` reachable only through `#[cfg(test)] mod
/// helpers;` is test code and does not.
#[test]
fn branching_scan_classifies_test_files_by_cfg_not_by_name() {
    let tree = tempfile::tempdir().expect("temp tree");
    let src = tree.path().join("src");
    std::fs::create_dir_all(&src).expect("src");
    let branch =
        "pub fn f(p: P) -> u8 { match p { RebornCompositionProfile::Standalone => 1, _ => 0 } }\n";
    std::fs::write(
        src.join("lib.rs"),
        "pub mod probe_tests;\n#[cfg(test)]\nmod helpers;\n",
    )
    .expect("lib.rs");
    std::fs::write(src.join("probe_tests.rs"), branch).expect("probe_tests.rs");
    std::fs::write(src.join("helpers.rs"), branch).expect("helpers.rs");

    let found = branching_files(&src);
    assert!(
        found.contains("probe_tests.rs"),
        "an ungated `pub mod probe_tests;` ships; its branching must be found: {found:?}"
    );
    assert!(
        !found.contains("helpers.rs"),
        "`helpers.rs` is reachable only through `#[cfg(test)]`; it is test code: {found:?}"
    );
}
