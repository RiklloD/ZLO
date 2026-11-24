//! Shared scanner machinery for the §10 anti-slippage ratchets.
//!
//! One hardened implementation of the walk/strip/match pipeline, so every
//! ratchet gets the same guarantees: comment/string stripping, restricted
//! visibility (`pub(crate)`/`pub(super)`/`pub(in …)`), optional `unsafe`/`auto`
//! modifiers, occurrence-preserving scans (same-file duplicates stay visible),
//! and a production-scoped walk (`production_rust_files`: skips `target/`,
//! `node_modules/`, crate-level `tests/` / `examples/` / `benches/`, and files
//! reachable only through test-only `mod` declarations). The type-definition
//! scanners are line-based, not cfg-aware: a pub-visible
//! definition in an inline `#[cfg(test)]` module in src IS inventoried — keep
//! test doubles under `tests/` (or justify an allowlist entry in review).

pub(crate) mod impl_census;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// `#[cfg(test)]` removal — TWO scanners, deliberately, because the callers ask
// two different questions (CHECKLIST WS10, the scanner-consolidation row).
//
// The row that ordered this consolidation described "one `strip_cfg_test_blocks`"
// maintained in four copies. Measured on this tree the copies are **six**
// spellings in **two semantic families**, and collapsing the families onto one
// body would silently change two gates' verdicts:
//
//   * [`strip_cfg_test_blocks`] — the *byte-scanning* family (four byte-identical
//     copies: the two port-inversion gates, `reborn_runner_sheds`,
//     `reborn_transport_product_boundary`, plus `reborn_registration_pipeline_
//     boundary`'s `strip_cfg_test`, which was the same body missing one guard).
//     It finds the marker ANYWHERE and brace-balances, so it is only sound on
//     source whose comments and string literals are already blanked. Its callers
//     all compose it as `strip_cfg_test_blocks(&strip_comments_and_strings(src))`.
//
//   * [`strip_line_anchored_cfg_test_items`] — the *line-anchored* family (two copies:
//     `reborn_extension_specificity`, `reborn_manifest_reparse_gate`). Its
//     callers scan **raw** source on purpose — the specificity gate treats a
//     vendor name in a comment as a violation, and the reparse gate filters
//     comment lines itself afterwards — so they cannot pre-strip. Feeding raw
//     source to the byte-scanning family would let a `#[cfg(test)]` written
//     inside a doc comment or a string literal swallow real production code:
//     the fail-OPEN direction, and the one this whole directory exists to stop.
//
// Two names, one implementation each, and the difference is documented rather
// than discovered again by the next reader.
// ---------------------------------------------------------------------------

/// One matched definition: where it was found and whether the definition line
/// sits under a `#[cfg(...)]` attribute (mutually exclusive compile branches —
/// e.g. the durable/no-durable alias pairs in composition's `factory.rs` — are
/// legitimate same-name definitions, not duplicate debt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TypeDefOccurrence {
    pub path: PathBuf,
    pub cfg_gated: bool,
}

/// The workspace root: the nearest ancestor of `start` holding both a `crates/`
/// directory and a `Cargo.toml`.
///
/// Deliberately a search, not `CARGO_MANIFEST_DIR.parent().parent()`. The fixed
/// two-level form encoded "this crate sits directly under `crates/`", which the
/// family move (`crates/<family>/ironclaw_*`, PROPOSAL §5) makes false — and its
/// failure is silent for any gate that walks a directory: a wrong root makes
/// `root.join("crates")` resolve to `crates/crates`, the walk finds nothing, and
/// the gate passes having scanned zero files (CHECKLIST WS0, #6963).
///
/// This is the crate's ONLY definition of the rule — all twelve private copies
/// were deleted in its favour, including
/// `reborn_registration_pipeline_boundary.rs`'s (a review catch on #6996: it
/// had been left behind with a comment claiming it needed one, which was never
/// true — nothing about resolving scopes prevents a test binary from declaring
/// `mod ratchet_support`).
#[allow(dead_code)]
pub(crate) fn find_workspace_root(start: &Path) -> Result<PathBuf, String> {
    let mut current = Some(start);
    while let Some(directory) = current {
        if directory.join("crates").is_dir() && directory.join("Cargo.toml").is_file() {
            return Ok(directory.to_path_buf());
        }
        current = directory.parent();
    }
    Err(format!(
        "no workspace root above {} — expected an ancestor holding both crates/ and Cargo.toml",
        start.display()
    ))
}

