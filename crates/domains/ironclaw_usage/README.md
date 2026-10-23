# ironclaw_usage

Subscription usage windows for the desktop app's Usage page and status bar:
how much of each provider's 5-hour, weekly or plan quota is used, and when it
resets. The provider logic is ported from
[CodexBar](https://github.com/steipete/CodexBar) (MIT, Peter Steinberger).

- **Family / layer:** `domains` / `substrates` · **Package:** `ironclaw_usage`
- **Use this when:** you add a usage provider or change how a provider's
  limits are read.
- **Don't use this when:** you want per-turn token counts or context size
  (that is per-thread telemetry, owned by the CLI drivers).

## Providers

| Id | Source | Credentials |
|---|---|---|
| `claude_cli` | `GET api.anthropic.com/api/oauth/usage` | `~/.claude/.credentials.json`, else Keychain `Claude Code-credentials` |
| `claude_cli_2` | same | `~/.claude-account2/.credentials.json`, else Keychain `Claude Code-credentials-<sha256(dir)[..8]>` |
| `zai` | `GET api.z.ai/api/monitor/usage/quota/limit` | operator secret `llm_provider_{zai,glm}_api_key` |

Read-only: tokens are never refreshed here; each CLI refreshes its own. A
Keychain item is read with `/usr/bin/security` only when the file is missing,
then cached in memory until it expires or is rejected.

The Claude endpoint is undocumented and may change or close. After every
turn the CLI driver (`ironclaw_cli_providers`) saves the CLI's own
`unifiedWindows` reading to `~/.ironclaw/usage/<runtime>.json`; when the
endpoint gives no windows, that reading is shown instead (windows past their
reset are dropped) with a note saying so. `IRONCLAW_USAGE_CLAUDE_ENDPOINT=off`
skips the endpoint, and with it every Keychain read, and uses only that file.

## Public surface

- `UsageMonitor::new(egress, secrets)` for the real accounts;
  `UsageMonitor::with_sources` for tests.
- `UsageMonitor::report().await` returns `ironclaw_product_contracts::usage::UsageReport`.

Fetching is on demand (no background task). Each provider is polled at most
once per 60 s; a 429 backs off (Retry-After, else doubling from 2 min to
30 min) and keeps the last good windows. Every response field is optional, so
schema drift drops a window instead of failing the provider.

## Tests

`cargo test -p ironclaw_usage` runs against fixtures through a fake egress;
nothing reaches the real endpoints.
