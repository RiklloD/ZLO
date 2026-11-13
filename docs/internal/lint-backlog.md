# Lint backlog

Lints that are enforced in only part of the workspace, and what is left before
they can widen. Each entry names where the enforced scope is pinned.

## `clippy::let_underscore_must_use`

Denied in 12 triaged crates via a crate-root `#![deny(...)]` in every target
root; the list and its gate are
`crates/app/ironclaw_architecture_tests/tests/reborn_let_underscore_scope.rs`
(rationale in the root `Cargo.toml`, under `[workspace.lints.rust]`). Every
other crate is untriaged — 428 discards in 124 files on 2026-11-13, production
sites marked `// silent-ok: <why>`, test sites unmarked. Count what remains
(lib and test targets are linted separately, so a site may count twice):
`cargo clippy --all --benches --tests --examples --all-features -- -W clippy::let_underscore_must_use 2>&1 | grep -c 'non-binding .let. on'`.
To retire a crate from the backlog: fix or `#[expect(..., reason = "...")]`
each site, add the deny to its target roots, and add it to the gate's list.
