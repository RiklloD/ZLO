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
--verbose --include-partial-messages` process per turn; stdin stays open
for the control protocol until the result arrives. The thread's
`native_session_id` is minted at creation; the first turn passes
`--session-id`, later turns `--resume`. Stop sends SIGTERM to the process
group, SIGKILL after 2 s. A rejected `rate_limit_event` (or a limit-shaped
error result) becomes `usage_limit_resets_at`. History reads the CLI's own
session journal. The CLI child dies with the backend, so
`reports_restart_cuts` stays off and the engine's live-work rows cover cuts.

Wire types come from `claude-codes`, pinned to the checked CLI release
(`claude::EXPECTED_CLI_VERSION`, `2.1.302`); a different installed version
is logged at `debug`.

## Tools and approvals

With a `bridge::CliBridge` (wired by composition next to the engine), the
CLI has its tools and every call waits for a person:

- Before the prompt, the driver sends a stream-json `initialize` control
  request that registers a PreToolUse hook (matcher: every tool, MCP tools
  included). The prompt goes out only after the CLI acknowledges it; a
  refusal or no ack within 60 s fails the turn.
- Each `hook_callback` becomes a pending gate on the bridge. The GPUI app
  shows it as a normal approval in the thread's stream (`gate.resolve`
  answers it, owner only). The driver answers the CLI with an explicit
  `allow` or `deny`, never an error frame: on an error the CLI falls back to
  its own rules, which auto-approve reads. Timeout (10 min), Stop, turn end
  and any failure deny.
- No allow rules are passed, "always allow" is not offered, prompts are
  answered "no" (`--permission-prompts none`), and
  `--dangerously-skip-permissions` is never used.
- The orchestrator tools (`t3_*`) come from one loopback MCP route
  (`POST /api/cli-bridge/mcp`, mounted by composition, 127.0.0.1 peers
  only). Each CLI process gets its own bearer token in a 0600
  `--mcp-config` file (`--strict-mcp-config` stays on); the token is bound
  to the thread and revoked when the turn ends. Calls run as the thread's
  owner scope.

Without a bridge every turn runs with `--tools ""` as before.

While a turn runs, its reply text streams through the bridge into the
thread's event stream (`text`, `gate`, `run_status` projection items). The
prompt's `uuid` is the run id, so the session journal's rows carry it and
the durable reply replaces the live bubble.

Other drivers can reuse the bridge: `request_approval` for a tool call,
`turn_started`/`text_delta`/`turn_ended` for live state, `mint_token` +
`mcp_config_json` for the MCP route.

## Profiles and limits

A profile whose default provider is `claude-cli` or `claude-cli-2` runs its
threads here (`Engine::bind_profile_runtime`); the product surface answers
their submit, Stop and timeline from the engine. After each turn the CLI's
`unifiedWindows` limit reading is saved to `~/.ironclaw/usage/<runtime>.json`
for the Usage page.

## Telemetry

After each turn: context used (last assistant message: input + cache read +
cache creation), context max (`result.modelUsage[model].contextWindow`),
tokens per second over the streamed messages, and cache hit rate, reported
through `TurnEventSink::turn_telemetry` and served by
`GET /api/webchat/v2/threads/{thread_id}/telemetry`.
