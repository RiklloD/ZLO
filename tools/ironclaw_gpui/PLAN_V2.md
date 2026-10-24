# IronClaw v2 plan

Agreed in the grilling session of 23.10.2026 (Q1 to Q32). IronClaw becomes Lorik's main agent. Hermes keeps running until each feature here reaches parity, then it is retired. Hermes and the t3code plugin are references for behaviour only. None of their infrastructure gets copied.

Goal across every stage: the lightest and fastest app we can build, client and backend both.

## Ground rules

- The backend owns all logic behind `ironclaw_webui` routes. The GPUI app only renders. Everything keeps working with the app closed.
- We no longer track upstream. Core crates get edited directly when that is simpler. New crates exist only where a feature is a clean, separate unit: `ironclaw_orchestrator`, `ironclaw_cli_providers`, `ironclaw_supermemory`, `ironclaw_usage`.
- No Python anywhere in the backend.
- Validate every step with `cargo check -p <crate>` before calling it done.
- Channels (Telegram etc.) are parked: off in config, code left in place.

## Stages

| # | Stage | Ships |
|---|---|---|
| 1 | Design system | Theme tokens (dark only, light later by config), motion helpers, background, layout shell, battery saver |
| 2 | Orchestrator engine | Rust port of the t3code orchestrator v2 core; owns every thread |
| 3 | Profiles | SOUL.md, tools, folder and default model per profile; shared skills and keys; Hermes importer |
| 4 | CLI drivers and telemetry | Claude CLI 1 and 2, Antigravity, Muse Code; status bar; Usage page |
| 5 | Inbox UI and the rest | t3code inbox, delegation and forks in the UI, goals strip, Supermemory, computer use backend |

### 1. Design system

