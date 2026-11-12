# IronClaw ↔ OpenClaw Feature Parity Matrix

This document tracks feature parity between IronClaw (the Rust "Reborn" stack under `crates/`, shipped as the `zlo` binary) and OpenClaw (TypeScript reference implementation). The IronClaw column describes only what exists in `crates/` today; the OpenClaw column is unchanged from the last upstream review.

**Legend:**

- ✅ Implemented
- 🚧 Partial (in progress or incomplete)
- ❌ Not implemented
- 🔮 Planned (in scope but not started)
- 🚫 Out of scope (intentionally skipped)
- ➖ N/A (not applicable to Rust implementation)

**Last reviewed against OpenClaw PRs:** 2026-05-02 (merged 2026-03-11 through 2026-04-30, OpenClaw releases 2026.3.11 → 2026.4.30)

**IronClaw column re-verified against the tree:** 2026-11-11. Every IronClaw claim was checked against live code in `crates/` (`rg` over the workspace). Features of the deleted v1 product (Ratatui TUI, WebSocket transport, `SseManager`, `ChannelManager`, Signal/Discord/WeCom/WeChat/Feishu channels, WASM channels, v1 hooks and routines) are no longer counted. No new OpenClaw PR review was done on that date.

**Channels today:** the only channel adapters are the `slack`, `telegram` and `web-app` packages under `crates/extensions/packages/`. The other packages there are tool or memory extensions. No new channels are planned, so every other messaging platform is marked 🚫.

---

## 1. Architecture

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Hub-and-spoke architecture | ✅ | ✅ | The WebChat v2 HTTP gateway (`ironclaw_webui`) is the hub; channels are extension surfaces behind it |
| WebSocket control plane | ✅ | ❌ | The WebSocket event transport was deleted (commit 4e7097706). Transport is HTTP + SSE only |
| Single-user system | ✅ | ✅ | Local single-user is the default profile; served profiles add SSO, per-user scope and sandboxed processes |
| Multi-agent routing | ✅ | 🚧 | `ironclaw_orchestrator` delegates threads to other runtimes (coding-agent CLIs via `ironclaw_cli_providers`); no per-agent workspace routing |
| Session-based messaging | ✅ | ✅ | Typed session/thread/turn/run identities (`ironclaw_threads`, `ironclaw_turns`) |
| Loopback-first networking | ✅ | ✅ | `zlo serve` binds `127.0.0.1` by default (`DEFAULT_SERVE_HOST`) |

### Owner: _Unassigned_

---

## 2. Gateway System

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Gateway control plane | ✅ | ✅ | WebChat v2 routes under `/api/webchat/v2/*` (threads, automations, extensions, llm, memory, fs, skills, logs, operator, admin, …) |
| HTTP endpoints for Control UI | ✅ | ✅ | React SPA served by `ironclaw_webui` (chat, settings, extensions, automations, logs, workspace, projects, admin) |
| Channel connection lifecycle | ✅ | ✅ | Extension lifecycle in `ironclaw_extension_host` / `ironclaw_extension_manager`; channels are extension surfaces, not a separate manager |
| Session management/routing | ✅ | ✅ | `SessionThreadService` + `ironclaw_conversations` bindings |
| Configuration hot-reload | ✅ | 🚧 | LLM provider config reloads at runtime (`ironclaw_operator::llm_admin::llm_reload`); boot config does not |
| Network modes (loopback/LAN/remote) | ✅ | ✅ | Loopback default; `zlo remote enable` publishes on the tailnet via `tailscale serve` (never Funnel) |
| OpenAI-compatible HTTP API | ✅ | ✅ | `/v1/chat/completions`, `/v1/models`, `/v1/responses` (`ironclaw_openai_compat`) |
| Canvas hosting | ✅ | ❌ | |
| Gateway lock (PID-based) | ✅ | ❌ | |
| launchd/systemd integration | ✅ | ✅ | `zlo service install/start/stop/status/uninstall` (launchd on macOS, systemd on Linux) |
| Bonjour/mDNS discovery | ✅ | ❌ | |
| Tailscale integration | ✅ | ✅ | `zlo remote enable/status/disable`, Tailscale identity listener in the gateway |
| Health check endpoints | ✅ | 🚧 | `/api/health` only; no `/healthz` / `/readyz` |
| `doctor` diagnostics | ✅ | 🚧 | `zlo doctor` checks the home, profile, `config.toml` and `providers.json`; WebUI has an operator diagnostics view |
| Agent event broadcast | ✅ | ✅ | Durable events → projections → `EventStreamManager` (`ironclaw_event_streams`) → SSE |
| Channel health monitor | ✅ | ❌ | |
| Presence system | ✅ | ❌ | |
| Trusted-proxy auth mode | ✅ | ❌ | Rate limiting deliberately ignores `X-Forwarded-For` |
| APNs push pipeline | ✅ | ❌ | Browser Web Push exists (`web-app` channel); no APNs |
| Oversized payload guard | ✅ | ✅ | Body-limit middleware runs before auth (`webui_body_limit.rs`) |
| Pre-prompt context diagnostics | ✅ | 🚧 | Operator-only run inspector shows prompt and activity; no token breakdown log |
| OpenAI-compat `/v1/models`, `/v1/embeddings` | ✅ | 🚧 | `/v1/models` exists; no `/v1/embeddings` |
| Outbound proxy routing | ✅ | ❌ | |
| Diagnostics export bundle | ✅ | ❌ | |
| Startup diagnostics timeline | ✅ | ❌ | |
| Event-loop readiness in `/readyz` | ✅ | ❌ | |
| OpenTelemetry exporter pipeline | ✅ | ❌ | `ironclaw_observability` only provides latency-trace macros |
| Prometheus exporter | ✅ | ❌ | |
| Stability snapshots / payload-free liveness | ✅ | ❌ | |

### Owner: _Unassigned_

---

## 3. Messaging Channels

| Channel | OpenClaw | IronClaw | Priority | Notes |
|---------|----------|----------|----------|-------|
| CLI/TUI | ✅ | 🚧 | - | `zlo repl` and `zlo run` are line-based stdin REPLs; there is no TUI |
| HTTP webhook | ✅ | ✅ | - | Channel webhooks under `/webhooks/extensions/<id>/…`, verified by the host's manifest-declared recipe |
| REPL (simple) | ✅ | ✅ | - | `zlo repl` |
| WASM channels | ❌ | ❌ | - | Removed. Channel adapters are first-party crates under `crates/extensions/packages/`; WASM is used for tools only |
| WhatsApp | ✅ | 🚫 | - | |
| Telegram | ✅ | ✅ | - | Bot API over a verified webhook, generated-code pairing, group mention via `bot_username`, forum-topic replies (`message_thread_id`), inbound file download, plus 15 linked-account (MTProto) tools |
| Discord | ✅ | 🚫 | - | |
| Signal | ✅ | 🚫 | - | |
| Slack | ✅ | ✅ | - | Events API channel with replies streamed through Slack's native Agent API, plus 16 user-token WASM tools |
| iMessage | ✅ | 🚫 | - | |
| Linq | ✅ | 🚫 | - | |
| Feishu/Lark | ✅ | 🚫 | - | |
| WeCom | ✅ | 🚫 | - | |
| LINE | ✅ | 🚫 | - | |
| WeChat (iLink bot) | ✅ | 🚫 | - | |
| WebChat | ✅ | ✅ | - | WebChat v2 in `ironclaw_webui`, streamed over SSE |
| Matrix | ✅ | 🚫 | - | |
| Mattermost | ✅ | 🚫 | - | |
| Google Chat | ✅ | 🚫 | - | |
| MS Teams | ✅ | 🚫 | - | |
| Twitch | ✅ | 🚫 | - | |
| Voice Call | ✅ | 🚫 | - | |
| Google Meet | ✅ | 🚫 | - | |
| Yuanbao (Tencent) | ✅ | 🚫 | - | |
| WeCom | ✅ | 🚫 | - | |
| Nostr | ✅ | 🚫 | - | |
| Web Push (browser) | ❌ | ✅ | - | `web-app` channel: RFC 8030/8291/8292 pushes to enrolled browsers |

