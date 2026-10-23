# ironclaw_cli_providers

Coding-agent CLIs as `ironclaw_orchestrator` turn runtimes. Today the Claude
CLI, registered twice: `claude-cli` (default account) and `claude-cli-2`
(`CLAUDE_CONFIG_DIR=~/.claude-account2`), only when a `claude` binary is
found. Antigravity (ACP) and Muse Code drivers follow on the same seam.

- **Family / layer:** `domains` / `substrates` · **Package:** `ironclaw_cli_providers`
- **Use this when:** you change how a CLI agent runs a turn: process spawn,
  stream-json parsing, session resume, Stop, usage-limit mapping, telemetry.
- **Don't use this when:** you change orchestration semantics
  (`ironclaw_orchestrator`) or IronClaw's own loop runtime
  (`ironclaw_assistant::orchestration`).

## How a turn runs

One `claude -p --output-format stream-json --input-format stream-json
--verbose --include-partial-messages` process per turn. The thread's
`native_session_id` is minted at creation; the first turn passes
`--session-id`, later turns `--resume`. Stop sends SIGTERM to the process
group, SIGKILL after 2 s. A rejected `rate_limit_event` (or a limit-shaped
error result) becomes `usage_limit_resets_at`. History reads the CLI's own
session journal. The CLI child dies with the backend, so
`reports_restart_cuts` stays off and the engine's live-work rows cover cuts.

Wire types come from `claude-codes`, pinned to the checked CLI release
(`claude::EXPECTED_CLI_VERSION`, `2.1.302`); a different installed version
is logged at `debug`.

## Tools are off

The user keeps auto-approve off, and the CLI's permission path cannot reach
IronClaw's approval queue (`ironclaw_approvals`) yet. Until that bridge
lands, every turn runs with `--tools ""`, `--strict-mcp-config`,
`--permission-mode default` and `--permission-prompts none`: no tools, no
MCP servers, and anything that would prompt is denied. No allow rules are
passed and `--dangerously-skip-permissions` is never used.

## Telemetry

After each turn: context used (last assistant message: input + cache read +
cache creation), context max (`result.modelUsage[model].contextWindow`),
tokens per second over the streamed messages, and cache hit rate, reported
through `TurnEventSink::turn_telemetry` and served by
`GET /api/webchat/v2/threads/{thread_id}/telemetry`.
