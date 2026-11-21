# {{name}}

A ZLO plugin, scaffolded by `zlo extension new {{id}}`.

## Layout

- `manifest.toml` — what the plugin is and what it asks for. ZLO reads only
  this file to learn the plugin's tools, effects, and network targets.
- `schemas/{{id}}/echo.input.v1.json` — the input schema of the example tool.
- `prompts/{{id}}/echo.md` — what the model reads about the example tool.

## Run it

The tools are served by an MCP server over HTTPS. Run a server that answers
`tools/list` and `tools/call` (with an `echo` tool) and set `[mcp].server` in
`manifest.toml` to its URL, then:

```bash
zlo extension search {{id}}     # it is listed as an installed (local) package
zlo extension install {{id}}    # shows what it asks for and asks for consent
```

## Consent

ZLO will not run a plugin until you allow it. The consent you grant is pinned
to a digest of every file in this folder: change any file and the plugin stops
running (`tampered`) until you review it and grant consent again with
`zlo extension grant {{id}} --digest <digest>` (or re-run `install`).

## Installing from another folder

`zlo extension install <path-to-folder>` installs a package from anywhere on
disk through the same checks as an uploaded bundle. That path accepts
WebAssembly component packages only (`[runtime] kind = "wasm"`), like uploads;
an `[mcp]` package such as this one is edited in place, where `zlo extension new`
put it.