### Telegram-Specific Features (since Feb 2025)

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Forum topic creation | ✅ | ❌ | Replies land in the inbound topic; the bot cannot create topics |
| channel_post support | ✅ | ❌ | |
| User message reactions | ✅ | ❌ | The bot sets its own run-status reaction (`setMessageReaction`); inbound user reactions are not surfaced |
| sendPoll | ✅ | ❌ | |
| Cron/heartbeat topic targeting | ✅ | ❌ | |
| DM topics support | ✅ | ❌ | |
| Persistent ACP topic binding | ✅ | ❌ | |
| sendVoice (voice note replies) | ✅ | ❌ | Outbound attachments go out as `sendDocument` |
| Native quote replies + retry | ✅ | ❌ | |
| Polling stall watchdog + liveness | ✅ | ➖ | Webhook-only; no polling mode |
| HTML mode + chunking | ✅ | 🚧 | Plain text split at Telegram's 4096 UTF-16-unit limit; no HTML mode |
| Photo dimension preflight | ✅ | ❌ | |
| Webhook-mode setWebhook recovery | ✅ | ❌ | |

### Discord-Specific Features (since Feb 2025)

No Discord channel exists; every row below is out of scope.

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Forwarded attachment downloads | ✅ | 🚫 | |
| Faster reaction state machine | ✅ | 🚫 | |
| Thread parent binding inheritance | ✅ | 🚫 | |
| Persistent components/forms across restarts | ✅ | 🚫 | |
| `autoArchiveDuration` per-channel | ✅ | 🚫 | |
| Auto thread name generation | ✅ | 🚫 | |
| Voice channel responses | ✅ | 🚫 | |
| CJK reply chunking | ✅ | 🚫 | |

### Slack-Specific Features (since Feb 2025)

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Streaming draft replies | ✅ | ✅ | Stream `ReplySink` on Slack's native Agent API; workspaces without the Agents feature fail clearly (no fallback) |
| Configurable stream modes | ✅ | ❌ | |
| Thread ownership | ✅ | 🚧 | Slack threads bind to one canonical thread; no thread-level ownership tracking |
| Download-file action | ✅ | 🚧 | Inbound files are fetched at receive time through restricted egress; no on-demand action |
| App Home tab views | ✅ | ❌ | |
| Persistent thread participation | ✅ | 🚧 | Thread bindings are durable (`ironclaw_conversations`) |
| Block Kit limit hardening | ✅ | ❌ | |
| Socket Mode pong tuning | ✅ | ➖ | Events API webhooks only; no Socket Mode |
| Native model picker (`/models`) | ✅ | ❌ | |

### Mattermost-Specific Features (since Mar 2026)

No Mattermost channel exists; every row below is out of scope.

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Interactive buttons | ✅ | 🚫 | |
| Interactive model picker | ✅ | 🚫 | |
| `replyToMode` thread reply control | ✅ | 🚫 | |
| Streaming draft preview | ✅ | 🚫 | |
| WebSocket ping/pong keepalives | ✅ | 🚫 | |
| DM-vs-channel routing fixes | ✅ | 🚫 | |

### Feishu/Lark-Specific Features (since Mar 2026)

No Feishu/Lark channel exists; every row below is out of scope.

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Doc/table actions | ✅ | 🚫 | |
| Rich-text embedded media extraction | ✅ | 🚫 | |
| Native interactive cards | ✅ | 🚫 | |
| Schema 2.0 card action callbacks | ✅ | 🚫 | |
| Streaming cards | ✅ | 🚫 | |
| WebSocket retry/backoff | ✅ | 🚫 | |
| Voice-note transcription | ✅ | 🚫 | |
| Bitable placeholder cleanup | ✅ | 🚫 | |

### QQBot-Specific Features (since Mar 2026)

No QQBot channel exists; every row below is out of scope.

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Engine architecture rewrite | ✅ | 🚫 | |
| Group chat full support | ✅ | 🚫 | |
| C2C `stream_messages` | ✅ | 🚫 | |
| Chunked media upload | ✅ | 🚫 | |

### BlueBubbles-Specific Features (since Mar 2026)

No BlueBubbles/iMessage channel exists; every row below is out of scope.

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Persistent inbound GUID dedupe | ✅ | 🚫 | |
| Catchup replay | ✅ | 🚫 | |
| Reply-context API fallback | ✅ | 🚫 | |
| TTS opus-in-CAF voice memos | ✅ | 🚫 | |
| Per-group `systemPrompt` injection | ✅ | 🚫 | |
| Per-message catchup retry ceiling | ✅ | 🚫 | |

### Channel Features

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| DM pairing codes | ✅ | ✅ | Generated pairing codes minted from the WebUI (`/api/webchat/v2/extensions/{id}/pairing/mint`); no `pairing` CLI command |
| Allowlist/blocklist | ✅ | 🚧 | Admission is presence-based: Slack and Telegram serve any channel or group the bot was added to; each participant must be paired. No allow/deny lists |
| Self-message bypass | ✅ | ❌ | |
| Mention-based activation | ✅ | ✅ | Telegram group trigger policy keyed on `bot_username`; Slack app mentions |
| Per-group tool policies | ✅ | ❌ | |
| Thread isolation | ✅ | ✅ | One canonical thread per Slack thread / Telegram chat or topic |
| Per-channel media limits | ✅ | 🚧 | Shared attachment size ceilings in `ironclaw_attachments`; Telegram checks provider file size |
| Typing indicators | ✅ | 🚧 | Telegram run-status reaction; Slack Agent API status |
| Per-channel ackReaction config | ✅ | ❌ | |
| Group session priming | ✅ | ❌ | |
| Sender_id in trusted metadata | ✅ | ❌ | |
| Per-group `systemPrompt` injection | ✅ | ❌ | |
| Visible reply enforcement | ✅ | ❌ | |
| Active-run steering queue | ✅ | ✅ | Messages to a busy thread are queued as steering input for the active run (`ironclaw_assistant::steering`) |
| Tool-progress streaming into previews | ✅ | 🚧 | WebChat shows live tool activity; Slack streams reply revisions |
| `dmPolicy="open"` semantics | ✅ | ❌ | Admission requires pairing |

### Owner: _Unassigned_

---

## 4. CLI Commands

The `zlo` command set is `completion`, `config`, `devices`, `doctor`, `export`, `extension`, `import`, `ironhub`, `models`, `onboard`, `pair`, `profile`, `remote`, `repl`, `run`, `serve`, `service`, `skills`, `status`, `traces`, `transfer`, `update` (`crates/app/ironclaw_cli/src/commands/mod.rs`).

