# Lint backlog

Lints that are enforced in only part of the workspace, and what is left before
they can widen. Each entry names where the enforced scope is pinned.

No lint is partially enforced today. (`clippy::map_err_ignore` is deferred, not
partially enforced; its note lives in the root `Cargo.toml` under
`[workspace.lints.rust]`.)

## Retired

### `clippy::let_underscore_must_use` — workspace-wide since 2026-11-24

Denied by `[workspace.lints.clippy]` in the root `Cargo.toml`, which binds
every package that inherits workspace lints. The members that do not inherit
them deny it themselves: the root integration-suite package
(`ironclaw_integration_tests`) with its own `[lints.clippy]` entry, and
`ironclaw_assistant` / `ironclaw_webui` with a crate-root
`#![deny(clippy::let_underscore_must_use)]` in every target root.
`crates/app/ironclaw_architecture_tests/tests/reborn_let_underscore_scope.rs`
pins the workspace entry and checks every workspace member is covered one of
those three ways. A kept discard carries a statement-level
`#[expect(clippy::let_underscore_must_use, reason = "...")]`, so a stale
exemption fails the build.

Crates that inherit workspace lints still carry the per-root
`#![deny(clippy::let_underscore_must_use)]` they gained while the lint was
crate-scoped. It is now redundant there and harmless; drop it when touching a
file, but there is no need for a sweep.

History: 12 crates triaged on 2026-11-13; batch 1 (2026-11-15) added 33 and
cleared 160 distinct sites; batch 2 (2026-11-19) enrolled the 22 zero-site
crates and triaged `ironclaw_llm`, `ironclaw_cli_providers`,
`ironclaw_trace_commons` and `ironclaw_cli` (302 sites). The last batch
(2026-11-24, branch `t3/must-use-lint-promotion`) triaged the root integration
suite (39 distinct sites; the 213 counted earlier included each `#[path]`-shared
support file once per test binary) and `tools/ironclaw_stress` (147 sites, 132
of them `let _ = writeln!(String, …)` report lines now routed through the one
infallible `report_text::ReportText` helper), then promoted the lint.

Check code behind `#[cfg(target_os = ...)]` by hand when adding code: a lint
run on one platform does not see the other's code.
