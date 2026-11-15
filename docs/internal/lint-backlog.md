# Lint backlog

Lints that are enforced in only part of the workspace, and what is left before
they can widen. Each entry names where the enforced scope is pinned.

## `clippy::let_underscore_must_use`

Denied in 45 triaged crates via a crate-root `#![deny(...)]` in every target
root; the list and its gate are
`crates/app/ironclaw_architecture_tests/tests/reborn_let_underscore_scope.rs`
(rationale in the root `Cargo.toml`, under `[workspace.lints.rust]`). The list
holds crate *directory* names (`memory-native`, `slack`), as the gate resolves
them. In a triaged crate a kept discard carries a statement-level
`#[expect(clippy::let_underscore_must_use, reason = "...")]`; everything else
was fixed (propagated, asserted, or traced at `debug!`).

History: 12 crates triaged on 2026-11-13; batch 1 (2026-11-15, branch
`chore/triage-let-underscore-batch-1`) added 33 more and cleared 160 distinct
sites.

Remaining on 2026-11-15: **662 distinct sites (file:line) in 133 files**; the
count command below reports 665, because lib and test targets are linted
separately and a site may count twice:
`cargo clippy --all --benches --tests --examples --all-features -- -W clippy::let_underscore_must_use 2>&1 | grep -c 'non-binding .let. on'`.
Production sites are marked `// silent-ok: <why>`; test sites are unmarked.

| Untriaged crate | Distinct sites |
| --- | --- |
| root package `ironclaw_integration_tests` (`tests/`) | 213 |
| `tools/ironclaw_stress` | 147 |
| `ironclaw_llm` | 91 |
| `ironclaw_cli` | 80 |
| `ironclaw_trace_commons` | 75 |
| `ironclaw_cli_providers` | 56 |

Next batches, in order: `ironclaw_llm` and `ironclaw_cli_providers` (provider
and CLI-runtime error paths), then `ironclaw_trace_commons`, then
`ironclaw_cli`. The root integration suite and `tools/ironclaw_stress` are test
and tooling code and come last; both sit outside `crates/`, so enrolling them
needs the gate's crate resolver extended first.

Twenty-two crates under `crates/` have **zero** sites but are not yet enrolled
(`ironclaw_common`, `ironclaw_extension_contracts`, `ironclaw_loop_contracts`,
`ironclaw_product_contracts`, `ironclaw_prompt_envelope`,
`ironclaw_conversations`, `ironclaw_extractors`, `ironclaw_outbound`,
`ironclaw_skills`, `ironclaw_supermemory`, `ironclaw_web_app`,
`ironclaw_extension_support`, `web-app`, `ironclaw_authorization`,
`ironclaw_runtime_policy`, `ironclaw_trust`, `ironclaw_turns`,
`ironclaw_agent_loop`, `ironclaw_host_ingress`, `ironclaw_libsql_runtime`,
`ironclaw_observability`, `ironclaw_safety`). Enrolling them is mechanical:
add the deny to each target root and the name to the gate's list.

To retire a crate from the backlog: fix or `#[expect(..., reason = "...")]`
each site, add the deny to its target roots, and add it to the gate's list.
Check code behind `#[cfg(target_os = ...)]` by hand, since a lint run on one
platform does not see the other's code.
