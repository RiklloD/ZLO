# ADR 0006: Coding-agent CLIs run as orchestrator runtimes in `ironclaw_cli_providers`

**Status:** Accepted 2026-10-23 (IronClaw v2 plan, stage 4 "CLI providers").
**Rows:** PROPOSAL §5 tree entry `ironclaw_cli_providers`; same-layer edge
`ironclaw_cli_providers → ironclaw_orchestrator`.

## Context

Stage 4 runs turns on external coding-agent CLIs (Claude CLI now,
Antigravity and Muse Code later) next to IronClaw's own loop. The engine
already abstracts runtimes behind `TurnRuntime`; each driver needs process
handling, a wire parser and a vendor name.

## Decision

- One `domains/` crate, `ironclaw_cli_providers`, at the `substrates` layer,
  holds every CLI driver. Its only in-family edge is to
  `ironclaw_orchestrator` (the `TurnRuntime` seam and telemetry record). It
  is vendor-scoped by charter, like `ironclaw_llm`'s providers.
- Composition calls one function, `register_claude_runtimes`, right after
  the engine opens; runtimes appear only when the `claude` binary exists.
- Claude CLI wire types come from `claude-codes` (Apache-2.0), pinned with
  `=` to the checked CLI release.

## Approvals: tools are off

The user keeps auto-approve off and every tool call must pass IronClaw's
approval queue. The CLI's permission callbacks (`--permission-prompt-tool`,
PreToolUse hooks) have no bridge into `ironclaw_approvals` yet: that queue
is keyed by turn runs and gates the CLI does not have. Until the bridge
exists the driver passes `--tools ""`, `--strict-mcp-config`,
`--permission-mode default` and `--permission-prompts none`, no allow rules,
and never `--dangerously-skip-permissions`. Verified against CLI 2.1.302:
the init frame reports `tools: []`, `mcp_servers: []`.

## Consequences

- CLI threads are orchestrator threads with a native session id; their
  transcript is the CLI's own journal, not `ironclaw_threads`.
- A backend restart kills the CLI child; `reports_restart_cuts` stays off
  and the engine's live-work rows drive restart continuation.
