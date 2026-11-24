# IronClaw — Claude Code adapter

@AGENTS.md

Everything above (from `AGENTS.md`) is the canonical, tool-neutral contract.
The rest of this file is Claude-specific.

## Check loop (read before running cargo)

- Use `cargo check -p <crate>` while iterating. Run `cargo clippy` only once, at the end of a task — don't run clippy after every edit.
- Run only the tests for the crate or module you changed, e.g. `cargo test -p <crate> some_test`. Don't run the whole suite unless the user asks.

## Skills and rules

- Project skills live in `.claude/skills/` — start from
  `ironclaw-reborn-orientation`; use `reborn-feature` for cross-layer product
  work, `reborn-extension-surfaces` for integrations,
  `ironclaw-reborn-testing` for test tiers, and
  `ironclaw-reborn-architecture-review` for boundary changes.
- Path-scoped rules in `.claude/rules/*.md` load automatically when you read
  matching files — they are canonical for their topics (testing, database,
  types, cargo-features, review discipline, guidance maintenance, …); do not
  restate them here. Guidance edits (`.claude/**`, any `AGENTS.md`/`CLAUDE.md`,
  runtime `skills/`) are governed by `.claude/rules/guidance-maintenance.md`.

## Codebase knowledge graph (MCP)

When the optional `codebase-memory` MCP server is installed locally, it indexes
`crates/` into a local knowledge graph (per-environment state in
`.codebase-memory/`, deliberately git-ignored so compressed SQLite blobs do not
bloat git history). Check freshness first: `bash scripts/codebase-graph.sh status`.

- Missing → `index_repository(repo_path=".", persistence=true)` once (or fall
  back to `crates/AGENTS.md` + targeted `rg`).
- Stale → `detect_changes(since="<indexed-commit>")`, or re-run
  `index_repository`.
- Where is a symbol → `search_graph(name_pattern=…)`, then
  `get_code_snippet(qualified_name=…)`.
- Who calls X / what X calls → `trace_path(function_name=…, mode="calls")`;
  value flow → `mode="data_flow"`; cross-crate feature path →
  `mode="cross_service"`.
- Structure of an area → `get_architecture(…)`; graph-augmented text search →
  `search_code(pattern=…)`; arbitrary queries → `query_graph(<Cypher>)`.

The graph is a point-in-time index — verify anything it asserts against live
code before acting. `Grep`/`Glob`/`Read` remain correct for text, config, and
non-code files. `openwiki/` is generated prose (never hand-edit) and may lag
recent code — verify claims against live symbols.

## REPL/TUI logging rule

`info!` and `warn!` output appears in the REPL and corrupts the terminal UI.
Use `debug!` for internal diagnostics (trace analysis, reflection results,
engine internals); reserve `info!` for user-facing status the REPL
intentionally renders. Background tasks must NEVER use `info!`.