Status 23.10.2026: first pass done. `app/src/design/` holds the Zeron dark palette (written into gpui-kit's theme so every screen repaints), Geist fonts, the motion catalog and the battery saver. The shell has a transparent titlebar, an inset content panel and the Cmd+Shift+S status bar. The battery saver freezes animation (zero frames) instead of t3code's 1 fps, through gpui's `reduce_motion`. Second pass: 18 theme presets from Zeron's built-ins (Settings → Appearance), a wallpaper layer (generated aurora or a user picture, blurred and dimmed once into a 960x600 texture) under glass shell and panel, preferences saved to `~/Library/Application Support/IronClaw Desktop/prefs.json` (theme, wallpaper, status bar), and a new app icon (`app/assets/icon.png`, drafted with Canva from a 200 px render; a full-resolution export is still open).

- Source: Zeron (`github.com/zeronsh/zeron`, MIT, GPUI). Copy and adapt its animation helpers, background and component structure where they fit our GPUI version, keep its MIT notice in `THIRD_PARTY_NOTICES.md`, then remodel.
- Tokens for colour, spacing, radius, type and motion live in one module so a GPUI bump or a light theme stays cheap.
- Battery saver: animations drop to 1 fps when the window is unfocused or idle for 30 s (t3code behaviour).
- Status bar along the bottom edge of the window, toggled with Cmd+Shift+S. Its content arrives in stage 4.

### 2. Orchestrator engine (`ironclaw_orchestrator`)

- Port of the t3code plugin core (`engine`, `store`, `restart`, `handoff`, `schedule`, about 2,800 lines of Python) to Rust. The plugin is the spec; the upstream t3code repo is read only where the plugin is unclear. The Hermes glue (`authority.py`, `gw.py`) stays behind.
- It owns every thread: durable queue, turn admission, delegation tree, forks and merge-back, Stop propagated to descendants, restart continuation, usage-limit resume.
- IronClaw's own agent loop and the CLI drivers become runtimes the engine runs turns on.
- One store in the existing libSQL file. Current `ironclaw_threads` data migrates into it once.
- Tools: `t3_capabilities`, `t3_delegate`, `t3_task`, `t3_thread` (send, steer, fork) first. Schedules reuse IronClaw's routines engine (`ironclaw_triggers`), no second scheduler. Durable `t3_queue` after that.

### 3. Profiles

| Per profile | Shared by all profiles |
|---|---|
| SOUL.md | Skills pool (a profile can hide skills, sees all by default) |
| Tool/extension allowlist | API keys and secrets |
| Default model and provider | MCP server definitions (each profile enables a subset) |
| Working folder, optionally a git repo | CLI logins (Claude 1 and 2, Antigravity, Muse) |
| Supermemory container | |
| Its threads | |

- A thread belongs to one profile for its whole life. Profiles take the place of t3code's projects.
- Each profile has its own folder (for example `ironclaw-builder` works in `~/ironclaw`). Without one, it gets `~/.ironclaw/profiles/<name>/`. CLI agents run in that folder. When it is a git repo, the inbox row shows the branch.
- New thread: the button opens a profile picker sorted by last used, with the last used profile highlighted.
- Hermes importer, one time: copies SOUL.md, default model and Supermemory container settings. Imports every Hermes profile except `hermes-builder`. Hermes `ironclaw` becomes `ironclaw-builder` (shown as "IronClaw Builder"). Both apps keep using the same Supermemory containers during the transition.

### 4. CLI drivers and telemetry

`ironclaw_cli_providers`, behaviour mirrors the t3code plugin's drivers:

| Driver | Base | License |
|---|---|---|
| Claude CLI 1 and 2 | Wire types from `claude-codes` (tracks CLI versions), process handling from Zeron `crates/harness` | Apache-2.0, MIT |
| Antigravity | `agent-client-protocol` 3.5.0 behind a thin wrapper | Apache-2.0 |
| Muse Code | Meta `muse-code-sdk` 1.4.12 (needs Rust 1.97, we pin 1.98) | MIT |

- Claude CLI 2 is the same driver with `CLAUDE_CONFIG_DIR=~/.claude-account2`.
- Pin the `claude` version and bump it on purpose.
- Approvals: every CLI tool call goes through IronClaw's approval queue (`ironclaw_approvals`). The Claude driver passes no allow rules and adds a PreToolUse hook, because Claude auto-approves some calls before the `can_use_tool` callback sees them.
- Orchestrator tools reach CLI agents through one loopback MCP route on the gateway, with a token per CLI session.
- Do not copy from Zed's ACP code or Padu (GPL). Reading is fine.

Status bar (per turn, every runtime):

| Field | Claude CLI source |
|---|---|
| Context used / max, % | Last assistant message: `input + cache_read + cache_creation`; max from `result.modelUsage.context_window` |
| TPS | `--include-partial-messages` stream frames, output tokens per second of generation |
| Cache hit rate | `cache_read / (input + cache_read + cache_creation)` |

The `result` frame totals add up every tool-loop step, so they are not used for context. Antigravity reports context used and window size through ACP `UsageUpdate`; its TPS may be missing.

`ironclaw_usage` (Usage page, footer bar-chart icon), ported from CodexBar (MIT):

- Claude 1 and 2: `/api/oauth/usage`, 5-hour and weekly windows. Token from the Keychain item `Claude Code-credentials` or `.credentials.json` in each config dir.
- Z.ai: `api.z.ai/api/monitor/usage/quota/limit`.
- Codex: `chatgpt.com/backend-api/wham/usage`, only if still used.
- These endpoints are undocumented. Handle drift and 429s, and leave token refresh to each CLI.

### 5. Inbox UI and the rest

Status 24.10.2026 (branch `v3-inbox-ui`): the inbox replaces the sidebar thread list (`app/src/workspace/inbox.rs`, one `GET /api/webchat/v2/inbox` that the backend joins: profile, title, branch, engines of the whole delegation tree, state). Pin, snooze, settle and visit persist in `orch_inbox` (`ironclaw_orchestrator/src/inbox.rs`). New thread opens the profile picker (last used first, highlighted). The goal strip sits above the composer (`/goal`, `orch_goals`). Engine marks are lettered tiles; no brand logos are bundled yet.

- Inbox exactly like t3code: one list by recency, search, filter (profile, model, state), new-profile button, Settled (N) folded at the bottom. States: pinned, inbox, working, snoozed, settled, plus crash and usage-limit recovery.
- Row: profile badge and name, title, branch when the folder is a repo, logos of every engine that ran in the thread (parent plus delegated children).
- Footer: settings and Usage. No PR view.
- Delegation and forks in the chat: subagent frame above the composer, context strip (delegated from, forked from with merge back, usage-limit countdown, paused after Stop, queued follow-ups).
- Goals strip above the composer.
- Supermemory (`ironclaw_supermemory`): with a key set, it is the only memory for that profile (automatic recall and capture, plus search and write tools). Without a key, local `ironclaw_memory` works as today. One primary container per profile, read access to others, as in the Hermes `supermemory.json` files.
- Computer use, backend only: run the Open Computer Use binary as an MCP server through `ironclaw_mcp` and port the Hermes plugin's extra layer (sessions, Stop, take over, frame stream) to Rust. The live tracker UI waits until Lorik asks for it. A full Rust rewrite of the engine is written up in `COMPUTER_USE_REWRITE.md` for later.

## Later, not planned

- Light theme.
- Gemini CLI driver (speaks ACP natively, cheap to add).
- Computer use live tracker UI.