| Command | OpenClaw | IronClaw | Priority | Notes |
|---------|----------|----------|----------|-------|
| `run` (agent) | ✅ | ✅ | - | `zlo run` (`--message` single-shot, otherwise stdin loop); `zlo serve` runs the gateway |
| `tool install/list/remove` | ✅ | ✅ | - | `zlo extension search/install/remove`; `zlo ironhub` for signed registry packages |
| `gateway start/stop` | ✅ | ✅ | - | Through `zlo service start/stop` |
| `onboard` (wizard) | ✅ | ✅ | - | `zlo onboard` |
| `tui` | ✅ | ❌ | - | No TUI |
| `config` | ✅ | ✅ | - | `zlo config path/init/list/get/set` |
| `backup` | ✅ | 🚧 | - | `zlo export` / `zlo import` write and restore one encrypted setup file; `zlo transfer` moves it over SSH |
| `channels` | ✅ | ❌ | - | Channels are managed as extensions (`zlo extension`, WebUI Extensions page) |
| `models` | ✅ | ✅ | - | `zlo models list [--verbose] [--json]`, `status`, `set`, `set-provider` |
| `status` | ✅ | ✅ | - | `zlo status` (runtime readiness, service state) |
| `agents` | ✅ | ❌ | - | |
| `sessions` | ✅ | ❌ | - | |
| `memory` | ✅ | ❌ | - | No memory CLI; memory is reached through `ironclaw.memory.*` tools and the WebUI |
| `skills` | ✅ | 🚧 | - | `zlo skills list`; install/update/remove are model tools and WebUI actions |
| `pairing` | ✅ | ❌ | - | Channel pairing is WebUI-driven; `zlo pair` pairs devices, not channel users |
| `nodes` | ✅ | 🚧 | - | `zlo pair` / `zlo devices list/revoke` manage devices paired with the backend |
| `plugins` | ✅ | 🚧 | - | `zlo extension` / `zlo ironhub` |
| `hooks` | ✅ | ❌ | - | |
| `cron` | ✅ | ❌ | - | Automations are created by `builtin.trigger_*` tools and managed on the WebUI Automations page |
| `webhooks` | ✅ | ❌ | - | |
| `message send` | ✅ | ❌ | - | |
| `browser` | ✅ | ❌ | - | |
| `sandbox` | ✅ | ❌ | - | |
| `doctor` | ✅ | 🚧 | - | Checks home, profile, `config.toml`, `providers.json` |
| `logs` | ✅ | ❌ | - | Logs are an in-memory ring shown at `/api/webchat/v2/logs` and the WebUI Logs page; no CLI |
| `traces` | ➖ | ✅ | - | IronClaw-native Trace Commons client (not an OpenClaw parity feature): `opt-in`, `opt-out`, `enroll-instance`, `status`, `preview`, `enqueue`, `flush-queue`, `queue-status`, `credit`, `submit`, `list-submissions`, `revoke`, `ingest-health`, `profile` |
| `update` | ✅ | ✅ | - | `zlo update` rebuilds from the source checkout |
| `completion` | ✅ | ✅ | - | |
| `migrate` | ✅ | 🚧 | - | Claude Code and Codex terminal sessions can be imported as threads (`ironclaw_orchestrator::session_imports`); no config/credential importers |
| `proxy validate` | ✅ | ❌ | - | |
| `plugins registry` | ✅ | ❌ | - | |
| `plugins deps` | ✅ | ❌ | - | |
| `infer model run --gateway` | ✅ | ❌ | - | |
| `infer image describe`/`describe-many` | ✅ | ❌ | - | |
| `qa` (suite/telegram/credentials) | ✅ | ❌ | - | |
| `voicecall setup`/`smoke`/`continue` | ✅ | 🚫 | - | No voice channel |
| `googlemeet doctor`/`recover-tab` | ✅ | 🚫 | - | No Meet channel |
| `matrix verify`/`encryption setup` | ✅ | 🚫 | - | No Matrix channel |
| `nodes remove` | ✅ | 🚧 | - | `zlo devices revoke` |
| `nodes list` (paired view) | ✅ | 🚧 | - | `zlo devices` |
| `cron run` / `cron edit --thread-id` | ✅ | ❌ | - | Run-now exists as `builtin.trigger_run` and the WebUI automation "run" action, not as CLI |
| `sessions export-trajectory` | ✅ | ❌ | - | |
| `/subagents spawn` | ✅ | ❌ | - | |
| `/export-session` | ✅ | ❌ | - | |
| `/export-trajectory` (chat) | ✅ | ❌ | - | |
| `/diagnostics` (owner-only) | ✅ | ❌ | - | |
| `/codex computer-use status/install` | ✅ | ❌ | - | |
| `/dock-*` route switches | ✅ | ❌ | - | |
| `--container` / `OPENCLAW_CONTAINER` | ✅ | ❌ | - | |

Trace Commons: the client in this tree is `crates/domains/ironclaw_trace_commons` (envelope schema, deterministic local redaction, submission queue, credits, device-key onboarding, autonomous capture), the `zlo traces` command and the `builtin.trace_commons.*` tools. The ingest/review/export server is not in this repository.

### Owner: _Unassigned_

---

