# Lint backlog

Lints that are enforced in only part of the workspace, and what is left before
they can widen. Each entry names where the enforced scope is pinned.

## `clippy::let_underscore_must_use`

Denied in 71 triaged crates (every package under `crates/`) via a crate-root
`#![deny(...)]` in every target root; the list and its gate are
`crates/app/ironclaw_architecture_tests/tests/reborn_let_underscore_scope.rs`
(rationale in the root `Cargo.toml`, under `[workspace.lints.rust]`). The list
holds crate *directory* names (`memory-native`, `slack`), as the gate resolves
them; a crate needs a `src/lib.rs` or `src/main.rs` root (`ironclaw_cli` is
binary-only). In a triaged crate a kept discard carries a statement-level
`#[expect(clippy::let_underscore_must_use, reason = "...")]`; everything else
was fixed (propagated, asserted, or traced at `debug!`).

History: 12 crates triaged on 2026-11-13; batch 1 (2026-11-15, branch
`chore/triage-let-underscore-batch-1`) added 33 more and cleared 160 distinct
sites; batch 2 (2026-11-19, branch `chore/triage-let-underscore-batch-2`)
enrolled the 22 zero-site crates and triaged `ironclaw_llm` (91),
`ironclaw_cli_providers` (56), `ironclaw_trace_commons` (75) and
`ironclaw_cli` (80), 302 sites.

Remaining on 2026-11-19: **360 distinct sites (file:line)**, all outside
`crates/`. Count with
`cargo clippy --all --benches --tests --examples --all-features -- -W clippy::let_underscore_must_use 2>&1 | grep 'non-binding .let. on'`
and de-duplicate by file:line (lib and test targets are linted separately, so a
site may count twice).

| Untriaged package | Distinct sites |
| --- | --- |
| root package `ironclaw_integration_tests` (`tests/`) | 213 |
| `tools/ironclaw_stress` | 147 |

Next batch: the root integration suite, then `tools/ironclaw_stress`. Both are
test and tooling code and sit outside `crates/`, so enrolling them needs the
gate's crate resolver extended first (it resolves only `crates/` directories).
Once both are triaged the lint can move into `[workspace.lints.clippy]` and the
per-root denies and `TRIAGED_CRATES` can retire.

To retire a crate from the backlog: fix or `#[expect(..., reason = "...")]`
each site, add the deny to its target roots, and add it to the gate's list.
Check code behind `#[cfg(target_os = ...)]` by hand, since a lint run on one
platform does not see the other's code.