pub(crate) fn workspace_root() -> PathBuf {
    find_workspace_root(Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("architecture crate must sit inside the workspace")
}

// ---------------------------------------------------------------------------
// Crate inventory: the Rust half of the WS10 path-keying rule
//
// Every gate in this directory that names a crate spells it the flat way —
// `crates/ironclaw_llm/src/lib.rs`. The family move (`crates/<family>/…`,
// PROPOSAL §5) makes that spelling false for ~60 crates at once, and a gate
// that resolves it with a bare `root.join(…)` then either scans a directory
// that is not there or asserts against an allowlist nothing can match.
//
// The rule these helpers implement is the one `scripts/ci/lib/crate_tree.py`
// states for the Python-side gates, so the two inventories agree by
// construction and `reborn_crate_inventory.rs` pins them equal:
//
//   * a crate directory is the OUTERMOST directory under `crates/` owning a
//     `Cargo.toml`;
//   * a manifest declaring its own `[workspace]` table is the root of a
//     DIFFERENT workspace (the six `wasm-src` guests) and is not a crate of
//     this one;
//   * `target/` and dotted directories are skipped;
//   * fewer than `MIN_CRATE_DIRECTORIES` results is a broken checkout, not an
//     answer — refuse rather than let a gate report success on an empty scan.
//
// A gate keeps its literal, readable `crates/<crate>/…` spelling and resolves
// it through `crate_path`; the literal becomes a crate NAME plus an in-crate
// remainder rather than a directory path. On today's flat tree resolution is
// the identity, which is what makes adopting it a behavior-free change.
// ---------------------------------------------------------------------------

/// Fail-closed floor, mirroring `crate_tree.py`'s. Far below the real count
/// (65 at the WS0 baseline, 64 after WS7 moved the silk decoder to `tools/`); a handful of results always means the walk lost
/// the tree rather than that the tree shrank.
pub(crate) const MIN_CRATE_DIRECTORIES: usize = 20;

const CRATES_ROOT: &str = "crates";
const WORKSPACE_TABLE_HEADER: &str = "[workspace]";

mod inventory_cache;
#[allow(unused_imports)]
pub(crate) use inventory_cache::try_crate_directories;

/// `try_crate_directories`, panicking on refusal.
#[allow(dead_code)]
pub(crate) fn crate_directories(root: &Path) -> Vec<String> {
    try_crate_directories(root).unwrap_or_else(|error| panic!("{error}"))
}

/// Directories under `crates/` that are roots of a *separate* workspace, and so
/// are excluded from the inventory by construction rather than by omission.
#[allow(dead_code)]
pub(crate) fn nested_workspace_roots(root: &Path) -> Vec<String> {
    try_nested_workspace_roots(root).unwrap_or_else(|error| panic!("{error}"))
}

/// `nested_workspace_roots`, returning the walk failure instead of panicking.
/// Fail-closed like `try_crate_directories`: an unreadable subtree is an error,
/// never a silently shorter list (which would let a guest workspace's types
/// leak into the location scans that subtract these roots).
#[allow(dead_code)]
pub(crate) fn try_nested_workspace_roots(root: &Path) -> Result<Vec<String>, String> {
    let mut roots = Vec::new();
    let crates_root = root.join(CRATES_ROOT);
    if crates_root.is_dir() {
        walk_manifest_dirs(root, &crates_root, &mut |relative, declares_workspace| {
            if declares_workspace {
                roots.push(relative.to_string());
            }
        })?;
    }
    roots.sort();
    Ok(roots)
}

fn collect_crate_manifest_dirs(
    root: &Path,
    directory: &Path,
    out: &mut Vec<String>,
) -> Result<(), String> {
    walk_manifest_dirs(root, directory, &mut |relative, declares_workspace| {
        if !declares_workspace {
            out.push(relative.to_string());
        }
    })
}

fn walk_manifest_dirs(
    root: &Path,
    directory: &Path,
    visit: &mut dyn FnMut(&str, bool),
) -> Result<(), String> {
    let entries = std::fs::read_dir(directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path.is_dir() {
            if name == "target" || name.starts_with('.') {
                continue;
            }
            walk_manifest_dirs(root, &path, visit)?;
            continue;
        }
        if name != "Cargo.toml" {
            continue;
        }
        // Read fail-closed, like `crate_tree.py`: an unreadable manifest counts
        // as a normal crate so a permissions problem surfaces as a build error
        // rather than as a directory quietly leaving every path-keyed gate.
        let declares_workspace = std::fs::read_to_string(&path)
            .map(|text| {
                text.lines()
                    .any(|line| line.trim() == WORKSPACE_TABLE_HEADER)
            })
            .unwrap_or(false);
        let relative = relative_posix(root, directory);
        visit(&relative, declares_workspace);
    }
    Ok(())
}

fn relative_posix(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The crate directory whose basename is `name`, repo-relative.
///
/// Absent or ambiguous is an error, never a silent empty scope: a gate that
/// names a crate must fail at the name it can no longer resolve.
#[allow(dead_code)]
pub(crate) fn try_crate_directory(root: &Path, name: &str) -> Result<String, String> {
    let inventory = try_crate_directories(root)?;
    let matches: Vec<&String> = inventory
        .iter()
        .filter(|directory| directory.rsplit('/').next() == Some(name))
        .collect();
    match matches.as_slice() {
        [only] => Ok((*only).clone()),
        found => Err(format!(
            "expected exactly one crate directory named {name:?} under {CRATES_ROOT}/, found \
             {}: {found:?}. If the crate was renamed or removed, repoint the gate that names \
             it rather than letting it measure an empty tree.",
            found.len()
        )),
    }
}

/// `try_crate_directory`, panicking on refusal.
#[allow(dead_code)]
pub(crate) fn crate_directory(root: &Path, name: &str) -> String {
    try_crate_directory(root, name).unwrap_or_else(|error| panic!("{error}"))
}

/// Absolute path of the crate directory named `name`.
#[allow(dead_code)]
pub(crate) fn crate_dir(root: &Path, name: &str) -> PathBuf {
    root.join(crate_directory(root, name))
}

/// Rewrite a *logical* `crates/<crate>/<remainder>` spelling into the path the
/// crate actually occupies, resolving `<crate>` through the inventory.
///
/// Order matters, and it is what makes adoption behavior-free:
///
///   1. the spec already names a real crate directory → identity (this is the
///      only branch taken on today's flat tree, and the branch that keeps an
///      already-nested crate such as `crates/extensions/ironclaw_extension_support`
///      spelled the way it really sits);
///   2. the spec resolves to something that exists but is owned by no crate
///      (`crates/AGENTS.md`, a data-only package directory) → identity;
///   3. the crate moved → resolve the first segment as a crate directory
///      basename and rejoin the remainder;
///   4. nothing resolves → refuse. A spec naming a crate that no longer exists
///      is a stale entry, and answering it with a path that matches nothing is
///      the silent failure this row exists to kill.
///
/// A trailing `/` is preserved, so allowlist entries used as `contains`
/// fragments (`crates/ironclaw_llm/src/`) resolve to fragments.
#[allow(dead_code)]
pub(crate) fn try_resolve_crate_relative(root: &Path, logical: &str) -> Result<String, String> {
    let prefix = format!("{CRATES_ROOT}/");
    if !logical.starts_with(&prefix) {
        return Ok(logical.to_string());
    }
    let inventory = try_crate_directories(root)?;
    for directory in &inventory {
        if logical == directory || logical.starts_with(&format!("{directory}/")) {
            return Ok(logical.to_string());
        }
    }
    if root.join(logical.trim_end_matches('/')).exists() {
        return Ok(logical.to_string());
    }
    let rest = &logical[prefix.len()..];
    let (name, remainder) = match rest.split_once('/') {
        Some((name, remainder)) => (name, Some(remainder)),
        None => (rest, None),
    };
    let directory = try_crate_directory(root, name).map_err(|error| {
        format!(
            "path {logical:?} names crate {name:?}, which does not resolve against the crate \
             inventory and does not exist on disk — repoint the gate that names it rather \
             than leaving a term that matches nothing ({error})"
        )
    })?;
    Ok(match remainder {
        Some(remainder) => format!("{directory}/{remainder}"),
        None => directory,
    })
}

/// `try_resolve_crate_relative`, panicking on refusal.
#[allow(dead_code)]
pub(crate) fn resolve_crate_relative(root: &Path, logical: &str) -> String {
    try_resolve_crate_relative(root, logical).unwrap_or_else(|error| panic!("{error}"))
}

/// Absolute path for a logical `crates/<crate>/<remainder>` spelling —
/// the drop-in replacement for `root.join("crates/ironclaw_x/…")`.
#[allow(dead_code)]
pub(crate) fn crate_path(root: &Path, logical: &str) -> PathBuf {
    root.join(resolve_crate_relative(root, logical))
}

/// The directory basename of the crate owning `path`, or `""` when no crate
/// under `crates/` owns it.
///
/// The drop-in replacement for "take the first path component under
/// `crates/`". That idiom answers `substrates` once a crate moves into a
/// family directory, and already answers `extensions` for the two crates that
/// sit one level down today — so a gate keyed on it compares an ownership
/// question against a directory name and silently attributes code to the wrong
/// owner. Outermost-wins, exactly like the inventory.
#[allow(dead_code)]
pub(crate) fn owning_crate_name(root: &Path, path: &Path) -> String {
    let Ok(relative) = path.strip_prefix(root) else {
        return String::new();
    };
    let relative = relative.to_string_lossy().replace('\\', "/");
    for directory in crate_directories(root) {
        if relative.starts_with(&format!("{directory}/")) || relative == directory {
            return directory
                .rsplit('/')
                .next()
                .unwrap_or(&directory)
                .to_string();
        }
    }
    String::new()
}

/// Names with more than one defining occurrence — a second same-named
/// definition elsewhere is new debt hiding behind an allowlist entry (§10) —
/// EXCEPT when every occurrence is `#[cfg(...)]`-gated (mutually exclusive
/// compile branches of the same type, the factory durable/no-durable pattern).
/// A mix of gated and ungated occurrences is still flagged.
#[allow(dead_code)]
pub(crate) fn duplicate_definitions(
    found: &BTreeMap<String, Vec<TypeDefOccurrence>>,
) -> Vec<(&str, &Vec<TypeDefOccurrence>)> {
    found
        .iter()
        .filter(|(_, occurrences)| {
            occurrences.len() > 1 && !occurrences.iter().all(|occ| occ.cfg_gated)
        })
        .map(|(name, occurrences)| (name.as_str(), occurrences))
        .collect()
}

/// Walk `dir` recursively, scanning every production `.rs` file for pub-visible
/// type definitions introduced by one of `keywords` (e.g. `"struct "`,
/// `"type "`) whose identifier satisfies `matches`. Records every occurrence
/// (identifier → defining file, once per occurrence). Skips `target/`,
/// `tests/`, `examples/`, and `benches/` trees plus any file named in
/// `skip_files` (the ratchet files themselves, as defense in depth — their
/// fixtures are already excluded by string stripping).
#[allow(dead_code)]
pub(crate) fn collect_type_defs(
    dir: &Path,
    keywords: &[&str],
    matches: &dyn Fn(&str) -> bool,
    skip_files: &[&str],
    out: &mut BTreeMap<String, Vec<TypeDefOccurrence>>,
) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|err| panic!("failed to read dir entry: {err}"));
        let path = entry.path();
        if path.is_dir() {
            let dir_name = path.file_name().and_then(|n| n.to_str());
            if matches!(dir_name, Some("target" | "tests" | "examples" | "benches")) {
                continue;
            }
            collect_type_defs(&path, keywords, matches, skip_files, out);
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && skip_files.contains(&name)
        {
            continue;
        }
        let contents = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
        for (ident, cfg_gated) in scan_type_defs(&contents, keywords, matches) {
            out.entry(ident).or_default().push(TypeDefOccurrence {
                path: path.clone(),
                cfg_gated,
            });
        }
    }
}

