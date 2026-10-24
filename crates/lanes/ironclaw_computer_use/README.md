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
| Config | Off unless `COMPUTER_USE_ENABLED=true`; `COMPUTER_USE_BINARY` must be an absolute path (see `.env.example`). Malformed config fails startup. |
| Process | Spawned on first use per (tenant, user, thread) with a scrubbed environment (`HOME`, `USER`, `PATH`, `TMPDIR`, locale, plus `OPEN_COMPUTER_USE_*` from config). `kill_on_drop`, so a dropped call kills it. |
| Protocol | MCP `initialize` / `notifications/initialized`, then `tools/call`. Lines are bounded (32 MiB) before buffering. A JSON-RPC error keeps the process; a timeout, exit, oversize line or Stop kills it and the next call restarts it. |
| Tools | `catalog/tools.json`, the server's own `tools/list` copied verbatim (offered without starting the process). |
| Stop | Aborts the call in flight, kills the process, refuses new actions for 15 s. Fired by the user (route) or by the thread's Stop (process-journal observer in `ironclaw_assistant`). |
| Take over | New actions answer `human_has_control` until hand back. A running action finishes. |
| Frames | The latest screenshot from the server's own results, per session, with the target app and window. Nothing captures the screen on its own. |
| Bounds | 4 live sessions, 15 min idle reap, 90 s per call, 256 KiB of tree text to the model; screenshots never go to the model. |

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