## 5. Agent System

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Pi agent runtime | ✅ | ➖ | IronClaw uses its own loop (`ironclaw_agent_loop`, `ironclaw_turn_runner`) |
| RPC-based execution | ✅ | ✅ | Capabilities run through `CapabilityHost` and runtime lanes (WASM, MCP, sandbox, first-party) |
| Multi-provider failover | ✅ | ✅ | `FailoverProvider` in `ironclaw_llm` |
| Per-sender sessions | ✅ | ✅ | |
| Global sessions | ✅ | ❌ | |
| Session pruning | ✅ | ➖ | LLM data is never deleted by design |
| Context compaction | ✅ | ✅ | `ironclaw_loop_host::compaction_task`, with sanitization of the summarized span |
| Compaction model override | ✅ | ❌ | |
| Compaction mid-turn precheck | ✅ | ❌ | |
| Post-compaction read audit | ✅ | ❌ | |
| Post-compaction context injection | ✅ | ❌ | |
| Compaction start/end notices | ✅ | ❌ | |
| Custom system prompts | ✅ | ✅ | Boot-seeded `SYSTEM.md` in the home; prompt assets under `crates/loop/ironclaw_loop_host/prompts/` |
| Skills (modular capabilities) | ✅ | ✅ | `SKILL.md` skills with trust levels, activation criteria and deterministic selection (`ironclaw_skills`) |
| Skill Workshop plugin | ✅ | 🚧 | `ironclaw_skills::learning` distills and refines skills from runs |
| Grouped skill directories | ✅ | ❌ | |
| Skill installer metadata | ✅ | 🚧 | Install-metadata records (`install_metadata`); no npm/pip recipes |
| Skill routing blocks | ✅ | 🚧 | `ActivationCriteria` (keywords, patterns, tags); no "Use when / Don't use when" blocks |
| Skill path compaction | ✅ | ❌ | |
| Thinking modes (off/minimal/low/medium/high/xhigh/adaptive/max) | ✅ | 🚧 | Reasoning effort `low`/`medium`/`high`/`xhigh` mapped per provider (`ironclaw_llm::reasoning_effort`); Anthropic thinking in `anthropic_thinking.rs` |
| Per-model thinkingDefault override | ✅ | ❌ | |
| Adaptive→provider thinking maps | ✅ | ❌ | |
| Native Codex app-server runtime | ✅ | ✅ | Codex runs as an orchestrator turn runtime (`ironclaw_cli_providers::codex`), alongside Claude Code |
| Codex Computer Use | ✅ | ➖ | IronClaw has its own computer use (`ironclaw_computer_use`, Open Computer Use over stdio MCP) |
| Codex hooks bridge | ✅ | 🚧 | No native Codex hook bridge. Codex app-server approval requests (`item/commandExecution/requestApproval`, `item/fileChange/requestApproval`) are bridged into IronClaw approvals in `ironclaw_cli_providers::codex`; the PreToolUse hook bridge belongs to the Claude adapter (`ironclaw_cli_providers::claude`) |
| Codex sub-agent metadata | ✅ | ❌ | |
| Codex context-engine integration | ✅ | ❌ | |
| Active Memory plugin | ✅ | 🚧 | With a Supermemory key, recall runs automatically before each turn (`ironclaw_supermemory`) |
| Inferred follow-up commitments | ✅ | ❌ | |
| `sessions_yield` | ✅ | ❌ | |
| Subagent forked context | ✅ | ❌ | |
| `agents.defaults.contextInjection: "never"` | ✅ | ❌ | |
| `agents.defaults.experimental.localModelLean` | ✅ | ❌ | |
| `agents.files.get/set` workspace tools | ✅ | ✅ | `builtin.read_file`, `write_file`, `list_dir`, `glob`, `grep`, `apply_patch`, `document_edit` |
| Trajectory export | ✅ | 🚧 | Run and thread artifact export views (`ironclaw.run_artifact.v1`, `ironclaw.thread_artifact.v1`) with redaction; admin thread-scrape routes |
| Block-level streaming | ✅ | ✅ | Reply revisions stream to WebChat and Slack |
| Tool-level streaming | ✅ | 🚧 | Live tool activity in WebChat; no streamed tool output |
| Z.AI tool_stream | ✅ | ❌ | |
| Plugin tools | ✅ | ✅ | WASM tools, hosted MCP tools, first-party tools |
| GSuite WASM tools | ✅ | ✅ | Gmail, Calendar (first-party executors), Drive, Docs, Sheets, Slides (WASM) packages with Google OAuth |
| Hosted MCP extensions | ✅ | ✅ | `ironclaw_mcp` lane; bundled `notion-mcp`; WebUI `register-hosted-mcp` |
| NEAR AI MCP extension | ✅ | ✅ | `nearai-mcp` package, server patched from operator LLM config |
| Tool policies (allow/deny) | ✅ | ✅ | Per-tool permission settings and persistent `AlwaysAllow` approval policy |
| Exec approvals (`/approve`) | ✅ | ✅ | Click-approval gates in WebChat (`ApprovalInteractionService`); profiles are "ask first" or "full access" |
| Tool inventory cache | ✅ | ❌ | |
| Pending exec approval `errorMessage` cleanup | ✅ | ❌ | |
| Elevated mode | ✅ | ❌ | |
| Subagent support | ✅ | ✅ | `builtin.spawn_subagent` exists but is disabled by default on purpose; delegation to other runtimes goes through `ironclaw_orchestrator` |
| `/subagents spawn` command | ✅ | ❌ | |
| Auth profiles | ✅ | 🚧 | Profiles in `ironclaw_orchestrator::profiles`; no multi-strategy auth profiles |
| Generic API key rotation | ✅ | ❌ | |
| Stuck loop detection | ✅ | ❌ | |
| llms.txt discovery | ✅ | ❌ | |
| Multiple images per tool call | ✅ | ❌ | |
| Web search extension | ✅ | ✅ | `web-access` package: `web-access.search` (Exa MCP) and `web-access.get_content`; Brave backend rejected as unsupported |
| URL allowlist (web_search/fetch) | ✅ | ✅ | All egress goes through `ironclaw_network` policy with private-address denial |
| suppressToolErrors config | ✅ | ❌ | |
| Intent-first tool display | ✅ | ❌ | |
| Transcript file size in status | ✅ | ❌ | |
| Stuck-session recovery | ✅ | 🚧 | Orchestrator restart and usage-limit recovery (`ironclaw_orchestrator::recovery`) |
| `Runner:` in `/status` | ✅ | ❌ | |
| Voice Wake routing | ✅ | ❌ | |

### Owner: _Unassigned_

---

## 6. Model & Provider Support

Providers come from `crates/domains/ironclaw_llm/assets/providers.json` plus the dedicated backends in `LlmBackendKind` (`nearai`, `bedrock`, `gemini_oauth`, `openai_codex`).

| Provider | OpenClaw | IronClaw | Priority | Notes |
|----------|----------|----------|----------|-------|
| NEAR AI | ✅ | ✅ | - | Dedicated backend, including NEAR AI login |
| Anthropic (Claude) | ✅ | ✅ | - | Native `anthropic` protocol, plus Anthropic OAuth (`anthropic_oauth`) |
| OpenAI | ✅ | ✅ | - | Native `openai`; Codex/ChatGPT login via `openai_codex` |
| OpenAI Codex (native app-server) | ✅ | ✅ | - | Codex CLI runtime in `ironclaw_cli_providers::codex` |
| AWS Bedrock | ✅ | ✅ | - | Dedicated backend (`bedrock.rs`) |
| Google Gemini | ✅ | ✅ | - | Native `gemini` protocol and `gemini_oauth` |
| Google Gemini Live (realtime) | ✅ | ❌ | - | |
| io.net | ✅ | ✅ | - | `ionet` |
| Mistral | ✅ | ✅ | - | `mistral` |
| Yandex AI Studio | ✅ | ✅ | - | `yandex` |
| Cloudflare Workers AI | ✅ | ✅ | - | `cloudflare` |
| NVIDIA API | ✅ | ✅ | - | `nvidia` |
| OpenRouter | ✅ | ✅ | - | `openrouter` |
| Tinfoil | ❌ | ✅ | - | `tinfoil` |
| OpenAI-compatible | ❌ | ✅ | - | `openai_compatible`, plus `lmstudio` |
| GitHub Copilot | ✅ | ✅ | - | Dedicated provider with OAuth token exchange |
| Ollama (local) | ✅ | ✅ | - | `ollama`, plus `ollama_cloud` |
| Perplexity | ✅ | ❌ | - | |
| MiniMax | ✅ | ✅ | - | `minimax` (chat only) |
| GLM-5 | ✅ | ✅ | - | Z.AI provider `zai` |
| Tencent Cloud (TokenHub) | ✅ | ❌ | - | |
| DeepInfra | ✅ | ✅ | - | `deepinfra` (chat only) |
| Cerebras | ✅ | ✅ | - | `cerebras` |
| Z.AI / GLM-5 | ✅ | ✅ | - | `zai` |
| Qwen / Model Studio | ✅ | ✅ | - | `dashscope` |
| DeepSeek | ✅ | ✅ | - | `deepseek` |
| Moonshot / Kimi | ✅ | ✅ | - | `moonshot` |
| xAI | ✅ | ✅ | - | `xai` (chat only) |
| Tencent Yuanbao | ✅ | ❌ | - | |
| Vercel AI Gateway | ✅ | ✅ | - | `vercel_ai_gateway` |
| Codex/OpenAI image generation | ✅ | ❌ | - | |
| OpenRouter image/video generation | ✅ | ❌ | - | |
| MiniMax music/video | ✅ | ❌ | - | |
| Google Veo (video gen) | ✅ | ❌ | - | |
| fal Seedance 2.0 | ✅ | ❌ | - | |
| Comfy (image/video/music) | ✅ | ❌ | - | |
| node-llama-cpp | ✅ | ➖ | - | |
| llama.cpp (native) | ❌ | ❌ | - | Reachable only through OpenAI-compatible servers |

