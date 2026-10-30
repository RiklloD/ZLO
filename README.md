# ZLO

My personal AI desktop app. Built in Rust for macOS, with a native [GPUI](https://www.gpui.rs/) interface and an [IronClaw](https://github.com/nearai/ironclaw) backend.

I'm [Lorik](https://github.com/RiklloD). I'm a business student who likes to tinker with tech. I have more ideas than engineering experience, and I work with AI agents to turn those ideas into software I can use. ZLO is the app I'm building around that way of working.

## Why I'm building it

I want an agent that can do work on my computer, not just tell me how to do it. I also want to see what it's doing, choose which model handles a task, and decide what it can access.

The desktop app matters to me as much as the agent behind it. I want something light that feels at home on my Mac. Memory use and battery life are part of the product, not things to fix after adding every feature.

This is my own working setup and a place to try ideas. It isn't a promise of a polished release or support for every machine.

## What I'm working with

- A native, GPU-rendered Rust interface using GPUI. No Electron and no always-on embedded browser. Browser and document views are created when needed.
- Chats, profiles, model selection, and an orchestrator for delegating work to other agents.
- Tools, skills, memory, and MCP integrations through the IronClaw backend.
- Computer use through my fork of the [computer-use engine](https://github.com/RiklloD/hermes-plugin-computer-use-plus).
- Approval controls so I can choose between being asked first and giving a profile full access. Automatic approval is off by default.

I'm changing the app as I use it. The code and development notes are a better guide to its current state than a fixed feature checklist.

## How it runs

ZLO bundles the desktop window, backend, and computer-use helper in one macOS app. The window starts the backend as a child process. Quitting the app stops the backend and orchestrator too. I don't run a separate always-on service for it.

The desktop talks to the backend over authenticated HTTP. My local setup uses libSQL, with state under `~/.ironclaw/reborn`, rather than a separate PostgreSQL server.

Local storage doesn't mean every task stays on the machine. Using a remote model or an external integration sends the data needed for that request to that provider. Computer use also needs the relevant macOS permissions.

## Finding your way around

| Path | What's there |
| --- | --- |
| [`tools/ironclaw_gpui/`](tools/ironclaw_gpui/) | The desktop app and its separate Cargo workspace |
| [`tools/ironclaw_gpui/app/`](tools/ironclaw_gpui/app/) | GPUI interface, native views, preferences, and backend supervision |
| [`tools/ironclaw_gpui/client/`](tools/ironclaw_gpui/client/) | Typed HTTP client for the desktop |
| [`crates/`](crates/) | IronClaw backend and my extensions to it |
| [`crates/product/ironclaw_webui/`](crates/product/ironclaw_webui/) | The HTTP gateway used by ZLO |
| [`AGENTS.md`](AGENTS.md) | Repository rules for working with coding agents |

The React frontend inherited from IronClaw isn't ZLO's desktop interface. The gateway behind it is still used.

You'll also see `ironclaw_*`, `.ironclaw`, and some older Numoh names in the code. ZLO is the app's name; I haven't renamed every internal crate, data path, or signing identifier. Keeping those stable makes upstream merges and macOS permissions easier to maintain.

## Building from source

This is a macOS development setup, not a cross-platform installer. It needs Xcode command-line tools, Rust through rustup, and an Apple Development signing identity. The repository pins the Rust toolchain in [`rust-toolchain.toml`](rust-toolchain.toml). The bundled computer-use engine currently targets Apple Silicon.

```sh
git clone https://github.com/RiklloD/zlo.git
cd zlo
sh tools/ironclaw_gpui/bundle.sh
```

The script builds the backend and desktop, downloads and verifies the pinned computer-use engine, signs the bundle, and installs it to `~/Applications/ZLO.app`. It replaces an existing local installation and quits the app first if it's running. It reopens the app if it was open before the install.

To build the bundle without installing it:

```sh
sh tools/ironclaw_gpui/bundle.sh --no-install
```

The bundle lands at `tools/ironclaw_gpui/target/ZLO.app`.

For changes to the desktop workspace:

```sh
cd tools/ironclaw_gpui
cargo check
cargo clippy -p ironclaw_gpui -p ironclaw_gpui_client --all-targets -- -D warnings
```

Read [`AGENTS.md`](AGENTS.md) and the owning crate's guidance before changing backend code.

## Where it comes from

ZLO is my personal fork of [IronClaw](https://github.com/nearai/ironclaw), built by [NEAR AI](https://github.com/nearai). I built my desktop app on that foundation and continue to change the backend for my own use. The agent runtime, security machinery, and much of this repository come from their work.

The interface uses [GPUI](https://www.gpui.rs/), the Rust UI framework developed for Zed. The computer-use helper comes from [hermes-plugin-computer-use-plus](https://github.com/RiklloD/hermes-plugin-computer-use-plus).

This repository is my project, not an official IronClaw or NEAR AI release.

## License

Licensed under either of the following, at your option:

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

Upstream license and attribution notices are retained. Bundled dependencies and the computer-use helper have their own licenses.
