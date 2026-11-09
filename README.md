# ZLO

My personal AI desktop app. Built in Rust for macOS, with a native [Rune](https://github.com/anomalyco/rune) interface and an [IronClaw](https://github.com/nearai/ironclaw) backend.

I'm [Lorik](https://github.com/RiklloD). I'm a business student who likes to tinker with tech. I have more ideas than engineering experience, and I work with AI agents to turn those ideas into software I can use. ZLO is the app I'm building around that way of working.

## Why I'm building it

The desktop app matters to me as much as the agent behind it. I want something light that feels at home on my Mac. Memory use and battery life are part of the product, not things to fix after adding every feature.

This is my own working setup and a place to try ideas. It isn't a promise of a polished release or support for every machine.

## What it does

- **A native interface.** GPU-rendered Rust with Rune. No Electron and no always-on embedded browser; browser and document views are created only when needed.
- **Chats, profiles and models.** Pick the model per task, keep separate profiles, and delegate work to other agents through the orchestrator, including Codex and Claude Code.
- **Tools, skills, memory and MCP** through the IronClaw backend.
- **Computer use** through my fork of the [computer-use engine](https://github.com/RiklloD/hermes-plugin-computer-use-plus).
- **Approvals.** Each profile is either asked first or given full access. Automatic approval is off by default.
- **Work on other computers.** The backend can run on another machine, such as a Mac mini or a Linux server, while ZLO on your laptop is only the window. Machines connect over Tailscale or SSH. See [Using another computer](tools/ironclaw_rune/docs/remote-computers.md).

I'm changing the app as I use it. The code and development notes are a better guide to its current state than a fixed feature checklist.

## How it runs

ZLO has two halves: the **window** (the Rune app) and the **backend** (IronClaw, which runs the agent, its tools and its memory). They talk over authenticated HTTP. Where the backend runs is up to you:

| Setup | What runs where | When to use it |
| --- | --- | --- |
| **All on this Mac** (the default) | ZLO starts its own backend as a child process and stops it when you quit. | One machine, nothing to set up. |
| **Backend on another computer** | The backend runs on a server or another Mac. ZLO connects to it over your tailnet or SSH. | Keep the agent running while the laptop sleeps, or work on a machine with more power, your repos, or a better connection. |
| **App only** (ZLO Light) | A build of ZLO without a backend inside. It only ever connects to other computers. | A laptop that should never run agents itself. |

You can switch between computers in **Settings → Computers**. ZLO works on one computer at a time. Each computer has its own threads, memory, settings and provider keys, because those live with its backend.

State lives under `~/.zlo` on whichever machine runs the backend (set `ZLO_HOME` to put it elsewhere), in libSQL rather than a separate PostgreSQL server. An older install's `~/.ironclaw/reborn` moves there on first start, and the old path stays as a link so nothing that remembers it breaks.

Local storage doesn't mean every task stays on the machine. Using a remote model or an external integration sends the data that request needs to that provider. Computer use needs the relevant macOS permissions on the machine where the backend runs.

## Install

On an Apple Silicon Mac, paste this into Terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | sh
```

It builds ZLO from source and installs it to `~/Applications/ZLO.app`. If the Xcode command-line tools are missing, it opens their installer and asks you to run the command again. It installs Rust if you don't have it. The first build takes 10 to 20 minutes. Run the same command again to update.

To build a branch other than `main`, set `ZLO_REF`:

```sh
curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | ZLO_REF=my-branch sh
```

Without an Apple Development signing identity the app is signed ad-hoc, so macOS asks for Accessibility and Screen Recording again after each update.

## Using another computer

The short version, for a server on your tailnet:

```sh
# On the server
zlo onboard            # providers and keys
zlo service install    # start at boot
zlo remote enable      # publish it on your tailnet (never the internet)
zlo service restart
```

Then open ZLO on your Mac, go to **Settings → Computers**, and click **Connect** next to the server under "Found on your tailnet".

[**Using another computer**](tools/ironclaw_rune/docs/remote-computers.md) covers the whole setup:

- setting up a Linux server or a second Mac;
- pairing with one click, with a code, or over SSH;
- managing paired devices;
- moving your setup (threads, memory, profiles, settings, provider keys) to the server with `zlo transfer user@host`, or `zlo export` / `zlo import`;
- what works remotely;
- how the connection is secured;
- troubleshooting.

[**Keeping machines updated**](tools/ironclaw_rune/docs/updates.md) explains "Update all machines" and `zlo update`.

## Building from source

This is a macOS development setup, not a cross-platform installer. It needs Xcode command-line tools, Rust through rustup, and an Apple Development signing identity. The repository pins the Rust toolchain in [`rust-toolchain.toml`](rust-toolchain.toml). The bundled computer-use engine currently targets Apple Silicon.

```sh
git clone https://github.com/RiklloD/ZLO.git
cd zlo
sh tools/ironclaw_rune/bundle.sh
```

The script builds the backend and desktop, downloads and verifies the pinned computer-use engine, signs the bundle, and installs it to `~/Applications/ZLO.app`. It replaces an existing installation, quitting the app first if it's running, and reopens it if it was open.

| Command | Result |
| --- | --- |
| `sh tools/ironclaw_rune/bundle.sh` | Build, install to `~/Applications/ZLO.app`, reopen |
| `sh tools/ironclaw_rune/bundle.sh --no-install` | Build only, to `tools/ironclaw_rune/target/ZLO.app` |
| `sh tools/ironclaw_rune/bundle.sh --light` | Build and install **ZLO Light.app**, which has no backend and no computer-use engine. It's a separate app, so it never replaces ZLO. |

The backend on its own (for a server) builds with:

```sh
cargo build --release -p ironclaw     # binary at target/release/zlo
```

For changes to the desktop workspace:

```sh
cd tools/ironclaw_rune
cargo check
cargo clippy --all-targets -- -D warnings
cargo test
```

Read [`AGENTS.md`](AGENTS.md) and the owning crate's guidance before changing backend code.

## Finding your way around

| Path | What's there |
| --- | --- |
| [`tools/ironclaw_rune/`](tools/ironclaw_rune/) | The desktop app and its separate Cargo workspace |
| [`tools/ironclaw_rune/app/`](tools/ironclaw_rune/app/) | Rune interface, native views, preferences, backend supervision, and the computers list (`src/remote*`, `src/pages/computers*`) |
| [`tools/ironclaw_rune/client/`](tools/ironclaw_rune/client/) | Typed HTTP client for the desktop, including pairing, Tailscale discovery, SSH and Keychain |
| [`tools/ironclaw_rune/docs/`](tools/ironclaw_rune/docs/) | User guides: other computers, updates |
| [`crates/`](crates/) | IronClaw backend and my extensions to it |
| [`crates/product/ironclaw_webui/`](crates/product/ironclaw_webui/) | The HTTP gateway ZLO talks to, including pairing, devices and the Tailscale identity listener |
| [`crates/app/ironclaw_cli/`](crates/app/ironclaw_cli/) | The `zlo` binary: `serve`, `remote`, `pair`, `devices`, `update`, `service` |
| [`docs/internal/plans/2026-11-08-zlo-remote-environments.md`](docs/internal/plans/2026-11-08-zlo-remote-environments.md) | Design and wire contract for remote computers |
| [`AGENTS.md`](AGENTS.md) | Repository rules for working with coding agents |

The React frontend inherited from IronClaw isn't ZLO's desktop interface. The gateway behind it is still used.

The command you type (`zlo`) and the data folder (`~/.zlo`) are ZLO's. Internal crates keep their IronClaw names (`ironclaw_*`, and the Cargo package `ironclaw` that builds `zlo`), as do the CLI agents' working folders under `~/.ironclaw/` and some older Numoh signing identifiers. Keeping those stable makes upstream merges and macOS permissions easier to maintain.

## Where it comes from

ZLO is my personal fork of [IronClaw](https://github.com/nearai/ironclaw), built by [NEAR AI](https://github.com/nearai). I built my desktop app on that foundation and continue to change the backend for my own use. The agent runtime, security machinery, and much of this repository come from their work.

The interface uses [Rune](https://github.com/anomalyco/rune). An earlier version of the app was built on [GPUI](https://www.gpui.rs/), the Rust UI framework developed for Zed; it is archived on the `archive/gpui` branch (tag `gpui-final`). The computer-use helper comes from [hermes-plugin-computer-use-plus](https://github.com/RiklloD/hermes-plugin-computer-use-plus). The way ZLO connects to other computers follows [T3 Code](https://github.com/pingdotgg/t3code)'s remote environments, with Tailscale in place of its hosted relay.

This repository is my project, not an official IronClaw or NEAR AI release.

## License

Licensed under either of the following, at your option:

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

Upstream license and attribution notices are retained. Bundled dependencies and the computer-use helper have their own licenses.