Also in the registry with no OpenClaw row: Groq, Venice, Together, Fireworks, SambaNova, Hugging Face, Nebius, Novita, StepFun, Arcee, Upstage, Inception, GMI, Kilo Code, OpenCode Zen, Xiaomi, Meta.

### Model Features

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Auto-discovery | ✅ | 🚧 | `/api/webchat/v2/llm/list-models` fetches a provider's model list; static catalog in `providers.json` |
| Failover chains | ✅ | ✅ | `FailoverProvider` over explicit routes |
| Cooldown management | ✅ | ✅ | Per-provider cooldown in `FailoverProvider` |
| Per-session model override | ✅ | ✅ | User model preference and per-task models (`/api/webchat/v2/llm/model-preference`, `task-models`) |
| Model selection UI | ✅ | ✅ | WebUI Settings → Inference |
| Per-model thinkingDefault | ✅ | ❌ | |
| 1M context support | ✅ | ❌ | |
| Fast mode (`/fast`) | ✅ | ❌ | |
| Tiered model pricing | ✅ | ❌ | |
| `models scan` (free-model fallback) | ✅ | ❌ | |
| Model catalog stale cache fallback | ✅ | ❌ | |
| `models.pricing.enabled` | ✅ | ❌ | |
| Auth status card | ✅ | 🚧 | Provider login status in Settings; subscription usage windows on the Usage page (`ironclaw_usage`) |
| Model fallback metadata | ✅ | ❌ | |
| `prompt_cache_key` opt-in | ✅ | ❌ | |
| Replay normalization | ✅ | ❌ | |

### TTS / STT / Realtime Voice

`ironclaw_llm` contains speech providers (`speech/`: OpenAI and ElevenLabs) and transcription providers (`transcription/`: OpenAI Whisper and a chat-completions transcriber). No backend product path calls them today, so they count as partial.

| Feature | OpenClaw | IronClaw | Priority | Notes |
|---------|----------|----------|----------|-------|
| TTS (Microsoft / Edge) | ✅ | ❌ | - | |
| TTS (OpenAI) | ✅ | 🚧 | - | `OpenAiSpeechProvider`, not wired |
| TTS (ElevenLabs v3) | ✅ | 🚧 | - | `ElevenLabsSpeechProvider`, not wired |
| TTS (Google Gemini) | ✅ | ❌ | - | |
| TTS (Azure Speech) | ✅ | ❌ | - | |
| TTS (Inworld) | ✅ | ❌ | - | |
| TTS (Volcengine/BytePlus Seed Speech) | ✅ | ❌ | - | |
| TTS (Xiaomi MiMo) | ✅ | ❌ | - | |
| TTS (Local CLI) | ✅ | ❌ | - | |
| TTS (Gradium) | ✅ | ❌ | - | |
| TTS (OpenRouter) | ✅ | ❌ | - | |
| TTS (xAI) | ✅ | ❌ | - | |
| TTS (DeepInfra) | ✅ | ❌ | - | |
| TTS (MiniMax) | ✅ | ❌ | - | |
| TTS (Tinfoil/local MLX) | ✅ | ❌ | - | |
| TTS personas | ✅ | ❌ | - | |
| Auto-TTS controls | ✅ | ❌ | - | |
| Talk Mode (browser realtime) | ✅ | ❌ | - | |
| STT (OpenAI Realtime) | ✅ | ❌ | - | Batch Whisper only (`OpenAiWhisperProvider`, not wired) |
| STT (xAI realtime) | ✅ | ❌ | - | |
| STT (Deepgram) | ✅ | ❌ | - | |
| STT (ElevenLabs Scribe v2) | ✅ | ❌ | - | |
| STT (Mistral) | ✅ | ❌ | - | |
| STT (SenseAudio) | ✅ | ❌ | - | |
| STT (local Whisper CLI) | ✅ | ❌ | - | |

### Owner: _Unassigned_

---

## 7. Media Handling

| Feature | OpenClaw | IronClaw | Priority | Notes |
|---------|----------|----------|----------|-------|
| Image processing (Sharp) | ✅ | ❌ | - | |
| Configurable image resize dims | ✅ | ❌ | - | |
| Multiple images per tool call | ✅ | ❌ | - | |
| Audio transcription | ✅ | 🚧 | - | Providers exist in `ironclaw_llm::transcription`; not wired |
| Video support | ✅ | ❌ | - | |
| PDF analysis tool | ✅ | ❌ | - | |
| PDF parsing | ✅ | ✅ | - | `ironclaw_extractors` (PDF, OOXML, legacy Office, RTF, text), bomb-capped |
| MIME detection | ✅ | 🚧 | - | Attachment landing and channel adapters classify media types |
| Media caching | ✅ | ❌ | - | |
| Vision model integration | ✅ | ✅ | - | Image attachments go to vision-capable models (`ironclaw_llm::vision_models`) |
| Image generation | ✅ | ❌ | - | |
| Music generation | ✅ | ❌ | - | |
| Multimodal memory indexing | ✅ | ❌ | - | |
| Audio-as-voice routing | ✅ | ❌ | - | |
| TTS providers | ✅ | 🚧 | - | See the TTS/STT subsection in Section 6 |
| Incremental TTS playback | ✅ | ❌ | - | |
| Sticker-to-image | ✅ | ❌ | - | |
| Per-channel media limits | ✅ | 🚧 | - | Shared ceilings in `ironclaw_attachments` |
| HTML to PDF | ❌ | ✅ | - | `builtin.html_to_pdf` |

### Owner: _Unassigned_

---

## 8. Plugin & Extension System

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Dynamic loading | ✅ | ✅ | Native, WASM and MCP loaders in `ironclaw_extension_host` |
| Manifest validation | ✅ | ✅ | `reborn.extension_manifest.v3`, compiled to a resolved, digested record (`ironclaw_extension_registry`) |
| HTTP path registration | ✅ | 🚧 | Channels get host-owned webhook routes from their manifest; extensions cannot register arbitrary routes |
| Workspace-relative install | ✅ | ✅ | Installation records under the ZLO home (`~/.zlo`) |
| Channel plugins | ✅ | ✅ | Channel surface of an extension manifest + `ChannelIngress`/`ChannelDelivery`/`ReplySink` |
| Auth plugins | ✅ | ✅ | Manifest `[auth.*]` recipes run by the `ironclaw_auth` engine (OAuth, device link, manual token) |
| Memory plugins | ✅ | ✅ | `[memory]` provider surface: `memory-native` (default) or `mem0`; Supermemory when a key is set |
| Context-engine plugins | ✅ | ❌ | |
| Tool plugins | ✅ | ✅ | WASM, MCP and first-party tool surfaces |
| Hook plugins | ✅ | 🚧 | Extension `[[hooks]]` load at the `Installed` tier, but hook activation is off by default and no bundled package declares hooks |
| Provider plugins | ✅ | ❌ | LLM providers are data in `providers.json`, not extensions |
| Plugin CLI (`install`, `list`) | ✅ | ✅ | `zlo extension search/install/remove`, `zlo ironhub` |
| ClawHub registry | ✅ | 🚧 | IronHub signed registry (`zlo ironhub`, `/api/webchat/v2/ironhub/install`) |
| `git:` plugin installs | ✅ | ❌ | |
| `before_agent_start` hook | ✅ | ❌ | |
| `before_agent_finalize` hook | ✅ | ❌ | |
| `before_message_write` hook | ✅ | ❌ | |
| `before_dispatch` hook | ✅ | ❌ | |
| `before_compaction`/`after_compaction` hooks | ✅ | ❌ | |
| `llm_input`/`llm_output` hooks | ✅ | 🚧 | `BeforePrompt` and `AfterModel` hook points in `ironclaw_hooks` |
| `model_call_started`/`ended` hooks | ✅ | ❌ | |
| `cron_changed` hook | ✅ | ❌ | |
| `gateway_start` hook context | ✅ | ❌ | |
| `agent_end` observation hooks | ✅ | 🚧 | `AfterTurn` hook point (privileged tiers only; used by memory curation) |
| Plugin SDK state store | ✅ | ❌ | |
| Plugin SDK Codex extensions | ✅ | ❌ | |
| Persisted plugin registry | ✅ | ✅ | Durable installation records in `ironclaw_extension_registry` |
| `plugins deps --repair` | ✅ | ❌ | |
| Plugin install conflict-aware writes | ✅ | ❌ | |
| Plugin compatibility registry | ✅ | ❌ | |
| Layered runtime-deps roots | ✅ | ❌ | |
| Bundled provider catalogs in manifest | ✅ | ❌ | |

