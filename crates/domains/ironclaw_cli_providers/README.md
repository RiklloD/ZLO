# ironclaw_cli_providers

Coding-agent CLIs as `ironclaw_orchestrator` turn runtimes: the Claude CLI,
registered twice: `claude-cli` (default account) and `claude-cli-2`
(`CLAUDE_CONFIG_DIR=~/.claude-account2`), only when a `claude` binary is
found; `antigravity` (Google's ACP agent) and `muse-code` (Meta's Muse Code),
each registered when installed (`register_antigravity_runtime`,
`register_muse_runtime`).

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

## Antigravity (`antigravity`)

Runs Google's Antigravity ACP agent (`agy_acp_server.par` with
`localharness_external` beside it), not the `agy` CLI: `agy` 1.4.0 has no
ACP mode. The runtime is found where Hermes or T3 Code installed it
(`~/.hermes/plugin-data/t3code/antigravity-acp`, then
`~/.t3/tools/antigravity-acp`, `<platform>/active.json` naming the version).
The protocol is spoken directly over newline JSON-RPC (`src/rpc.rs`); the
`agent-client-protocol` crate is not used because it turns on `serde_json`'s
`preserve_order` for the whole workspace build.

- **Sign-in:** `GEMINI_HOME` is the profile Hermes already signed in
  (`~/.hermes/plugin-data/t3code/antigravity/profile`) when it exists, else
  `~/.ironclaw/antigravity/profile`. `authenticated` is the presence of
  `antigravity-acp/acp_token.json` there. The agent inherits only an environment
  allowlist (`HOME`, `PATH`, locale, proxy and CA settings), so ambient
  cloud credentials and API keys never reach it, and `BROWSER` is `/usr/bin/true`: a
  missing sign-in fails the turn after `auth_timeout` instead of opening a
  browser.
- **Turns:** one agent process per turn (it starts in about a second):
  `initialize`, `authenticate`, `session/new` on the first turn (the agent's
  session id is kept in `~/.ironclaw/antigravity/sessions/<key>`), then
  `session/resume`. The thread's model is applied with
  `session/set_config_option` (`default` keeps the agent's choice).
- **Stop:** `session/cancel`, then SIGTERM/SIGKILL of the process group
  after 2 s each. No in-turn steering: a steer restarts the turn.
- **History:** IronClaw's own journal,
  `~/.ironclaw/antigravity/transcripts/<key>.jsonl` (user prompt, final
  answer, tool call names).
- **Models:** the list the last session reported
  (`~/.ironclaw/antigravity/models.json`), else Hermes's cache in the shared
  profile, else `default`.
- **Limits:** a quota-shaped error (`RESOURCE_EXHAUSTED`, "quota", "rate
  limit") becomes a `usage_limit:` failure; ACP reports no reset time.
- **Telemetry:** context used and window from ACP `usage_update`; cache hit
  rate and TPS only when `session/prompt` returns `usage` (the 1.3.0 agent
  does not, so TPS stays empty).
- **Tools:** the client offers no file system and no terminal, the session
  stays in the agent's ask-first `default` mode, and every
  `session/request_permission` goes through `permissions::decide`, which
  answers `reject_once`. Native questions are cancelled.

## Muse Code (`muse-code`)

Runs `muse serve` and speaks the Muse Session Protocol directly over the same
JSON-RPC module, mirroring the Hermes t3code driver. `muse-code-sdk` is not a
dependency: its facade has no `turn/interrupt`, `model/list` or
`usage/read`, and its spawn does not start a process group.

- **Turns:** one host per turn. The thread's `native_session_id` is a UUIDv7
  minted at creation; the first turn calls `session/start` with it, later
  turns `session/resume` (`excludeItems`). Commands carry UUIDv7 ids.
- **Steer:** `turn/steer` with `expectedTurnId` into the running turn
  (`steer_in_turn`); a refused steer or `SteerHow::Restart` restarts.
- **Stop:** `turn/interrupt`, then SIGTERM/SIGKILL of the process group.
- **History:** IronClaw's journal,
  `~/.ironclaw/muse-code/transcripts/<session>.jsonl`, which also marks the
  session as started.
- **Models and sign-in:** `model/list` from a read-only host
  (`--no-session-log`), cached 10 minutes; MSP has no sign-in signal, so a
  non-empty catalog counts as authenticated, as in Hermes.
- **Limits:** `turn/completed.error.quota.resetsAtMs` becomes
  `usage_limit_resets_at`. The `usage/read` / `usage/changed` subscription
  windows are saved to `~/.ironclaw/usage/muse-code.json`
  (`{"observedAt", "usage"}`).
- **Telemetry:** context used and window from `session/contextUsage`
  (window falls back to the catalog's `contextLimit`), cache hit rate from
  the last `session/tokenUsage`, TPS as output tokens over the summed
  `durationMs` of the turn's model calls.
- **Tools:** `--disable-shell --disable-write --disable-request-user-input`,
  no `--trust-workspace` (workspace rules cannot allow anything), approval
  mode `denyUnmatched`, and any approval Muse still raises goes through
  `permissions::decide`, which picks the `denied` (else `abort`) choice.

## The approval seam

`permissions::decide` is the one function both drivers ask about a tool
call. It denies everything until the bridge into `ironclaw_approvals` lands;
the bridge replaces its body (async if it waits for the user).

## Tests

`tests/claude_driver.rs` and `tests/agent_hosts.rs` run the drivers against
fake agent scripts. `tests/live_smoke.rs` holds ignored live canaries, one
short real turn per agent host:
`cargo test -p ironclaw_cli_providers --test live_smoke -- --ignored --nocapture`.
