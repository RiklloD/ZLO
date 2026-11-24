//! Workspace dependency centralization and package-metadata inheritance.
//!
//! Before this gate, the same external crate was declared independently in up
//! to 65 member manifests, and the copies drifted: `wit-component` /
//! `wit-parser` 0.254 beside 0.258 (two wasm-tools lines in `Cargo.lock`),
//! `hex` "0.4" beside "0.4.3", `wat` "1" beside "1.245.1", and one hardcoded
//! `libsql = "0.9"` that bypassed the workspace's feature policy. Every one of
//! those was a silent second resolution path. The rule now:
//!
//! 1. An external (non-`path`) dependency declared by **three or more**
//!    workspace members is defined once in the root `[workspace.dependencies]`.
//! 2. Every member reference to a dependency that has a
//!    `[workspace.dependencies]` entry uses `workspace = true` — members may
//!    add `features`, `optional`, and `default-features = true`, never their
//!    own version.
//! 3. Every `[workspace.dependencies]` entry is used by some member (no dead
//!    pins that look authoritative but resolve nothing).
//! 4. Every member inherits `edition`, `rust-version`, `license`, and
//!    `repository` from `[workspace.package]`, so a toolchain bump is one edit.
//!
//! Duplicate *resolved* versions are the other half of this ratchet and live in
//! `deny.toml` (`[bans] multiple-versions = "deny"` with a version-pinned skip
//! list), checked by `cargo deny check`.

#![deny(clippy::let_underscore_must_use)]
mod ratchet_support;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ratchet_support::workspace_root;

/// Members that declare an external crate this many times or more must share
/// one `[workspace.dependencies]` definition.
const CENTRALIZE_AT_MEMBERS: usize = 3;

/// `[workspace.package]` keys every member must inherit.
const INHERITED_PACKAGE_KEYS: &[&str] = &["edition", "rust-version", "license", "repository"];

const DEPENDENCY_TABLES: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

/// One external dependency declaration in one member manifest.
#[derive(Debug)]
struct Declaration {
    member: String,
    table: String,
    key: String,
    inherits_workspace: bool,
}

/// The external (non-`path`) dependency declarations of one member manifest,
/// keyed by the registry crate name (`package = "…"` renames resolve to the
/// real crate).
fn external_declarations(member: &str, manifest: &toml::Table) -> Vec<(String, Declaration)> {
    let mut tables: Vec<(String, &toml::Table)> = Vec::new();
    for name in DEPENDENCY_TABLES {
        if let Some(table) = manifest.get(*name).and_then(toml::Value::as_table) {
            tables.push(((*name).to_string(), table));
        }
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for (cfg, target) in targets {
            for name in DEPENDENCY_TABLES {
                if let Some(table) = target.get(*name).and_then(toml::Value::as_table) {
                    tables.push((format!("target.{cfg}.{name}"), table));
                }
            }
        }
    }

    let mut out = Vec::new();
    for (table_name, table) in tables {
        for (key, spec) in table {
            let (crate_name, inherits_workspace) = match spec {
                toml::Value::Table(spec) => {
                    if spec.contains_key("path") {
                        continue;
                    }
                    let crate_name = spec
                        .get("package")
                        .and_then(toml::Value::as_str)
                        .unwrap_or(key)
                        .to_string();
                    let inherits =
                        spec.get("workspace").and_then(toml::Value::as_bool) == Some(true);
                    (crate_name, inherits)
                }
                _ => (key.clone(), false),
            };
            out.push((
                crate_name,
                Declaration {
                    member: member.to_string(),
                    table: table_name.clone(),
                    key: key.clone(),
                    inherits_workspace,
                },
            ));
        }
    }
    out
}

/// Every rule violation across the given root manifest and member manifests.
fn centralization_violations(root: &toml::Table, members: &[(String, toml::Table)]) -> Vec<String> {
    let workspace_dependencies: BTreeSet<String> = root
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml::Value::as_table)
        .map(|table| table.keys().cloned().collect())
        .unwrap_or_default();

    let mut by_crate: BTreeMap<String, Vec<Declaration>> = BTreeMap::new();
    for (member, manifest) in members {
        for (crate_name, declaration) in external_declarations(member, manifest) {
            by_crate.entry(crate_name).or_default().push(declaration);
        }
    }

    let mut violations = Vec::new();
    for (crate_name, declarations) in &by_crate {
        let member_count = declarations
            .iter()
            .map(|declaration| declaration.member.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let centralized = workspace_dependencies.contains(crate_name);
        if member_count >= CENTRALIZE_AT_MEMBERS && !centralized {
            violations.push(format!(
                "`{crate_name}` is declared by {member_count} workspace members but has no \
                 root `[workspace.dependencies]` entry; add one and reference it with \
                 `{{ workspace = true }}`"
            ));
        }
        if centralized {
            for declaration in declarations.iter().filter(|d| !d.inherits_workspace) {
                violations.push(format!(
                    "{}: `[{}] {}` declares its own version of `{crate_name}`; it has a root \
                     `[workspace.dependencies]` entry, so use `{{ workspace = true }}` \
                     (keep per-member `features` / `optional`)",
                    declaration.member, declaration.table, declaration.key
                ));
            }
        }
    }
    for crate_name in &workspace_dependencies {
        if !by_crate.contains_key(crate_name) {
            violations.push(format!(
                "root `[workspace.dependencies]` pins `{crate_name}`, but no workspace member \
                 uses it; delete the entry"
            ));
        }
    }
    violations
}