### Owner: _Unassigned_

---

## 9. Configuration System

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Primary config file | ✅ `~/.openclaw/openclaw.json` | ✅ `~/.zlo/config.toml` | Plus `providers.json` and encrypted secrets; environment variables documented in `.env.example` |
| JSON5 support | ✅ | ❌ | |
| YAML alternative | ✅ | ❌ | |
| Environment variable interpolation | ✅ | ❌ | Environment variables are read directly; no `${VAR}` interpolation in `config.toml` |
| Config validation/schema | ✅ | ✅ | Typed `config.toml` schema with inline-secret rejection (`ironclaw_config`) |
| Hot-reload | ✅ | 🚧 | LLM provider config only |
| Legacy migration | ✅ | 🚧 | `~/.ironclaw/reborn` moves to `~/.zlo` on first start; retired config sections are rejected with a pointer |
| State directory | ✅ `~/.openclaw-state/` | ✅ `~/.zlo/` | `ZLO_HOME` overrides |
| Credentials directory | ✅ | ✅ | Encrypted secret store (`ironclaw_secrets`) |
| Full model compat fields in schema | ✅ | ❌ | |
| `models.pricing.enabled` | ✅ | ❌ | |
| `agents.list[].contextTokens` | ✅ | ❌ | |
| `gateway.handshakeTimeoutMs` | ✅ | ➖ | No WebSocket handshake |
| `--profile <name>` | ✅ | 🚧 | Boot profiles (`zlo profile list`); not used for plugin install paths |
| Config recovery on clobber | ✅ | ❌ | |
| Modular `$include` files | ✅ | ❌ | |
| `config set --merge`/`--replace` | ✅ | ❌ | |
| Wrapper-based service install | ✅ | 🚧 | `zlo service install` writes launchd/systemd units running `serve`; no custom wrapper option |

### Owner: _Unassigned_

---

## 10. Memory & Knowledge System

The default `memory-native` provider is filesystem-backed with full-text search only: no embeddings, vector or hybrid lane. `mem0` and Supermemory are external services.

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Vector memory | ✅ | 🚧 | Only through the external `mem0` provider or Supermemory |
| Session-based memory | ✅ | ✅ | |
| Hybrid search (BM25 + vector) | ✅ | ❌ | Native provider is full-text only |
| Temporal decay (hybrid search) | ✅ | ❌ | |
| MMR re-ranking | ✅ | ❌ | |
| LLM-based query expansion | ✅ | ❌ | |
| OpenAI embeddings | ✅ | ❌ | No embedding provider in the tree |
| Bedrock embeddings | ❌ | ❌ | |
| Gemini embeddings | ✅ | ❌ | |
| GitHub Copilot embeddings | ✅ | ❌ | |
| Ollama embeddings | ✅ | ❌ | `mem0` can be configured to use one on its side |
| Local embeddings | ✅ | ❌ | |
| Asymmetric embedding endpoints | ✅ | ❌ | |
| SQLite-vec backend | ✅ | ❌ | |
| LanceDB backend | ✅ | ❌ | |
| QMD backend | ✅ | ❌ | |
| Active Memory plugin | ✅ | 🚧 | Supermemory automatic recall/capture when configured |
| Memory wiki (people-aware) | ✅ | ❌ | |
| Dreaming (REM cycles) | ✅ | ❌ | |
| `recallMaxChars` cap | ✅ | ❌ | |
| `corpus=sessions` ranking | ✅ | 🚧 | Thread history search at `/api/webchat/v2/history/search` |
| Atomic reindexing | ✅ | ❌ | |
| Embeddings batching | ✅ | ❌ | |
| Citation support | ✅ | ❌ | |
| Memory CLI commands | ✅ | ❌ | Memory tools (`ironclaw.memory.read/write/search/tree/profile_set`) and the WebUI memory page instead |
| `openclaw ltm list` | ✅ | ❌ | |
| Flexible path structure | ✅ | ✅ | `/memory` path grammar (`MemoryDocumentPath`) |
| Identity files (AGENTS.md, etc.) | ✅ | ✅ | `MEMORY.md` served at the head of long-term recall; protected identity paths in the write-safety policy |
| Daily logs | ✅ | ❌ | |
| Heartbeat checklist | ✅ | ❌ | `HEARTBEAT.md` is only a protected path; no heartbeat runner |
| Hybrid post-compaction reindex | ✅ | ❌ | |

### Owner: _Unassigned_

---

## 11. Mobile Apps

| Feature | OpenClaw | IronClaw | Priority | Notes |
|---------|----------|----------|----------|-------|
| iOS app (SwiftUI) | ✅ | 🚫 | - | |
| Android app (Kotlin) | ✅ | 🚫 | - | |
| Apple Watch companion | ✅ | 🚫 | - | |
| Gateway WebSocket client | ✅ | 🚫 | - | |
| Camera/photo access | ✅ | 🚫 | - | |
| Voice input | ✅ | 🚫 | - | |
| Push-to-talk | ✅ | 🚫 | - | |
| Location sharing | ✅ | 🚫 | - | |
| Node pairing | ✅ | 🚫 | - | |
| APNs push notifications | ✅ | 🚫 | - | |
| Share to OpenClaw (iOS) | ✅ | 🚫 | - | |
| Background listening toggle | ✅ | 🚫 | - | |

### Owner: _Unassigned_ (if ever prioritized)

---

## 12. macOS App

The ZLO desktop app lives in `tools/ironclaw_rune/` (Rune, separate Cargo workspace) and is outside this matrix, which covers `crates/` only. The rows below compare OpenClaw's SwiftUI app with the backend.

| Feature | OpenClaw | IronClaw | Priority | Notes |
|---------|----------|----------|----------|-------|
| SwiftUI native app | ✅ | 🚫 | - | |
| Menu bar presence | ✅ | 🚫 | - | |
| Bundled gateway | ✅ | 🚫 | - | |
| Canvas hosting | ✅ | 🚫 | - | |
| Voice wake | ✅ | 🚫 | - | |
| Voice wake overlay | ✅ | 🚫 | - | |
| Push-to-talk hotkey | ✅ | 🚫 | - | |
| Exec approval dialogs | ✅ | ✅ | - | Approval gates exposed by the gateway and rendered in WebChat |
| iMessage integration | ✅ | 🚫 | - | |
| Instances tab | ✅ | 🚫 | - | |
| Agent events debug window | ✅ | ✅ | - | Operator-only run inspector (prompt, activity, tool details) |
| Sparkle auto-updates | ✅ | 🚫 | - | |

