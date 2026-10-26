# ironclaw_computer_use

Computer use on the user's Mac: the [Open Computer Use](https://github.com/iFurySt/open-codex-computer-use)
binary (`OpenComputerUse mcp`) run as a stdio MCP server, one process per
thread, plus the layer the Hermes `computer-use-plus` plugin put on top of it,
ported to Rust: sessions, Stop, take over / hand back, and the latest
screenshot frame for a route to read. No UI.

- **Family / layer:** `lanes` / `runtimes` · **Package:** `ironclaw_computer_use`
- **Use this when:** an already-approved computer-use action must run, or a
  route needs a thread's session state, frame or controls.
- **Don't use this when:** you are deciding whether an action may run. That
  is the approval gate in `ironclaw_assistant::computer_use`, which raises an
  approval request on **every** call and reaches this crate only through the
  `ironclaw_product_contracts::computer_use::ComputerUseSessions` port.

## How it runs

| Piece | Behaviour |
|---|---|
| Config | On when the engine is bundled next to the backend (`../Helpers/Numoh Computer Use.app`, see `tools/ironclaw_gpui/bundle.sh`) or `COMPUTER_USE_BINARY` (absolute path) names one; `COMPUTER_USE_ENABLED=false` turns it off. Malformed config fails startup. The engine runs in-process (`OPEN_COMPUTER_USE_DISABLE_APP_AGENT_PROXY=1`), so macOS permissions belong to the app that started the backend. |
| Process | Spawned on first use per (tenant, user, thread) with a scrubbed environment (`HOME`, `USER`, `PATH`, `TMPDIR`, locale, plus `OPEN_COMPUTER_USE_*` from config). `kill_on_drop`, so a dropped call kills it. |
| Protocol | MCP `initialize` / `notifications/initialized`, then `tools/call`. Lines are bounded (32 MiB) before buffering. A JSON-RPC error keeps the process; a timeout, exit, oversize line or Stop kills it and the next call restarts it. |
| Tools | `catalog/tools.json`, the server's own `tools/list` copied verbatim (offered without starting the process). |
| Stop | Aborts the call in flight, kills the process, refuses new actions for 15 s. Fired by the user (route) or by the thread's Stop (process-journal observer in `ironclaw_assistant`). |
| Prompt | `ComputerUse::instructions()`: the server's own instructions (in `catalog/tools.json`) with the Codex-specific sentences made true for IronClaw, the `computer_use_` tool names, and the fork's guidance (`prompts/`). Composition appends it to the system prompt while computer use is on. The tools are core under tool disclosure. |
| Fork | The bundled engine or a `(Dev).app` binary is the patched fork: action results are tree diffs (`OPEN_COMPUTER_USE_ACTION_RESULT=diff`) and carry a screenshot. |
| Take over | Per desktop (tenant, user), as in Hermes: every session of that user answers `human_has_control` until hand back. A running action finishes. A viewer that stops polling for 30 s loses its hold. |
| Frames | The latest screenshot from the server's own results, per session, with the target app and window, plus the newest 3 tied to the tool result each came with. |
| Live view | Per session: the windows used (CGWindowList), the action label, status, the pointer (coordinate targets, and the native cursor followed from the session's observation file), typing. Window pictures come from `screencapture -l` while a viewer asks (one shared producer per window, stopped 1 s after the last read), else the agent's own screenshot; none while a person holds the desktop, and one taken across a takeover is dropped. |
| Trace | One JSON line per call in `COMPUTER_USE_TRACE_FILE` (default `~/.ironclaw/logs/computer-use.jsonl`): tool, ok, code, seconds. Never arguments or screen content. |
| Bounds | 4 live sessions, 15 min idle reap, 90 s per call, 256 KiB of tree text stored; the model sees up to 20 KiB inline (`ironclaw_assistant::computer_use`). Vision models see each of the newest 3 results with its own screenshot (composition's `LoopToolImagePort`): inside the tool output for the Codex providers, as a user image after the tool results elsewhere. Frames are not persisted. |

## Seams

- `ironclaw_mcp::McpClient` — the MCP lane's client contract. The lane's own
  `McpRuntime` refuses stdio by charter ("unsupported until process-level
  egress controls land"), so only the client seam is shared; the binary path
  comes from host config, never from the request.
- `ironclaw_product_contracts::computer_use::ComputerUseSessions` — the port
  product calls; its tool calls go through the `McpClient` seam.

## Tests

```bash
cargo test -p ironclaw_computer_use   # fake MCP server (a POSIX shell script); no screen access
```
