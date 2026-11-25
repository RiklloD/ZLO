# ZLO

English | [简体中文](README.zh-CN.md) | [Русский](README.ru.md) | [日本語](README.ja.md) | [한국어](README.ko.md)

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

On an Apple Silicon Mac, paste this into Terminal (or have a coding agent do it: see [Install with a coding agent](#install-with-a-coding-agent)):

```sh
curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | sh
```

It builds ZLO from source and installs it to `~/Applications/ZLO.app`. If the Xcode command-line tools are missing, it opens their installer and asks you to run the command again. It installs Rust if you don't have it. The first build takes 10 to 20 minutes.

It also puts the `zlo` command on your `PATH`: `~/.zlo/bin/zlo` links to the CLI inside the app, and one marked block in your login shell's profile (`~/.zprofile` for zsh) adds `~/.zlo/bin`. Open a new terminal afterwards. Set `ZLO_NO_MODIFY_PATH=1` to leave your profile alone. A `zlo` there that ZLO did not make is never replaced. If you move the app, it repairs the link the next time it opens; `zlo update` rebuilds the app where it is. `ZLO_REF=<branch>` installs and follows another branch; a tag pins the install and turns self-update off.

| To | Run |
| --- | --- |
| Update | `zlo update` (or `zlo upgrade`), or the install command again |
| Investigate a problem | `zlo triage` |
| Check the installation | `zlo doctor` |

The installer turns on self-update for the copy of the source it downloaded, so `zlo update` and ZLO's **Update available** work with no setup. If `zlo` is not found, run the app's copy directly: `~/Applications/ZLO.app/Contents/MacOS/zlo-backend triage`.

To build a branch other than `main`, set `ZLO_REF`:

```sh
curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | ZLO_REF=my-branch sh
```

Without an Apple Development signing identity the app is signed ad-hoc, so macOS asks for Accessibility and Screen Recording again after each update.

## Install with a coding agent

You can have Claude Code, Codex, Antigravity or another coding agent install ZLO for you. Paste this into the agent:

```text
Install ZLO on this Mac. Follow the "Instructions for agents" in
https://github.com/RiklloD/ZLO#instructions-for-agents exactly.
```

### Instructions for agents

You are installing ZLO, a macOS desktop app built from source, for the person you are working with. Follow these steps in order. Don't improvise around a failure; report it.

1. **Check the machine.** `uname -s` must print `Darwin` and `uname -m` must print `arm64`. ZLO runs only on Apple Silicon Macs. On anything else, stop and tell the person.
2. **Check the Xcode command-line tools.** Run `xcode-select -p`. If it fails, run `xcode-select --install`. That opens a macOS dialog only the person can click through. Ask them to finish it, wait until `xcode-select -p` succeeds, then continue.
3. **Ask one question before you run the installer:** may it add one line to their shell profile (`~/.zprofile` for zsh), so the `zlo` command works in new terminals? If they say no, set `ZLO_NO_MODIFY_PATH=1` in step 4.
4. **Run the installer** from a shell, in the foreground or as a background job with a timeout of at least 30 minutes. The first build takes 10 to 20 minutes, and later ones are faster.

   ```sh
   curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | sh
   ```

   Rules:
   - Never use `sudo`; the installer needs no admin rights.
   - Don't pass flags it doesn't document. The supported settings are environment variables: `ZLO_REF=<branch or tag>` (default `main`), `ZLO_SRC=<path>` (default `~/Library/Caches/ZLO/src`) and `ZLO_NO_MODIFY_PATH=1`.
   - If Rust is missing, the installer installs it through rustup by itself.
   - If it stops with a message, relay that message to the person instead of working around it. Messages you may see include missing Xcode tools, "has local changes" for a `ZLO_SRC` with uncommitted edits, and "has no branch or tag named …".
   - Exit status 0 means installed. Any other status means it failed.
5. **Verify it.** The installer's PATH change applies only to new terminals, so call the CLI by its full path:

   ```sh
   ~/.zlo/bin/zlo --version          # prints "zlo <version>"
   ~/.zlo/bin/zlo doctor             # every line ✔, exit status 0
   ls ~/Applications/ZLO.app         # the app exists
   ```

   If `~/.zlo/bin/zlo` is missing, use the copy inside the app: `~/Applications/ZLO.app/Contents/MacOS/zlo-backend`. It is the same program.
6. **Hand over.** The installer opens ZLO when it finishes, and the app guides the person through connecting their agents. Tell them to open a new terminal to use `zlo`, and that macOS will ask them to allow Accessibility and Screen Recording the first time computer use runs.

Later, the same agent can use these commands:

| The person wants to | Run |
| --- | --- |
| Update ZLO | `zlo update` (or `zlo upgrade`). It works in the background; `zlo update --json` shows progress. Re-running the install command also updates. |
| Find out why something is broken | `zlo triage` starts a coding agent with ZLO's triage playbook. If you are that agent, follow [`.github/triage/PLAYBOOK.md`](.github/triage/PLAYBOOK.md). |
| Check the setup | `zlo doctor` |
| Find ZLO's data folder | `zlo config path --home` (usually `~/.zlo`) |

Never read the secrets in ZLO's data folder or any agent's sign-in files, and don't delete `~/.zlo`: it holds the person's threads, memory and settings.

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

The interface uses [Rune](https://github.com/anomalyco/rune). The computer-use helper comes from [hermes-plugin-computer-use-plus](https://github.com/RiklloD/hermes-plugin-computer-use-plus). The way ZLO connects to other computers follows [T3 Code](https://github.com/pingdotgg/t3code)'s remote environments, with Tailscale in place of its hosted relay.

This repository is my project, not an official IronClaw or NEAR AI release.

## License

Licensed under either of the following, at your option:

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

Upstream license and attribution notices are retained. Bundled dependencies and the computer-use helper have their own licenses.