### Owner: _Unassigned_ (if ever prioritized)

---

## 13. Web Interface

| Feature | OpenClaw | IronClaw | Priority | Notes |
|---------|----------|----------|----------|-------|
| Control UI Dashboard | ✅ | ✅ | - | React SPA: chat, settings, extensions, automations, logs, workspace, projects, admin, onboarding |
| Channel status view | ✅ | 🚧 | - | Channels appear as extensions with connection state |
| Agent management | ✅ | ❌ | - | |
| Model selection | ✅ | ✅ | - | Settings → Inference: providers, model policy, per-user preference |
| Config editing | ✅ | 🚧 | - | Operator config keys and tool permissions; no raw config editor |
| Debug/logs viewer | ✅ | ✅ | - | Logs page with level/target and run/thread/turn/tool filters |
| WebChat interface | ✅ | ✅ | - | Served at `/chat`; events over SSE |
| Canvas system (A2UI) | ✅ | ❌ | - | |
| Control UI i18n | ✅ | ✅ | - | 11 locales: ar, de, en, es, fr, hi, ja, ko, pt-BR, uk, zh-CN |
| WebChat theme sync | ✅ | 🚧 | - | Follows the system theme on first load; afterwards an explicit light/dark choice |
| Partial output on abort | ✅ | ❌ | - | |
| PWA + Web Push | ✅ | ✅ | - | Service worker + `web-app` Web Push channel |
| Talk Mode (browser realtime voice) | ✅ | ❌ | - | |
| Steer queued messages | ✅ | ✅ | - | Composer stays enabled during a run; follow-ups are queued as steering input |
| Quick Settings dashboard | ✅ | ❌ | - | |
| Markdown preview dialog | ✅ | ❌ | - | Chat messages render markdown; no preview dialog |
| Cron job dashboard | ✅ | ✅ | - | Automations page: list, run, pause, resume, rename, delete, run history |
| Personal identity (operator) | ✅ | ❌ | - | |
| Trajectory export UI | ✅ | 🚧 | - | Run/thread artifact export routes |
| Restart-impacting Dreaming confirm | ✅ | ❌ | - | |
| Mobile chat settings sheet | ✅ | ❌ | - | |

### Owner: _Unassigned_

---

## 14. Automation

Reborn automations are `ironclaw_triggers` records (`TriggerSchedule::Cron` with timezone, or `TriggerSchedule::Once`), fired by the composition-owned poller, created through `builtin.trigger_*` tools and managed on the WebUI Automations page. The orchestrator also runs webhook schedules (`ironclaw_orchestrator::schedule_webhooks`).

| Feature | OpenClaw | IronClaw | Priority | Notes |
|---------|----------|----------|----------|-------|
| Cron jobs | ✅ | ✅ | - | Cron triggers with timezone; sub-minute schedules rejected |
| Reborn scheduled trigger loop | ➖ | ✅ | - | Poller tick, deterministic fire identity, sealed trusted submission with a prompt-safety scan |
| Per-job model fallback override | ✅ | ❌ | - | |
| Cron stagger controls | ✅ | ❌ | - | |
| Cron finished-run webhook | ✅ | ❌ | - | |
| `--thread-id` cron CLI | ✅ | ❌ | - | |
| `failureAlert.includeSkipped` | ✅ | ❌ | - | |
| `delivery.threadId` (gateway cron schemas) | ✅ | 🚧 | - | Results can be delivered to a channel target through outbound delivery |
| Cron `nested` lane | ✅ | ❌ | - | |
| Cron stuck-session timeout | ✅ | ❌ | - | |
| Timezone support | ✅ | ✅ | - | IANA timezone on every schedule |
| One-shot/recurring jobs | ✅ | ✅ | - | `TriggerSchedule::Once` and cron; manual run-now |
| Channel health monitor | ✅ | ❌ | - | |
| `beforeInbound` hook | ✅ | ❌ | - | |
| `beforeOutbound` hook | ✅ | ❌ | - | |
| `beforeToolCall` hook | ✅ | 🚧 | - | `BeforeCapability` hook point; hook activation is off by default |
| `before_agent_start` hook | ✅ | ❌ | - | |
| `before_agent_finalize` hook | ✅ | ❌ | - | |
| `before_message_write` hook | ✅ | ❌ | - | |
| `before_dispatch` hook | ✅ | ❌ | - | |
| `before_compaction`/`after_compaction` | ✅ | ❌ | - | |
| `onMessage` hook | ✅ | ❌ | - | |
| Structured system-event routines | ✅ | ❌ | - | Trigger sources are `Schedule` and `Manual` only |
| `onSessionStart` hook | ✅ | ❌ | - | |
| `onSessionEnd` hook | ✅ | 🚧 | - | `AfterTurn` fires once per terminal run |
| `transcribeAudio` hook | ✅ | ❌ | - | |
| `transformResponse` hook | ✅ | ❌ | - | |
| `llm_input`/`llm_output` hooks | ✅ | 🚧 | - | `BeforePrompt` / `AfterModel` hook points |
| `model_call_started`/`ended` hooks | ✅ | ❌ | - | |
| `cron_changed` hook | ✅ | ❌ | - | |
| Cron `jobId` hook context | ✅ | ❌ | - | |
| Bundled hooks | ✅ | ❌ | - | The first-party builtin hook catalog is deliberately empty |
| Plugin hooks | ✅ | 🚧 | - | Extension `[[hooks]]` at the `Installed` tier |
| Workspace hooks | ✅ | ❌ | - | |
| Outbound webhooks | ✅ | ❌ | - | |
| Heartbeat system | ✅ | ❌ | - | |
| Gmail pub/sub | ✅ | ❌ | - | |
| Inferred follow-up commitments | ✅ | ❌ | - | |
| Inbound webhook schedules | ❌ | ✅ | - | Each request to a schedule's loopback URL starts a run, with optional HMAC check |

### Owner: _Unassigned_

---

