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
| `claude_cli` | `claude -p` `get_usage` control request, then `GET api.anthropic.com/api/oauth/usage` | the CLI's own login; for the endpoint, the live one of `~/.claude/.credentials.json` and Keychain `Claude Code-credentials` |
| `claude_cli_2` | same, with `CLAUDE_CONFIG_DIR=~/.claude-account2` | same; Keychain `Claude Code-credentials-<sha256(dir)[..8]>` |
| `codex` | `codex app-server` `account/rateLimits/read` | the CLI's ChatGPT login |
| `zai` | `GET api.z.ai/api/monitor/usage/quota/limit` | operator secret `llm_provider_{zai,glm}_api_key` |
| `antigravity` | `v1internal:retrieveUserQuotaSummary` (fallback `fetchAvailableModels`) | refresh token in the ACP profile's `antigravity-acp/acp_token.json` |
| `muse_code` | last `usage/changed` reading a runtime saved | none (local files) |

The CLI probes follow t3code (pingdotgg/t3code, MIT): the CLI answers with
its own, freshly refreshed login, the plan name and every window, so the
Claude probe is tried before the undocumented endpoint. The CLI on macOS
refreshes only its Keychain copy; a credentials file left by an older login
is used only while it is still live. Tokens are never refreshed or written
here (the Antigravity access token is refreshed in memory only). A Keychain
item is read with `/usr/bin/security` only when the file has no live token,
then cached in memory until it expires or is rejected.

After every turn the CLI drivers (`ironclaw_cli_providers`) save the CLI's
own reading to `~/.ironclaw/usage/<runtime>.json`. For Claude it overlays the
cached windows between polls, so bars move with use; when neither the CLI
nor the endpoint answers it is shown alone (windows past their reset are
dropped) with a note. Muse Code reports limits only during a turn, so its
card always shows the newest saved reading (IronClaw's or the Hermes
runtime's). `IRONCLAW_USAGE_CLAUDE_ENDPOINT=off` skips the endpoint, and with
it every Keychain read.

## Public surface

- `UsageMonitor::for_host(secrets)` / `UsageMonitor::new(egress, home, secrets)`
  for the real accounts;
  `UsageMonitor::with_sources` for tests.
- `UsageMonitor::report().await` returns `ironclaw_product_contracts::usage::UsageReport`.

Fetching is on demand (no background task). Each provider is polled at most
once per 60 s (Z.ai), 120 s (the CLI probes, which start a process), 180 s
(Antigravity) or 15 s (Muse's local file); a 429 backs off (Retry-After, else doubling from 2 min to
30 min) and keeps the last good windows. Every response field is optional, so
schema drift drops a window instead of failing the provider.

## Tests

`cargo test -p ironclaw_usage` runs against fixtures through a fake egress;
nothing reaches the real endpoints.