/// Members that hardcode, or omit, a `[workspace.package]` key.
fn metadata_violations(members: &[(String, toml::Table)]) -> Vec<String> {
    let mut violations = Vec::new();
    for (member, manifest) in members {
        let package = manifest.get("package").and_then(toml::Value::as_table);
        for key in INHERITED_PACKAGE_KEYS {
            let inherits = package
                .and_then(|package| package.get(*key))
                .and_then(toml::Value::as_table)
                .and_then(|value| value.get("workspace"))
                .and_then(toml::Value::as_bool)
                == Some(true);
            if !inherits {
                violations.push(format!(
                    "{member}: `[package] {key}` must be `{key}.workspace = true`"
                ));
            }
        }
    }
    violations
}

fn read_manifest(path: &Path) -> toml::Table {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    text.parse::<toml::Table>()
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()))
}

/// The root manifest plus every `[workspace] members` manifest, in order.
fn workspace_manifests() -> (toml::Table, Vec<(String, toml::Table)>) {
    let root_dir = workspace_root();
    let root = read_manifest(&root_dir.join("Cargo.toml"));
    let members = root
        .get("workspace")
        .and_then(|workspace| workspace.get("members"))
        .and_then(toml::Value::as_array)
        .expect("root Cargo.toml must list [workspace] members")
        .iter()
        .map(|member| {
            let member = member.as_str().expect("workspace member is a string");
            (
                member.to_string(),
                read_manifest(&root_dir.join(member).join("Cargo.toml")),
            )
        })
        .collect::<Vec<_>>();
    (root, members)
}

#[test]
fn shared_external_dependencies_are_defined_once_in_workspace_dependencies() {
    let (root, members) = workspace_manifests();
    // Non-vacuity: the workspace has ~70 members and dozens of shared crates;
    // a manifest walk that finds neither is reading the wrong root.
    assert!(
        members.len() > 50,
        "expected the full workspace member list, found {}",
        members.len()
    );
    let shared = {
        let mut counts: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
        for (member, manifest) in &members {
            for (crate_name, _) in external_declarations(member, manifest) {
                counts.entry(crate_name).or_default().insert(member);
            }
        }
        counts
            .values()
            .filter(|m| m.len() >= CENTRALIZE_AT_MEMBERS)
            .count()
    };
    assert!(
        shared > 20,
        "expected dozens of shared external crates, found {shared}"
    );

    let violations = centralization_violations(&root, &members);
    assert!(
        violations.is_empty(),
        "workspace dependency centralization violations:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn every_member_inherits_workspace_package_metadata() {
    let (_, members) = workspace_manifests();
    let violations = metadata_violations(&members);
    assert!(
        violations.is_empty(),
        "workspace package metadata violations:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn centralization_check_flags_drift_uncentralized_and_dead_entries() {
    let root: toml::Table = r#"
        [workspace]
        members = ["a", "b", "c"]
        [workspace.dependencies]
        serde = "1"
        unused = "1"
    "#
    .parse()
    .expect("fixture root");
    let member = |text: &str| text.parse::<toml::Table>().expect("fixture member");
    let members = vec![
        (
            "a".to_string(),
            member(
                r#"
                [dependencies]
                serde = { workspace = true, features = ["derive"] }
                tokio = { version = "1", features = ["rt"] }
                sibling = { path = "../b" }
                "#,
            ),
        ),
        (
            "b".to_string(),
            member(
                r#"
                [dev-dependencies]
                serde = "1"
                [target.'cfg(unix)'.dependencies]
                tokio = "1"
                "#,
            ),
        ),
        (
            "c".to_string(),
            member(
                r#"
                [dependencies]
                rt = { package = "tokio", version = "1" }
                sibling = { path = "../b" }
                "#,
            ),
        ),
    ];
    let violations = centralization_violations(&root, &members);
    let expect = |needle: &str| {
        assert!(
            violations.iter().any(|v| v.contains(needle)),
            "expected a violation containing {needle:?}, got {violations:#?}"
        );
    };
    // tokio: three members (one via a `package` rename, one under a target
    // table) and no workspace entry.
    expect("`tokio` is declared by 3 workspace members");
    // serde: centralized, but `b` hardcodes a version.
    expect("b: `[dev-dependencies] serde` declares its own version of `serde`");
    // unused: pinned at the root, referenced nowhere.
    expect("pins `unused`, but no workspace member uses it");
    // Path dependencies are internal and never counted.
    assert!(
        !violations.iter().any(|v| v.contains("sibling")),
        "{violations:#?}"
    );
    assert_eq!(violations.len(), 3, "{violations:#?}");
}

#[test]
fn metadata_check_flags_hardcoded_and_missing_keys() {
    let members = vec![(
        "a".to_string(),
        r#"
        [package]
        name = "a"
        edition = "2024"
        rust-version.workspace = true
        repository.workspace = true
        "#
        .parse::<toml::Table>()
        .expect("fixture member"),
    )];
    let violations = metadata_violations(&members);
    assert_eq!(
        violations,
        vec![
            "a: `[package] edition` must be `edition.workspace = true`".to_string(),
            "a: `[package] license` must be `license.workspace = true`".to_string(),
        ]
    );
}