## 15. Security Features

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Gateway token auth | ✅ | ✅ | Bearer token (env or token file), signed session cookies |
| Device pairing | ✅ | ✅ | `zlo pair` one-time codes; `zlo devices list/revoke` |
| Tailscale identity | ✅ | ✅ | Tailscale identity listener for one-click pairing |
| Trusted-proxy auth | ✅ | ❌ | |
| OAuth flows | ✅ | ✅ | WebUI SSO (Google with PKCE, GitHub) and OIDC bearer tokens; product-auth OAuth for extensions; NEAR AI, Codex, Anthropic, Gemini and Copilot provider logins |
| DM pairing verification | ✅ | ✅ | Generated-code channel pairing |
| Allowlist/blocklist | ✅ | 🚧 | Pairing-gated, presence-based admission; no allow/deny lists |
| Per-group tool policies | ✅ | ❌ | |
| Exec approvals | ✅ | ✅ | Approval gates with durable `AlwaysAllow` policy; automatic approval off by default |
| Owner allowlists | ✅ | ❌ | |
| TLS 1.3 minimum | ✅ | ❌ | `ironclaw_network` uses rustls defaults (TLS 1.2+); no TLS 1.3 floor is set |
| SSRF protection | ✅ | ✅ | `ironclaw_network`: private-address denial after DNS resolution, redirect hardening |
| SSRF IPv6 transition bypass block | ✅ | ✅ | IPv4-mapped IPv6 addresses are unwrapped before the check (`policy.rs`) |
| Cron webhook SSRF guard | ✅ | ➖ | No outbound cron webhooks |
| Loopback-first | ✅ | ✅ | Default bind `127.0.0.1`; non-loopback binds need a matching profile |
| Docker sandbox | ✅ | ✅ | `ironclaw_sandbox`: validated `SandboxProcessPlan` run in a container with managed egress; served multi-user deployments must use it for shell |
| Podman support | ✅ | ❌ | |
| WASM sandbox | ❌ | ✅ | `ironclaw_wasm` lane with `ironclaw_wasm_limiter` |
| Sandbox env sanitization | ✅ | ✅ | Plans reject sensitive raw env values; brokered credentials use placeholders |
| `OPENCLAW_*` env block | ✅ | ➖ | |
| Workspace `.env` injection blocks | ✅ | ❌ | |
| Tool policies | ✅ | ✅ | |
| Elevated mode | ✅ | ❌ | |
| Safe bins allowlist | ✅ | ❌ | |
| LD*/DYLD* validation | ✅ | 🚧 | Sandbox plan validation covers loader variables; host-process shell does not |
| Path traversal prevention | ✅ | ✅ | `ScopedFilesystem` containment; traversal rejected on fs routes |
| Credential theft via env injection | ✅ | 🚧 | Credentials stay host-side and are injected at mediated egress; output is scrubbed |
| Session file permissions (0o600) | ✅ | ✅ | Token and secret files written with mode 0o600 |
| Skill download path restriction | ✅ | 🚧 | Skills install into scoped mounts (`ironclaw_skills::scoped_management`) |
| Skill installer metadata validation | ✅ | 🚧 | `ironclaw_skills::validation` |
| Webhook signature verification | ✅ | ✅ | Host runs each channel's manifest-declared verification recipe |
| Media URL validation | ✅ | 🚧 | Attachment fetches go through restricted egress with path-traversal and size checks |
| Prompt injection defense | ✅ | ✅ | `ironclaw_safety` sanitizer and trusted-trigger prompt validation |
| Internal scaffolding stripping | ✅ | ❌ | |
| Leak detection | ✅ | ✅ | `ironclaw_safety::leak_detector` on process output and model-visible results |
| Dangerous tool re-enable warning | ✅ | ❌ | |
| OpenGrep static analysis | ✅ | ❌ | |
| Logging redaction expansion | ✅ | 🚧 | Redaction obligations on events, logs and results |
| Trace context propagation | ✅ | ❌ | |
| Forwarded-header IP detection | ✅ | 🚧 | Rate limiting uses the peer address and never trusts forwarded headers |
| Trusted-content sanitization | ✅ | 🚧 | Prompt envelopes keep untrusted channel content marked as untrusted |
| Per-tool MCP loopback policy | ✅ | ❌ | |
| Mobile pairing TLS requirement | ✅ | ➖ | |
| Webhook auth rate-limit | ✅ | 🚧 | Gateway-wide per-IP rate limiting |

### Owner: _Unassigned_

---

## 16. Development & Build System

| Feature | OpenClaw | IronClaw | Notes |
|---------|----------|----------|-------|
| Primary language | TypeScript | Rust | |
| Build tool | tsdown | cargo | |
| Type checking | TypeScript/tsgo | rustc | |
| Linting | Oxlint | clippy | |
| Formatting | Oxfmt | rustfmt | |
| Package manager | pnpm | cargo | pnpm for the WebUI frontend |
| Test framework | Vitest | built-in | Vitest for the WebUI frontend |
| Coverage | V8 | llvm-cov | |
| CI/CD | GitHub Actions | GitHub Actions | |
| Pre-commit hooks | prek | ✅ | `scripts/pre-commit-safety.sh` |
| Docker: Chromium + Xvfb | ✅ | ❌ | |
| Docker: init scripts | ✅ | ❌ | |
| Browser: extraArgs config | ✅ | ❌ | |

### Owner: _Unassigned_

---

## Implementation Priorities

### Done

- ✅ WebChat v2 gateway and SPA, streamed over SSE
- ✅ Slack channel (Events API, native Agent API streaming) and Telegram channel (Bot API webhook, generated-code pairing)
- ✅ Web Push channel (`web-app`)
- ✅ OpenAI-compatible API (`/v1/chat/completions`, `/v1/models`, `/v1/responses`)
- ✅ Extension model with WASM, MCP and first-party runtimes; GitHub, Google Workspace, Notion, NEAR AI and web-access packages
- ✅ Approval gates with persistent tool policy
- ✅ Context compaction
- ✅ Skills (parsing, trust levels, deterministic selection)
- ✅ Scheduled automations (cron and one-shot triggers)
- ✅ Multi-provider LLM registry with failover and cooldown
- ✅ Docker sandbox lane and WASM sandbox
- ✅ Network egress policy, leak detection, prompt-injection defense
- ✅ Device pairing, Tailscale remote access, launchd/systemd service install
- ✅ Orchestrator delegation to Codex and Claude Code CLI runtimes
- ✅ Computer use (`ironclaw_computer_use`)

### Partial

- 🚧 Hooks: framework and extension loader exist, activation is off by default, no bundled hooks
- 🚧 Speech and transcription providers exist in `ironclaw_llm` but are not wired into a product path
- 🚧 Memory: full-text native provider; vector search only through mem0 or Supermemory
- 🚧 Thinking levels: reasoning effort per provider, no per-model defaults

### Not implemented

- ❌ Configuration hot-reload beyond LLM providers
- ❌ Channel health monitor
- ❌ Partial output preservation on abort
- ❌ Embeddings and hybrid memory search
- ❌ Image, audio or video generation
- ❌ OpenTelemetry / Prometheus exporters
- ❌ Heartbeat system

### Out of scope

- 🚫 New messaging channels beyond Slack, Telegram and Web Push
- 🚫 Mobile apps

---

## How to Contribute

1. **Claim a section**: Edit this file and add your name/handle to the "Owner" field
2. **Create a tracking issue**: Link to GitHub issue for the feature area
3. **Update status**: Change ❌ to 🚧 when starting, ✅ when complete
4. **Add notes**: Document any design decisions or deviations, and name the crate or file that implements the row

### Coordination

- Each major section should have one owner to avoid conflicts
- Owners can delegate sub-features to others
- Update this file as part of your PR

---

## Deviations from OpenClaw

IronClaw intentionally differs from OpenClaw in these ways:

1. **Rust vs TypeScript**: native performance, memory safety, single binary (`zlo`)
2. **WASM and container sandboxes**: WASM for extension tools, Docker containers for OS processes in multi-user deployments
3. **libSQL by default, PostgreSQL optional**: storage goes through one filesystem mount catalog (`ironclaw_filesystem`); composition picks the backend
4. **NEAR AI focus**: dedicated NEAR AI backend with session login
5. **No mobile apps**: the backend is a server/CLI; the ZLO desktop client lives in `tools/ironclaw_rune/`
6. **One extension model**: a channel is one capability surface of an extension manifest, not a separate plugin type
7. **Tinfoil private inference**: IronClaw-only provider
8. **GitHub WASM tools**: GitHub integration as a WASM tool package
9. **Prompt-based skills**: `SKILL.md` skills with trust levels and activation criteria, unlike OpenClaw capability bundles
10. **LLM data is never deleted**: no session pruning

These are intentional architectural choices, not gaps to be filled.