/// Extract the identifier from every pub-visible type definition introduced by
/// one of `keywords` — `pub`, `pub(crate)`, `pub(super)`, or `pub(in path)`,
/// with optional `unsafe`/`auto` modifiers (e.g. `pub unsafe trait …`).
/// Comments and string literals are stripped first, so definition-shaped text
/// inside them is not matched. Matches the definition form, not references.
/// Returns every occurrence in source order (no dedup) so same-file duplicate
/// definitions in different modules stay visible to the multiplicity check.
/// The `bool` per occurrence is whether the definition sits under a
/// (single-line) `#[cfg(...)]` attribute in its immediately preceding attribute
/// block — used to exempt mutually exclusive compile branches from the
/// duplicate check.
#[allow(dead_code)]
pub(crate) fn scan_type_defs(
    source: &str,
    keywords: &[&str],
    matches: &dyn Fn(&str) -> bool,
) -> Vec<(String, bool)> {
    let stripped = strip_comments_and_strings(source);
    let mut out = Vec::new();
    // `#[...]` attributes immediately preceding the current line; reset by any
    // other non-blank line. Multi-line attributes (rustfmt splits long
    // `#[cfg(any(...))]` gates) are tracked by square-bracket balance — strings
    // are already stripped, so bracket counting is safe.
    let mut pending_attrs: Vec<String> = Vec::new();
    let mut attr_bracket_depth: usize = 0;
    for line in stripped.lines() {
        let trimmed = line.trim_start();
        if attr_bracket_depth > 0 {
            // Continuation of a multi-line attribute.
            attr_bracket_depth = (attr_bracket_depth + trimmed.matches('[').count())
                .saturating_sub(trimmed.matches(']').count());
            continue;
        }
        if trimmed.starts_with("#[") {
            pending_attrs.push(trimmed.to_string());
            attr_bracket_depth = trimmed
                .matches('[')
                .count()
                .saturating_sub(trimmed.matches(']').count());
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let cfg_gated = pending_attrs.iter().any(|attr| attr.starts_with("#[cfg("));
        pending_attrs.clear();
        let Some(after_pub) = trimmed.strip_prefix("pub") else {
            continue;
        };
        // Optional restricted-visibility qualifier: `(crate)`, `(super)`, `(in path)`.
        let mut rest = match after_pub.trim_start().strip_prefix('(') {
            Some(inner) => match inner.split_once(')') {
                Some((_, tail)) => tail,
                None => continue,
            },
            None => after_pub,
        }
        .trim_start();
        // Optional declaration modifiers before the keyword.
        loop {
            let mut advanced = false;
            for modifier in ["unsafe ", "auto "] {
                if let Some(tail) = rest.strip_prefix(modifier) {
                    rest = tail.trim_start();
                    advanced = true;
                }
            }
            if !advanced {
                break;
            }
        }
        let Some(after_kw) = keywords.iter().find_map(|kw| rest.strip_prefix(kw)) else {
            continue;
        };
        let ident: String = after_kw
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if matches(&ident) {
            out.push((ident, cfg_gated));
        }
    }
    out
}

/// Whether stripped `code` names the crate `krate` as a whole token — i.e.
/// not as a prefix of a longer identifier. `ironclaw_assistant::X` and
/// `use ironclaw_assistant as p;` both match for `ironclaw_assistant`;
/// `ironclaw_product_contracts::X` does not, because the token continues with
/// an identifier character. A plain `contains("{krate}::")` misses the
/// `use … as alias;` and `extern crate … as alias;` forms, through which the
/// dependency would keep compiling while the scan reports it gone.
#[allow(dead_code)]
pub(crate) fn names_crate(code: &str, krate: &str) -> bool {
    let bytes = code.as_bytes();
    let mut search = 0;
    while let Some(pos) = code[search..].find(krate) {
        let at = search + pos;
        search = at + 1;
        let before_ok = at == 0 || {
            let ch = bytes[at - 1];
            !(ch.is_ascii_alphanumeric() || ch == b'_')
        };
        let end = at + krate.len();
        let after_ok = end >= bytes.len() || {
            let ch = bytes[end];
            !(ch.is_ascii_alphanumeric() || ch == b'_')
        };
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

/// Out-of-line module declarations (`mod name;`) in stripped source, each with
/// whether the declaration's immediately preceding attribute run contains
/// `cfg(test)`. Inline modules (`mod name { … }`) are not returned — their
/// contents live in the same file and are the brace-stripper's problem.
///
/// The gate is read by walking **backwards** from the `mod` keyword, so the
/// visibility qualifier that may sit between the attribute run and `mod` has to
/// be stepped over first ([`strip_trailing_visibility`]). Not doing so ended the
/// run at `pub` and reported `#[cfg(test)] pub mod x;` as **ungated** — the
/// fail-open direction, since [`cfg_test_only_files`] would then leave a
/// test-only file classified as production and a test double in a
/// production-named file could satisfy a residue row or an implementor pin.
/// Raised by @serrrfirat on #7000.
#[allow(dead_code)]
pub(crate) fn out_of_line_mod_decls(stripped: &str) -> Vec<(String, bool)> {
    let bytes = stripped.as_bytes();
    let mut decls = Vec::new();
    let mut search = 0;
    while let Some(pos) = stripped[search..].find("mod ") {
        let at = search + pos;
        search = at + "mod ".len();
        if at > 0 {
            let prev = bytes[at - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                continue;
            }
        }
        let rest = &stripped[at + "mod ".len()..];
        let name: String = rest
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        if !rest[name.len()..].trim_start().starts_with(';') {
            continue;
        }
        let before = strip_trailing_visibility(&stripped[..at]);
        let gated = trailing_attribute_run_is_test_only(before);
        decls.push((name, gated));
    }
    decls
}

/// `#[path = "…"] mod name;` declarations in **raw** source (the path lives in
/// a string literal, which the stripper blanks), mapping module name to its
/// explicit file path. The scan tolerates further attributes between the
/// `path` attribute and the `mod` keyword.
#[allow(dead_code)]
pub(crate) fn explicit_mod_paths(raw: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut search = 0;
    while let Some(pos) = raw[search..].find("#[path") {
        let at = search + pos;
        search = at + "#[path".len();
        let rest = &raw[at..];
        let Some(open_quote) = rest.find('"') else {
            continue;
        };
        let after_quote = &rest[open_quote + 1..];
        let Some(close_quote) = after_quote.find('"') else {
            continue;
        };
        let path = &after_quote[..close_quote];
        // Walk forward past whitespace and further attributes to `mod name;`.
        let mut tail = after_quote[close_quote + 1..].trim_start();
        tail = tail.strip_prefix(']').unwrap_or(tail).trim_start();
        while tail.starts_with("#[") {
            let Some(close) = tail.find(']') else { break };
            tail = tail[close + 1..].trim_start();
        }
        for visibility in ["pub(crate) ", "pub(super) ", "pub "] {
            tail = tail.strip_prefix(visibility).unwrap_or(tail).trim_start();
        }
        let Some(after_mod) = tail.strip_prefix("mod ") else {
            continue;
        };
        let name: String = after_mod
            .trim_start()
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_')
            .collect();
        if !name.is_empty() {
            out.insert(name, path.to_string());
        }
    }
    out
}

/// Strip one trailing visibility qualifier — `pub`, `pub(crate)`,
/// `pub(super)`, `pub(in path)` — from the end of `before`, so a backwards
/// attribute-run walk starts at the true beginning of the declaration rather
/// than stopping dead at `pub`. Returns `before` (trimmed) unchanged when the
/// tail is not a visibility qualifier, so nothing else can be consumed by
/// accident: the parenthesised form is only stripped when the group balances
/// *and* is immediately preceded by the whole token `pub`.
fn strip_trailing_visibility(before: &str) -> &str {
    let trimmed = before.trim_end();
    // `pub(crate)` / `pub(super)` / `pub(in a::b)` — step over the group first.
    let head = match trimmed.strip_suffix(')') {
        Some(inner) => {
            let bytes = inner.as_bytes();
            let mut depth = 1usize;
            let mut index = inner.len();
            while index > 0 {
                index -= 1;
                match bytes[index] {
                    b')' => depth += 1,
                    b'(' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if depth != 0 {
                return trimmed;
            }
            inner[..index].trim_end()
        }
        None => trimmed,
    };
    // `pub` must be a whole token: not the tail of an identifier, and not the
    // raw form `r#pub`.
    match head.strip_suffix("pub") {
        Some(rest)
            if rest
                .chars()
                .next_back()
                .is_none_or(|ch| !(ch.is_alphanumeric() || ch == '_' || ch == '#')) =>
        {
            rest
        }
        _ => trimmed,
    }
}

/// Whether the contiguous run of `#[…]` attributes ending at the end of
/// `before` holds a test-only one ([`attribute_line_is_test_only`], strict:
/// `feature = "test-support"` is a real build, and its string value is blanked
/// here anyway). Strings are already stripped, so `#[` inside a literal cannot
/// mislead the backwards walk.
fn trailing_attribute_run_is_test_only(before: &str) -> bool {
    let mut rest = before.trim_end();
    while rest.ends_with(']') {
        let Some(open) = rest.rfind("#[") else {
            return false;
        };
        if attribute_line_is_test_only(&rest[open..], false) {
            return true;
        }
        rest = rest[..open].trim_end();
    }
    false
}

/// Directories pruned from **every** walk at any depth — build output,
/// vendored packages, and git metadata, which hold no first-party Rust source.
const PRUNED_DIRS: &[&str] = &["target", "node_modules", ".git"];

/// Directory names cargo compiles as **separate targets** when they sit beside
/// a `Cargo.toml` (integration tests, examples, benches). Only there are they
/// excluded by directory; a `src/**/tests/` module directory is classified by
/// its declarations like any other file (gate audit 2026-08 §9 residual 1).
const CRATE_TARGET_DIRS: &[&str] = &["tests", "examples", "benches"];

/// Whether the walk skips `dir` outright: a [`PRUNED_DIRS`] name anywhere, or
/// a [`CRATE_TARGET_DIRS`] name whose parent is a package directory.
#[allow(dead_code)]
pub(crate) fn is_pruned_dir(dir: &Path) -> bool {
    let Some(name) = dir.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    PRUNED_DIRS.contains(&name)
        || (CRATE_TARGET_DIRS.contains(&name)
            && dir
                .parent()
                .is_some_and(|parent| parent.join("Cargo.toml").is_file()))
}

/// **The** definition of "a production Rust file under `root`", as absolute
/// paths, sorted: the fatal walk (pruning [`is_pruned_dir`]) minus the
/// [`cfg_test_only_files`] subtraction, composed once. `root` may be a crate's
/// `src`, a crate directory, or a tree of crates (`crates/`): crate-level
/// `tests/` / `examples/` / `benches/` are recognized by their `Cargo.toml`
/// sibling, not by the walk root.
///
/// Neither a file NAME nor a directory name inside `src` makes a file
/// test-only. `tests.rs` / `*_tests.rs` / `src/**/tests/*.rs` are excluded only
/// when [`cfg_test_only_files`] finds them reachable solely through test-only
/// `mod` declarations — until 2026-11-12 any such basename was dropped, so a
/// `pub mod foo_tests;` holding a `TcpListener::bind` was invisible to every
/// gate built on this walk (gate audit 2026-08 §6.2), and until 2026-11-17 a
/// `src/**/tests/` directory was dropped by directory (§9). Same rule as
/// `scripts/check_no_panics.py`'s `is_test_only_path`.
///
/// Ratchets used to each spell this two-step recipe themselves, and they had
/// already drifted: `reborn_extension_host_port_inversion.rs` skipped
/// `node_modules` and `reborn_extension_manager_split.rs` did not. That is
/// exactly the failure mode a shrink-only list cannot survive — two gates
/// disagreeing about what production code *is* means one of them is measuring
/// a different tree than the row it enforces was written against. Raised by
/// @serrrfirat on #7003 (and independently on #7004). Since 2026-11-17 every
/// walker in this directory that filters test code goes through it (§9).
///
/// I/O errors are **fatal**, not skipped: a scan that silently swallows a
/// failed `read_dir` walks a smaller tree and passes a shrink-only list
/// vacuously.
#[allow(dead_code)]
pub(crate) fn production_rust_files(root: &Path) -> Vec<PathBuf> {
    let test_only = cfg_test_only_files(root);
    let mut all = Vec::new();
    all_rust_files(root, &mut all);
    let mut out: Vec<PathBuf> = all
        .into_iter()
        .filter(|path| !test_only.contains(path))
        .collect();
    out.sort();
    out
}

/// Whether `path` lies in the feature-gated `src/test_support` root of the
/// package that owns it — [`is_test_support_root`] with the package's `src`
/// found from the path itself (the nearest ancestor `src` beside a
/// `Cargo.toml`), so walks rooted above a crate apply the same exclusion as
/// walks rooted at its `src`.
#[allow(dead_code)]
pub(crate) fn in_test_support_root(path: &Path) -> bool {
    path.ancestors()
        .skip(1)
        .find(|ancestor| {
            ancestor.file_name().is_some_and(|name| name == "src")
                && ancestor
                    .parent()
                    .is_some_and(|parent| parent.join("Cargo.toml").is_file())
        })
        .is_some_and(|src| is_test_support_root(src, path))
}

/// Every `.rs` file under `src` (as walked by [`all_rust_files`]) that is
/// test-only, by [`module_graph`]: every edge that includes it is test-gated,
/// or sits in a file that is itself test-only (transitively), and nothing the
/// resolver could not prove touches it. A file with no edge, or with any one
/// ungated edge, is production — whatever its name. The name conventions (`tests.rs`, `*_tests.rs`) are NOT a rule:
/// e.g. a `#[cfg(test)] mod e2e_tests;` whose `e2e_tests.rs` declares
/// `mod fixture;` makes `fixture.rs` test-only through the chain, while a
/// `pub mod foo_tests;` keeps `foo_tests.rs` production. A scan that counts a
/// test-only file can keep a residue row or an implementor pin "satisfied"
/// with a test double; one that drops a production file hides real code. I/O
/// errors are fatal, same as the walkers.
#[allow(dead_code)]
pub(crate) fn cfg_test_only_files(src: &Path) -> BTreeSet<PathBuf> {
    module_graph(src).test_only
}

/// Every edge under `src` the resolver could not resolve, as `(file holding
/// it, what and why)`. Each one makes its whole containing module production
/// ([`module_graph`]); the list is how a reviewer sees what was given up on.
#[allow(dead_code)]
pub(crate) fn unresolved_module_edges(src: &Path) -> Vec<(PathBuf, String)> {
    module_graph(src).unresolved
}

/// The resolved module graph of a tree: which files are test-only, and which
/// edges could not be resolved.
struct ModuleGraph {
    test_only: BTreeSet<PathBuf>,
    unresolved: Vec<(PathBuf, String)>,
}

/// One edge [`module_items`] reads from a file.
#[derive(Debug)]
enum ModuleItem {
    /// An out-of-line `mod name;`: test-only-`gated` (its own cfg or an
    /// enclosing gated block, strict about `test-support`), the module
    /// directory it resolves under, and its plain `#[path]` if any.
    Declaration {
        name: String,
        gated: bool,
        dir: PathBuf,
        path: Option<String>,
    },
    /// A file compiled (or read) in for every build: a literal `include!` /
    /// `include_str!` / `include_bytes!` of a `.rs` file, or the `path` of a
    /// `#[cfg_attr(…, path = "…")]`. Candidates in resolution order.
    Inclusion {
        candidates: Vec<PathBuf>,
        what: String,
    },
    /// An edge that cannot be resolved; its module `dir` (and the file)
    /// become production.
    Unresolvable { dir: PathBuf, why: String },
}

/// The test-only set and the unresolved edges of every `.rs` file under `src`
/// (as walked by [`all_rust_files`]). A file is test-only when every edge that
/// includes it is gated by a test-only cfg (`#[cfg(test)]`,
/// `#[cfg(all(test, …))]`, an enclosing gated inline module — strict about
/// `feature = "test-support"`) or sits in a file that is itself test-only
/// (transitively). A file with no edge, or with any one ungated edge, is
/// production — whatever its name.
///
/// **Fail closed on whatever cannot be proven test-only** (GPT review of the
/// gate audit 2026-08 §9 sweep). [`module_items`] reads *every* module item
/// on a line (`mod first; mod hidden;`), an `include!` / `include_str!` of a
/// `.rs` file and a `#[path]` inside `cfg_attr` are production edges whatever
/// the surrounding cfg, and an edge it cannot resolve (`include!(concat!(…))`,
/// a `mod x;` with no file) makes its whole containing module production,
/// recorded in `unresolved`. Python twin: `check_no_panics.module_items`.
fn module_graph(src: &Path) -> ModuleGraph {
    let mut all = Vec::new();
    all_rust_files(src, &mut all);

    // target -> every (including file, gated) edge; poisoned module dirs.
    let mut included_by: BTreeMap<PathBuf, Vec<(PathBuf, bool)>> = BTreeMap::new();
    let mut poisoned: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut unresolved: Vec<(PathBuf, String)> = Vec::new();
    for file in &all {
        let source = std::fs::read_to_string(file)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", file.display()));
        for item in module_items(file, &source) {
            match item {
                ModuleItem::Declaration {
                    name,
                    gated,
                    dir,
                    path,
                } => match resolve_declaration(file, &name, &dir, path.as_deref()) {
                    Some(target) => included_by
                        .entry(target)
                        .or_default()
                        .push((file.clone(), gated)),
                    None => {
                        unresolved
                            .push((file.clone(), format!("`mod {name};` resolves to no file")));
                        poisoned.push((file.clone(), dir));
                    }
                },
                ModuleItem::Inclusion { candidates, what } => {
                    match candidates.into_iter().find(|candidate| candidate.is_file()) {
                        Some(target) => included_by
                            .entry(target)
                            .or_default()
                            .push((file.clone(), false)),
                        None => {
                            unresolved.push((file.clone(), format!("{what} resolves to no file")));
                            poisoned.push((file.clone(), root_module_directory(file)));
                        }
                    }
                }
                ModuleItem::Unresolvable { dir, why } => {
                    unresolved.push((file.clone(), why));
                    poisoned.push((file.clone(), dir));
                }
            }
        }
    }
    let is_poisoned = |path: &Path| {
        poisoned
            .iter()
            .any(|(file, dir)| path == file.as_path() || path.starts_with(dir))
    };

    // Fixpoint: a file is test-only once every edge to it is gated or sits in
    // a test-only file. Monotone, so it terminates.
    let mut test_only: BTreeSet<PathBuf> = BTreeSet::new();
    loop {
        let mut grew = false;
        for (target, edges) in &included_by {
            if test_only.contains(target) || is_poisoned(target) {
                continue;
            }
            if edges
                .iter()
                .all(|(including, gated)| *gated || test_only.contains(including))
            {
                test_only.insert(target.clone());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    unresolved.sort();
    ModuleGraph {
        test_only,
        unresolved,
    }
}

/// The file an out-of-line `mod name;` names: its `#[path]` (relative to the
/// declaring file's directory, its module directory, or the enclosing inline
/// module's directory — whichever exists), else `<dir>/name.rs` or
/// `<dir>/name/mod.rs` under the line's module directory.
fn resolve_declaration(
    declaring: &Path,
    name: &str,
    dir: &Path,
    path: Option<&str>,
) -> Option<PathBuf> {
    let candidates = match path {
        Some(explicit) => path_attribute_candidates(declaring, dir, explicit),
        None => vec![
            dir.join(format!("{name}.rs")),
            dir.join(name).join("mod.rs"),
        ],
    };
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn path_attribute_candidates(declaring: &Path, dir: &Path, explicit: &str) -> Vec<PathBuf> {
    let parent = declaring
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut candidates = vec![parent.join(explicit)];
    if let Some(stem) = declaring.file_stem() {
        candidates.push(parent.join(stem).join(explicit));
    }
    candidates.push(dir.join(explicit));
    candidates
}

/// The `"…"` value of a `key = "…"` pair inside one raw attribute, `key` a
/// whole word (`path`, not `xpath`).
fn attribute_string_value(attribute: &str, key: &str) -> Option<String> {
    let mut search = 0;
    while let Some(found) = attribute[search..].find(key) {
        let at = search + found;
        search = at + key.len();
        let before_ok = attribute[..at]
            .chars()
            .next_back()
            .is_none_or(|ch| !(ch.is_alphanumeric() || ch == '_'));
        let rest = attribute[at + key.len()..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        if !before_ok {
            continue;
        }
        let rest = rest.trim_start().strip_prefix('"')?;
        return rest.find('"').map(|end| rest[..end].to_string());
    }
    None
}

/// Whether one raw attribute mentions `key` as a whole word.
fn attribute_mentions(attribute: &str, key: &str) -> bool {
    attribute.match_indices(key).any(|(at, _)| {
        let before = attribute[..at].chars().next_back();
        let after = attribute[at + key.len()..].chars().next();
        before.is_none_or(|ch| !(ch.is_alphanumeric() || ch == '_'))
            && after.is_none_or(|ch| !(ch.is_alphanumeric() || ch == '_'))
    })
}

/// Every module edge in one file, by one token walk over the source with
/// comments and string literals blanked IN PLACE
/// ([`blank_comments_and_strings`], so attribute text can be read back from the
/// raw source at the same offsets). Every `mod` item counts, however many
/// share a line. The walk keeps a block stack of `(test-only, module dir)`;
/// attributes seen since the last `;` / `{` / `}` gate the next item (or the
/// block it opens). Same algorithm as `scripts/check_no_panics.py::module_items`.
fn module_items(file: &Path, source: &str) -> Vec<ModuleItem> {
    let code = blank_comments_and_strings(source);
    let bytes = code.as_bytes();
    let root = root_module_directory(file);
    let file_dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut items = Vec::new();
    let mut stack: Vec<(bool, PathBuf)> = Vec::new();
    let mut pending_test = false;
    let mut pending_path: Option<String> = None;
    let mut pending_cfg_attr_paths: Vec<Option<String>> = Vec::new();
    let mut pending_inline: Option<String> = None;
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let skip_ws = |mut j: usize| {
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        j
    };
    let read_ident = |j: usize| {
        let mut k = j;
        while k < bytes.len() && is_ident(bytes[k]) {
            k += 1;
        }
        k
    };
    let mut i = 0;
    while i < bytes.len() {
        let (top_test, top_dir) = stack
            .last()
            .cloned()
            .unwrap_or_else(|| (false, root.clone()));
        let byte = bytes[i];
        if byte == b'#' {
            let mut j = skip_ws(i + 1);
            let inner = bytes.get(j) == Some(&b'!');
            if inner {
                j = skip_ws(j + 1);
            }
            if bytes.get(j) == Some(&b'[') {
                let Some(end) = matching_close(bytes, j, b'[', b']') else {
                    break;
                };
                if !inner {
                    let attribute = &source[i..=end];
                    pending_test |= attribute_line_is_test_only(attribute, false);
                    let body = attribute[1..]
                        .trim_start()
                        .trim_start_matches('[')
                        .trim_start();
                    if body.starts_with("cfg_attr") {
                        if attribute_mentions(attribute, "path") {
                            pending_cfg_attr_paths.push(attribute_string_value(attribute, "path"));
                        }
                    } else if body.starts_with("path") {
                        pending_path = attribute_string_value(attribute, "path");
                    }
                }
                i = end + 1;
                continue;
            }
            i += 1;
            continue;
        }
        match byte {
            b'{' => {
                let inline = pending_inline.take();
                let has_path = !pending_cfg_attr_paths.is_empty() || pending_path.is_some();
                let dir = match inline {
                    Some(name) => {
                        let dir = top_dir.join(&name);
                        if has_path {
                            // `#[path]` re-roots the inline module's children;
                            // not modelled, so they are not provable.
                            items.push(ModuleItem::Unresolvable {
                                dir: dir.clone(),
                                why: format!("`#[path]` on inline `mod {name} {{ … }}`"),
                            });
                        }
                        dir
                    }
                    None => top_dir,
                };
                stack.push((pending_test || top_test, dir));
                pending_test = false;
                pending_path = None;
                pending_cfg_attr_paths.clear();
                i += 1;
                continue;
            }
            b'}' | b';' => {
                if byte == b'}' {
                    stack.pop();
                }
                pending_test = false;
                pending_path = None;
                pending_cfg_attr_paths.clear();
                pending_inline = None;
                i += 1;
                continue;
            }
            _ => {}
        }
        if !(byte.is_ascii_alphabetic() || byte == b'_') || (i > 0 && is_ident(bytes[i - 1])) {
            i += 1;
            continue;
        }
        let end = read_ident(i);
        let ident = &code[i..end];
        // `r#mod` is an identifier, not the keyword.
        if ident == "r" && bytes.get(end) == Some(&b'#') {
            i = read_ident(end + 1);
            continue;
        }
        if ident == "mod" {
            let name_start = skip_ws(end);
            let name_end = read_ident(name_start);
            let after = skip_ws(name_end);
            if name_end > name_start {
                let name = code[name_start..name_end].to_string();
                match bytes.get(after) {
                    Some(b';') => {
                        for value in pending_cfg_attr_paths.drain(..) {
                            items.push(match value {
                                Some(value) => ModuleItem::Inclusion {
                                    candidates: path_attribute_candidates(file, &top_dir, &value),
                                    what: format!(
                                        "`#[cfg_attr(…, path = \"{value}\")] mod {name};`"
                                    ),
                                },
                                None => ModuleItem::Unresolvable {
                                    dir: top_dir.clone(),
                                    why: format!(
                                        "`cfg_attr` `path` on `mod {name};` has no literal value"
                                    ),
                                },
                            });
                        }
                        items.push(ModuleItem::Declaration {
                            name,
                            gated: pending_test || top_test,
                            dir: top_dir,
                            path: pending_path.take(),
                        });
                        pending_test = false;
                        pending_inline = None;
                        i = after + 1;
                        continue;
                    }
                    Some(b'{') => {
                        pending_inline = Some(name);
                        i = after;
                        continue;
                    }
                    _ => {}
                }
            }
            i = end;
            continue;
        }
        if matches!(ident, "include" | "include_str" | "include_bytes") {
            let bang = skip_ws(end);
            if bytes.get(bang) == Some(&b'!') {
                let open = skip_ws(bang + 1);
                let close = match bytes.get(open) {
                    Some(b'(') => matching_close(bytes, open, b'(', b')'),
                    Some(b'[') => matching_close(bytes, open, b'[', b']'),
                    Some(b'{') => matching_close(bytes, open, b'{', b'}'),
                    _ => None,
                };
                if let Some(close) = close {
                    if let Some(item) =
                        inclusion_item(ident, source[open + 1..close].trim(), &file_dir, &top_dir)
                    {
                        items.push(item);
                    }
                    i = close + 1;
                    continue;
                }
            }
        }
        i = end;
    }
    items
}

/// The edge one `include!`-family call makes, from its raw argument text: a
/// literal `.rs` path is an [`ModuleItem::Inclusion`] (resolved against the
/// file's directory, as rustc does); a non-literal `include!` — or a
/// non-literal `include_str!` / `include_bytes!` whose last literal piece does
/// not end in a non-`.rs` extension — is [`ModuleItem::Unresolvable`]; a
/// non-Rust file is no edge.
fn inclusion_item(
    macro_name: &str,
    arguments: &str,
    file_dir: &Path,
    dir: &Path,
) -> Option<ModuleItem> {
    let literal = arguments
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .filter(|inner| !inner.contains('"'));
    if let Some(path) = literal {
        return (macro_name == "include" || path.ends_with(".rs")).then(|| ModuleItem::Inclusion {
            candidates: vec![file_dir.join(path)],
            what: format!("`{macro_name}!(\"{path}\")`"),
        });
    }
    let last_literal = arguments
        .rsplit('"')
        .nth(1)
        .filter(|_| arguments.matches('"').count() >= 2);
    // The text after the last `.` of the last literal piece, when it is a
    // file extension (`".md"` alone is one; `"../dir/"` names none).
    let names_other_file = last_literal
        .and_then(|piece| piece.rsplit_once('.'))
        .is_some_and(|(_, extension)| {
            !extension.is_empty() && !extension.contains('/') && extension != "rs"
        });
    if macro_name != "include" && names_other_file {
        return None;
    }
    Some(ModuleItem::Unresolvable {
        dir: dir.to_path_buf(),
        why: format!(
            "`{macro_name}!({})` is not a literal path",
            arguments.split_whitespace().collect::<Vec<_>>().join(" ")
        ),
    })
}

/// The index of the `close` byte matching the `open` byte at `start`, on code
/// whose comments and strings are already blanked.
fn matching_close(bytes: &[u8], start: usize, open: u8, close: u8) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, byte) in bytes[start..].iter().enumerate() {
        if *byte == open {
            depth += 1;
        } else if *byte == close {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return Some(start + offset);
            }
        }
    }
    None
}

/// The directory a file's top-level out-of-line `mod x;` resolves under:
/// its own directory for `lib.rs` / `main.rs` / `mod.rs`, else `<dir>/<stem>`.
fn root_module_directory(file: &Path) -> PathBuf {
    let parent = file.parent().map(Path::to_path_buf).unwrap_or_default();
    match file.file_name().and_then(|name| name.to_str()) {
        Some("lib.rs" | "main.rs" | "mod.rs") => parent,
        _ => match file.file_stem() {
            Some(stem) => parent.join(stem),
            None => parent,
        },
    }
}

/// Every `.rs` file under `dir`, with no test-*shape* exclusions (the caller
/// decides). Prunes only what [`is_pruned_dir`] names — build output, vendored
/// packages, and crate-level cargo targets. I/O errors are fatal.
fn all_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("read_dir {}: {error}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| {
            panic!("cannot read an entry under {}: {error}", dir.display())
        });
        let path = entry.path();
        if path.is_dir() {
            if is_pruned_dir(&path) {
                continue;
            }
            all_rust_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Remove `#[cfg(test)]`-gated items by brace matching. Only the *production*
/// edge is an architectural fact: a test double may implement a product trait
/// through the crate's dev-dependency without the shipped artifact depending on
/// product.
///
/// **Callers must strip comments and string literals first.** This walk finds
/// the block by counting raw `{`/`}` bytes, so a brace inside a doc comment or a
/// string literal in a gated block desynchronizes the depth and either leaks a
/// test-only `impl` into the production set or swallows the production code that
/// follows. Every caller composes it as
/// `strip_cfg_test_blocks(&strip_comments_and_strings(source))`, and
/// `reborn_ratchet_support_scanners.rs` pins that ordering hazard with a
/// fixture. For a caller that deliberately scans **raw** source, use
/// [`strip_line_anchored_cfg_test_items`] instead.
///
/// `#[cfg(feature = "test-support")]` items are deliberately **not** stripped:
/// that feature compiles into a real build (CI's `--all-features` lanes enable
/// it), so an `impl` behind it is a genuine normal-dependency edge. Only
/// `#[cfg(test)]` is invisible to a shipped artifact.
///
/// The `;`-before-`{` guard is what makes a bodiless gated item
/// (`#[cfg(test)] mod tests;`, `#[cfg(test)] use …;`) end at its own semicolon
/// instead of brace-balancing from the *next* item's block and eating it.
/// `reborn_registration_pipeline_boundary.rs`'s copy shipped without that guard;
/// adopting this body restores it, which can only ever KEEP more production code
/// (the fail-closed direction).
#[allow(dead_code)]
pub(crate) fn strip_cfg_test_blocks(source: &str) -> String {
    const MARKER: &str = "#[cfg(test)]";
    let mut out = String::with_capacity(source.len());
    let mut rest = source;
    while let Some(at) = rest.find(MARKER) {
        out.push_str(&rest[..at]);
        let after = &rest[at + MARKER.len()..];
        let Some(open) = after.find('{') else {
            // A `#[cfg(test)] use …;` line: drop through the statement end.
            match after.find(';') {
                Some(semi) => {
                    rest = &after[semi + 1..];
                    continue;
                }
                None => return out,
            }
        };
        // An attribute followed by a `;` before any `{` is a gated statement.
        if let Some(semi) = after.find(';')
            && semi < open
        {
            rest = &after[semi + 1..];
            continue;
        }
        let bytes = after.as_bytes();
        let mut depth = 0usize;
        let mut idx = open;
        while idx < bytes.len() {
            match bytes[idx] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            idx += 1;
        }
        if idx >= bytes.len() {
            return out;
        }
        rest = &after[idx + 1..];
    }
    out.push_str(rest);
    out
}

/// Remove `#[cfg(test)]`-gated items from **raw** source, anchoring on the
/// attribute being the first token of its line.
///
/// The counterpart to [`strip_cfg_test_blocks`] for the two gates that must read
/// comments and string literals rather than blank them: `reborn_extension_
/// specificity` (a vendor name in a comment IS a specificity violation) and
/// `reborn_manifest_reparse_gate` (which filters comment lines itself, after
/// this call). Anchoring at line start is what keeps a `#[cfg(test)]` written
/// inside a doc comment or a string literal from stripping production code —
/// the byte-scanning family has no way to tell those apart without a
/// pre-stripped input.
///
/// Skips the attribute run, then the annotated item: a braced item ends when the
/// brace depth returns to zero, a bodiless one (`mod tests;`) at its semicolon.
#[allow(dead_code)]
pub(crate) fn strip_line_anchored_cfg_test_items(source: &str) -> String {
    // Anchor and brace depth read the comment/string-STRIPPED twin of each
    // line (line structure preserved), while the kept output is the raw line.
    // Counting raw braces let a test item holding `"{{{{"` or `'{'` never
    // close, stripping every production line after it; anchoring on raw text
    // let a multiline string line starting `#[cfg(test)]` strip real code.
    let stripped = strip_comments_and_strings(source);
    let mut code_lines = stripped.lines();
    let mut kept = String::with_capacity(source.len());
    let mut lines = source.lines();
    while let Some(line) = lines.next() {
        let code = code_lines.next().unwrap_or("");
        if !code.trim_start().starts_with("#[cfg(test)]") {
            kept.push_str(line);
            kept.push('\n');
            continue;
        }
        // Skip attribute lines, then the annotated item.
        let mut depth: i64 = 0;
        let mut opened = false;
        for _skipped in lines.by_ref() {
            let code = code_lines.next().unwrap_or("");
            let trimmed = code.trim_start();
            if !opened && trimmed.starts_with("#[") {
                continue;
            }
            depth += code.matches('{').count() as i64;
            depth -= code.matches('}').count() as i64;
            if !opened {
                if code.contains('{') {
                    opened = true;
                } else if trimmed.ends_with(';') {
                    // `mod tests;` — single-line item, nothing else to skip.
                    break;
                }
            }
            if opened && depth <= 0 {
                break;
            }
        }
    }
    kept
}

/// Whether `ident` is a plain Rust identifier (ASCII, no path separators, no
/// generics). Used to reject the fragments a substring-oriented scan produces
/// when it reads a shape it does not understand.
#[allow(dead_code)]
pub(crate) fn is_rust_identifier(ident: &str) -> bool {
    let mut chars = ident.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Byte index of the `>` that closes the `<` at index 0, counting nesting.
/// `None` when the brackets never balance (a truncated slice), which the caller
/// treats as "not an impl header I can read" rather than guessing.
#[allow(dead_code)]
pub(crate) fn balanced_angle_close(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    for (index, ch) in text.char_indices() {
        match ch {
            '<' => depth += 1,
            // A `->` inside a bound (`impl<F: Fn(&str) -> bool>`) is a return
            // arrow, not a closing bracket. `-` never opens one, so a `>`
            // preceded by `-` is skipped.
            '>' if index > 0 && bytes[index - 1] == b'-' => {}
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Trait names appearing as the *implemented* trait of an `impl … for …` item,
/// with any leading path qualifier and generic arguments dropped
/// (`impl ironclaw_assistant::Foo<T> for Bar` → `Foo`). Inherent impls
/// (`impl Bar {`) have no `for` and never match.
///
/// Composes the two strippers in the only sound order — comments and strings
/// first, then `#[cfg(test)]` — so callers cannot get it wrong.
#[allow(dead_code)]
pub(crate) fn implemented_trait_names(source: &str) -> BTreeSet<String> {
    let cleaned = strip_cfg_test_blocks(&strip_comments_and_strings(source));
    let mut names = BTreeSet::new();
    for segment in cleaned.split("impl").skip(1) {
        let Some(head) = segment.split_once(" for ") else {
            continue;
        };
        let mut candidate = head.0.trim();
        // Drop an `<'a, T>` generic-parameter list that binds the impl itself.
        // The close must be found by *balancing*, not by the first `>`: a bound
        // may itself be generic (`impl<T: Iterator<Item = X>> Port for Host<T>`),
        // and taking the first `>` would leave `> Port` — not an identifier, so
        // the impl would be skipped and the gate would enforce nothing for it.
        if candidate.starts_with('<') {
            let Some(close) = balanced_angle_close(candidate) else {
                continue;
            };
            candidate = candidate[close + 1..].trim();
        }
        // Drop generic arguments on the trait itself, then the path qualifier.
        let candidate = candidate.split('<').next().unwrap_or(candidate).trim();
        let Some(last) = candidate.rsplit("::").next() else {
            continue;
        };
        let last = last.trim();
        if is_rust_identifier(last) {
            names.insert(last.to_string());
        }
    }
    names
}

/// Replace line comments, block comments (nested), plain/raw string literal
/// contents, and char literals with blanks, preserving newlines so the
/// line-based matcher keeps operating on real code lines only. A minimal
/// lexer — good enough for rustfmt'd source; it intentionally errs on the side
/// of stripping (a mis-lex would surface loudly as a frozen-set mismatch).
pub(crate) fn strip_comments_and_strings(source: &str) -> String {
    lex_comments_and_strings(source, false)
}

/// [`strip_comments_and_strings`] with every stripped character replaced IN
/// PLACE by spaces of the same UTF-8 length (newlines kept), so byte offsets
/// in the result index the same text in `source` — what a token walk needs to
/// read an attribute's raw text (its blanked string values) back.
#[allow(dead_code)]
pub(crate) fn blank_comments_and_strings(source: &str) -> String {
    lex_comments_and_strings(source, true)
}

/// The one lexer behind both: `blank` keeps positions, otherwise stripped text
/// is dropped (newlines kept where the line structure needs them).
fn lex_comments_and_strings(source: &str, blank: bool) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::with_capacity(source.len());
    // A character the lexer removes: blanked in place, or dropped keeping only
    // a newline.
    let drop = |out: &mut String, c: char| {
        if c == '\n' {
            out.push('\n');
        } else if blank {
            for _ in 0..c.len_utf8() {
                out.push(' ');
            }
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // Line comment.
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                drop(&mut out, chars[i]);
                i += 1;
            }
            continue;
        }
        // Block comment (Rust block comments nest).
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            let mut depth = 1usize;
            drop(&mut out, '/');
            drop(&mut out, '*');
            i += 2;
            while i < chars.len() && depth > 0 {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    drop(&mut out, '/');
                    drop(&mut out, '*');
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    drop(&mut out, '*');
                    drop(&mut out, '/');
                    i += 2;
                } else {
                    drop(&mut out, chars[i]);
                    i += 1;
                }
            }
            continue;
        }
        // Raw string literal: r"..." / r#"..."# (optionally b/c-prefixed).
        if c == 'r' || ((c == 'b' || c == 'c') && chars.get(i + 1) == Some(&'r')) {
            let hash_start = if c == 'r' { i + 1 } else { i + 2 };
            let mut j = hash_start;
            while chars.get(j) == Some(&'#') {
                j += 1;
            }
            if chars.get(j) == Some(&'"') {
                let hashes = j - hash_start;
                for &prefix in &chars[i..=j] {
                    drop(&mut out, prefix);
                }
                let mut k = j + 1;
                while k < chars.len() {
                    if chars[k] == '"' && (0..hashes).all(|h| chars.get(k + 1 + h) == Some(&'#')) {
                        for &closing in &chars[k..k + 1 + hashes] {
                            drop(&mut out, closing);
                        }
                        k += 1 + hashes;
                        break;
                    }
                    drop(&mut out, chars[k]);
                    k += 1;
                }
                i = k;
                continue;
            }
        }
        // Plain string literal (handles escapes).
        if c == '"' {
            drop(&mut out, '"');
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' {
                    // A `\<newline>` continuation still ends a source line:
                    // dropping it undercounts lines and shifts every later
                    // offender's reported line number.
                    drop(&mut out, '\\');
                    if let Some(&next) = chars.get(i + 1) {
                        drop(&mut out, next);
                    }
                    i += 2;
                    continue;
                }
                if chars[i] == '"' {
                    drop(&mut out, '"');
                    i += 1;
                    break;
                }
                drop(&mut out, chars[i]);
                i += 1;
            }
            continue;
        }
        // Char literal vs lifetime: only consume when it closes as a literal.
        // (Stripped without its newlines either way; blanked in place.)
        if c == '\'' {
            if chars.get(i + 1) == Some(&'\\') {
                let mut k = i + 2;
                while k < chars.len() && chars[k] != '\'' {
                    k += 1;
                }
                if blank {
                    for &literal in &chars[i..(k + 1).min(chars.len())] {
                        drop(&mut out, literal);
                    }
                }
                i = k + 1;
                continue;
            }
            if chars.get(i + 2) == Some(&'\'') {
                if blank {
                    for &literal in &chars[i..i + 3] {
                        drop(&mut out, literal);
                    }
                }
                i += 3;
                continue;
            }
            // A lifetime (`'a`) — emit and move on.
            out.push(c);
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Line-level test context — the ONE counting rule, mirrored from
// `scripts/check_no_panics.py` (`CfgParser`, `cfg_possible_values`,
// `attribute_is_test_only`, `line_test_contexts_from_code`), which the shell
// LOC gate (`scripts/ci/lib/production_loc.py`) and the panic gate use. Keep the
// two in step: a change here without the Python twin (or vice versa) is two
// gates disagreeing about what production code is.
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum CfgPredicate {
    Test,
    TestSupport,
    Unknown,
    Not(Box<CfgPredicate>),
    All(Vec<CfgPredicate>),
    Any(Vec<CfgPredicate>),
}

#[allow(dead_code)]
fn cfg_tokens(source: &str) -> Option<Vec<String>> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            tokens.push(chars[start..i].iter().collect());
        } else if c == '"' {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            if i >= chars.len() {
                return None;
            }
            i += 1;
            tokens.push(chars[start..i].iter().collect());
        } else if matches!(c, '(' | ')' | ',' | '=') {
            tokens.push(c.to_string());
            i += 1;
        } else {
            return None;
        }
    }
    Some(tokens)
}

#[allow(dead_code)]
struct CfgParser {
    tokens: Vec<String>,
    position: usize,
}

#[allow(dead_code)]
impl CfgParser {
    fn peek(&self, token: &str) -> bool {
        self.tokens.get(self.position).is_some_and(|t| t == token)
    }

    fn consume(&mut self, token: &str) -> bool {
        let found = self.peek(token);
        if found {
            self.position += 1;
        }
        found
    }

    fn expression(&mut self) -> Option<CfgPredicate> {
        let name = self.tokens.get(self.position)?.clone();
        if !is_rust_identifier(&name) {
            return None;
        }
        self.position += 1;
        if self.consume("=") {
            // A value blanked by the comment/string stripper leaves `key =`
            // with nothing after it: an unknown predicate, never `test`.
            let value = self
                .tokens
                .get(self.position)
                .filter(|t| t.starts_with('"') || is_rust_identifier(t))
                .cloned();
            if value.is_some() {
                self.position += 1;
            }
            return Some(
                if name == "feature" && value.as_deref() == Some("\"test-support\"") {
                    CfgPredicate::TestSupport
                } else {
                    CfgPredicate::Unknown
                },
            );
        }
        if !self.consume("(") {
            return Some(if name == "test" {
                CfgPredicate::Test
            } else {
                CfgPredicate::Unknown
            });
        }
        let mut arguments = Vec::new();
        if !self.peek(")") {
            loop {
                arguments.push(self.expression()?);
                if !self.consume(",") {
                    break;
                }
            }
        }
        if !self.consume(")") {
            return None;
        }
        match name.as_str() {
            "not" if arguments.len() == 1 => {
                arguments.pop().map(|a| CfgPredicate::Not(Box::new(a)))
            }
            "not" => None,
            "all" => Some(CfgPredicate::All(arguments)),
            "any" => Some(CfgPredicate::Any(arguments)),
            _ => Some(CfgPredicate::Unknown),
        }
    }
}

/// `(can be true, can be false)` of a cfg predicate in a production build.
#[allow(dead_code)]
fn cfg_possible(predicate: &CfgPredicate, test_support_is_test: bool) -> (bool, bool) {
    match predicate {
        CfgPredicate::Test => (false, true),
        CfgPredicate::TestSupport if test_support_is_test => (false, true),
        CfgPredicate::TestSupport | CfgPredicate::Unknown => (true, true),
        CfgPredicate::Not(inner) => {
            let (t, f) = cfg_possible(inner, test_support_is_test);
            (f, t)
        }
        CfgPredicate::All(arguments) => {
            let values: Vec<_> = arguments
                .iter()
                .map(|a| cfg_possible(a, test_support_is_test))
                .collect();
            (values.iter().all(|v| v.0), values.iter().any(|v| v.1))
        }
        CfgPredicate::Any(arguments) => {
            let values: Vec<_> = arguments
                .iter()
                .map(|a| cfg_possible(a, test_support_is_test))
                .collect();
            (values.iter().any(|v| v.0), values.iter().all(|v| v.1))
        }
    }
}

/// Whether a cfg predicate (the text inside `cfg(...)`) can never hold in a
/// production build: `test`, `all(test, …)`, `not(not(test))`, and — when
/// `test_support_is_test` — `feature = "test-support"` / `any(test, …)` of it.
/// An unparseable predicate is not test-only (the fail-closed direction).
#[allow(dead_code)]
pub(crate) fn cfg_predicate_is_test_only(predicate: &str, test_support_is_test: bool) -> bool {
    let Some(tokens) = cfg_tokens(predicate) else {
        return false;
    };
    let mut parser = CfgParser {
        tokens,
        position: 0,
    };
    let Some(expression) = parser.expression() else {
        return false;
    };
    parser.position == parser.tokens.len() && !cfg_possible(&expression, test_support_is_test).0
}

/// Whether one source line is exactly a test-only attribute: `#[test]`,
/// `#[tokio::test…]`, `#[rstest…]`, `#[test_case…]`, or a whole-line
/// `#[cfg(<predicate>)]` whose predicate [`cfg_predicate_is_test_only`].
#[allow(dead_code)]
pub(crate) fn attribute_line_is_test_only(line: &str, test_support_is_test: bool) -> bool {
    let trimmed = line.trim();
    let Some(rest) = trimmed
        .strip_prefix('#')
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix('['))
        .map(str::trim_start)
    else {
        return false;
    };
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == ':')
        .collect();
    let after = rest[name.len()..].trim_start();
    match name.as_str() {
        "test" => return after.starts_with(']'),
        "tokio::test" | "rstest" | "test_case" => {
            return after.starts_with(']') || (after.starts_with('(') && after.contains(']'));
        }
        "cfg" => {}
        _ => return false,
    }
    let Some(predicate) = after
        .strip_prefix('(')
        .and_then(|inner| inner.trim_end().strip_suffix(']'))
        .and_then(|inner| inner.trim_end().strip_suffix(')'))
    else {
        return false;
    };
    cfg_predicate_is_test_only(predicate, test_support_is_test)
}

/// Whether a (comment/string-stripped) code line opens an item the tracker
/// follows: `fn mod struct enum trait union impl const static`, after an
/// optional visibility and `async`/`unsafe`/`const` qualifiers.
#[allow(dead_code)]
fn line_opens_item(code: &str) -> bool {
    let mut rest = code.trim_start();
    if let Some(after) = rest.strip_prefix("pub") {
        let after_trim = after.trim_start();
        if let Some(group) = after_trim.strip_prefix('(') {
            match group.find(')') {
                Some(close) => rest = group[close + 1..].trim_start(),
                None => return false,
            }
        } else if after.starts_with(char::is_whitespace) {
            rest = after_trim;
        }
    } else if let Some(after) = rest.strip_prefix("crate")
        && after.starts_with(char::is_whitespace)
    {
        rest = after.trim_start();
    }
    let word = |text: &str| -> String {
        text.chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect()
    };
    loop {
        let first = word(rest);
        match first.as_str() {
            "fn" | "mod" | "struct" | "enum" | "trait" | "union" | "impl" | "static" => {
                return true;
            }
            "const" => {
                // `const NAME` (an item) or a `const fn` qualifier.
                return true;
            }
            "async" | "unsafe" => rest = rest[first.len()..].trim_start(),
            _ => return false,
        }
    }
}

/// Split the run of complete `#[…]` attributes that opens `text` (after
/// leading whitespace) from the rest of the line: `#[cfg(test)] mod x;` is
/// `(["#[cfg(test)]"], "mod x;")`. Brackets inside a string literal do not end
/// an attribute; an attribute left open at the end of the line ends the run
/// (multi-line attributes are not split). Inner attributes (`#![…]`) are not
/// item attributes and are left in place. Mirrored by
/// `scripts/check_no_panics.py::split_leading_attributes` — the same algorithm,
/// so both trackers see the same attributes on the same line.
#[allow(dead_code)]
pub(crate) fn split_leading_attributes(text: &str) -> (Vec<&str>, &str) {
    let mut attributes = Vec::new();
    let mut rest = text.trim_start();
    while rest.starts_with("#[") {
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut end = None;
        for (index, ch) in rest.char_indices() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_string = false;
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '[' => depth += 1,
                ']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else { break };
        attributes.push(&rest[..=end]);
        rest = rest[end + 1..].trim_start();
    }
    (attributes, rest)
}

/// Per line of `source`: whether it is **test** context — inside an item gated
/// by a test-only attribute (or inside such an item's block), or a line whose
/// leading attribute run holds a test-only attribute. Comments and string
/// literals are stripped first with line structure preserved, so a brace or a
/// `#[cfg(test)]` inside a literal or comment moves nothing. A module merely
/// *named* `tests` is not test context unless gated (it ships in release
/// builds otherwise).
///
/// Attributes are read per attribute, not per line ([`split_leading_attributes`]):
/// `#[cfg(test)] mod x_tests;` on one line gates `x_tests` exactly as the same
/// attribute on the line above would — until 2026-11-17 the one-line form read
/// as ungated (gate audit 2026-08 §9 residual 2), as did a gating line with a
/// trailing comment.
#[allow(dead_code)]
pub(crate) fn test_context_lines(source: &str, test_support_is_test: bool) -> Vec<bool> {
    let code = strip_comments_and_strings(source);
    let raw: Vec<&str> = source.lines().collect();
    let code_lines: Vec<&str> = code.lines().collect();
    let mut contexts = vec![false; code_lines.len()];
    let mut block_stack: Vec<bool> = Vec::new();
    let mut pending_attr = false;
    let mut pending_block: Option<bool> = None;
    for (index, line) in code_lines.iter().enumerate() {
        let current = block_stack.last().copied().unwrap_or(false);
        let (code_attributes, rest) = split_leading_attributes(line);
        // The test check reads the RAW attribute (the stripper blanks the
        // `"test-support"` value); the code split only says how many there are.
        let test_attr = !code_attributes.is_empty()
            && raw.get(index).is_some_and(|raw_line| {
                split_leading_attributes(raw_line)
                    .0
                    .iter()
                    .take(code_attributes.len())
                    .any(|attribute| attribute_line_is_test_only(attribute, test_support_is_test))
            });
        if test_attr {
            pending_attr = true;
        }
        if line_opens_item(rest) {
            pending_block = Some(pending_attr || current);
            pending_attr = false;
        } else if !rest.trim().is_empty() && !rest.starts_with("#[") && pending_attr {
            // An attribute still open at the end of the line (`#[allow(`)
            // keeps the pending gate, as a whole attribute line does.
            pending_attr = false;
        }
        contexts[index] = current || pending_block.unwrap_or(false) || test_attr;
        for ch in line.chars() {
            match ch {
                '{' => block_stack.push(
                    pending_block
                        .take()
                        .unwrap_or_else(|| block_stack.last().copied().unwrap_or(false)),
                ),
                '}' => {
                    block_stack.pop();
                }
                _ => {}
            }
        }
        if line.trim().ends_with(';') {
            pending_block = None;
        }
    }
    contexts
}

/// Production lines of one source file under the one counting rule
/// ([`test_context_lines`], `feature = "test-support"` counting as test, as in
/// `scripts/ci/lib/production_loc.py`). Comment and blank lines in production
/// context still count, matching the `wc -l` semantics the ceilings were
/// seeded with.
#[allow(dead_code)]
pub(crate) fn production_line_count(source: &str) -> usize {
    test_context_lines(source, true)
        .into_iter()
        .filter(|in_test| !in_test)
        .count()
}

/// The comment/string-stripped code of each **production** line of `source`,
/// with its 1-based line number — what a production-only scan should read.
#[allow(dead_code)]
pub(crate) fn production_code_lines(source: &str) -> Vec<(usize, String)> {
    let contexts = test_context_lines(source, true);
    strip_comments_and_strings(source)
        .lines()
        .zip(contexts)
        .enumerate()
        .filter(|(_, (_, in_test))| !in_test)
        .map(|(index, (line, _))| (index + 1, line.to_string()))
        .collect()
}

/// For a stripped code line that is exactly an out-of-line `mod name;`
/// (optionally `pub`/`pub(..)`), the module name.
#[allow(dead_code)]
fn out_of_line_mod_name(code: &str) -> Option<String> {
    let mut rest = code.trim_start();
    if let Some(after) = rest.strip_prefix("pub") {
        let after_trim = after.trim_start();
        if let Some(group) = after_trim.strip_prefix('(') {
            rest = group[group.find(')')? + 1..].trim_start();
        } else if after.starts_with(char::is_whitespace) {
            rest = after_trim;
        } else {
            return None;
        }
    }
    let after_mod = rest.strip_prefix("mod")?;
    if !after_mod.starts_with(char::is_whitespace) {
        return None;
    }
    let after_mod = after_mod.trim_start();
    let name: String = after_mod
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() || !after_mod[name.len()..].trim_start().starts_with(';') {
        return None;
    }
    Some(name)
}

/// Whether `path` is the feature-gated `src/test_support` root (the file
/// `src/test_support.rs` or anything under `src/test_support/`) — the one
/// path-based exclusion the LOC counters share with
/// `scripts/check_no_panics.py`'s `is_test_only_path`. Not applied by
/// [`production_rust_files`]: an `impl` behind `feature = "test-support"` is a
/// real build edge for the impl ratchets, but it is not production mass.
#[allow(dead_code)]
pub(crate) fn is_test_support_root(src: &Path, path: &Path) -> bool {
    path.strip_prefix(src)
        .ok()
        .and_then(|relative| relative.components().next())
        .is_some_and(|first| {
            first.as_os_str() == "test_support" || first.as_os_str() == "test_support.rs"
        })
}
