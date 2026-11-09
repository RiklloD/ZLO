# Using another computer

ZLO is a window onto an IronClaw backend. By default that backend runs on the same Mac, but it doesn't have to: it can run on a Mac mini, a Linux server, or any machine you own, while ZLO on your laptop only shows the window. This page covers setting that up.

- [How it fits together](#how-it-fits-together)
- [Before you start](#before-you-start)
- [Set up the backend computer](#set-up-the-backend-computer)
- [Connect from ZLO](#connect-from-zlo)
- [Pairing other devices](#pairing-other-devices)
- [Laptop only: turning off the local backend](#laptop-only-turning-off-the-local-backend)
- [What works on another computer](#what-works-on-another-computer)
- [How it is secured](#how-it-is-secured)
- [Troubleshooting](#troubleshooting)
- [Command reference](#command-reference)

## How it fits together

```
 Your laptop                                   The backend computer
┌──────────────┐   HTTPS over your tailnet    ┌───────────────────────────────┐
│ ZLO (window) │ ───────────────────────────► │ tailscale serve (port 443)    │
│              │   (WireGuard, end to end)    │        │                      │
│ Keychain:    │                              │        ▼                      │
│ device token │                              │ ironclaw serve                │
└──────────────┘                              │   127.0.0.1:3000  main        │
                                              │   127.0.0.1:3001  tailnet     │
                                              │ threads, memory, keys, repos  │
                                              └───────────────────────────────┘
```

- **The backend never listens on the network.** It stays on `127.0.0.1`. `tailscale serve` publishes it to your tailnet only, at `https://<machine>.<tailnet>.ts.net`, with a real certificate. It is never published to the internet; ZLO refuses Tailscale Funnel.
- **Each device gets its own token.** Pairing turns a one-time code into a device token, stored in the macOS Keychain. Revoking one device doesn't affect the others.
- **ZLO works on one computer at a time.** Each computer has its own threads, memory, settings, provider keys and files, because they live with its backend. Switching computers in ZLO switches all of that. Nothing is copied between computers.
- **Without Tailscale**, ZLO can reach a computer over SSH instead: it opens an SSH tunnel to the backend's loopback port when you use that computer.

This follows [T3 Code](https://github.com/pingdotgg/t3code)'s remote environments, with Tailscale doing the job of its paid relay.

## Before you start

- **Tailscale** on both machines, signed in to the same account. The free personal plan is enough. On a Mac, the App Store or standalone Tailscale app both work.
- **A build of this repository** on the backend computer. It needs the same version as your ZLO, because both check a protocol version on every connect.
- **Your repos on the backend computer.** The agent works on that machine's files, so the projects it should touch need to be there.
- **Codex or Claude Code**, if you use them, installed and logged in on the backend computer.

What doesn't move: threads, memory and keys on your Mac stay on your Mac. A new backend computer starts empty.

## Set up the backend computer

### A Linux server

```sh
# 1. Tailscale
curl -fsSL https://tailscale.com/install.sh | sh
sudo tailscale up

# 2. Build IronClaw (needs Rust from rustup, Node.js and pnpm for the bundled web assets)
git clone https://github.com/RiklloD/zlo.git ~/src/zlo
cd ~/src/zlo
cargo build --release -p ironclaw
mkdir -p ~/.ironclaw/bin
cp target/release/ironclaw ~/.ironclaw/bin/ironclaw
export PATH="$HOME/.ironclaw/bin:$PATH"   # add this to your shell profile

# 3. Set it up and run it at boot (systemd user service)
ironclaw onboard
ironclaw service install
loginctl enable-linger "$USER"            # keep it running after you log out

# 4. Publish it on your tailnet
ironclaw remote enable
ironclaw service restart
```

`ironclaw remote enable` prints the address, for example `https://studio.tail1234.ts.net`, and whether one-click pairing is on.

Computer use is macOS-only. On a Linux backend, ZLO shows it as unavailable.

### Another Mac

**With ZLO on it.** Open ZLO on that Mac, go to **Settings → Computers**, and turn on **Let my other devices use this Mac**. ZLO runs `ironclaw remote enable` and restarts its backend. The backend runs while ZLO is open there.

**Without ZLO**, as a service that runs at login:

```sh
brew install tailscale    # or the Tailscale app, signed in
git clone https://github.com/RiklloD/zlo.git ~/src/zlo && cd ~/src/zlo
cargo build --release -p ironclaw
mkdir -p ~/.ironclaw/bin && cp target/release/ironclaw ~/.ironclaw/bin/
~/.ironclaw/bin/ironclaw onboard
~/.ironclaw/bin/ironclaw service install       # launchd
~/.ironclaw/bin/ironclaw remote enable
~/.ironclaw/bin/ironclaw service restart
```

For computer use on that Mac, grant Accessibility and Screen Recording to the process that runs the backend. When remote, ZLO shows on your laptop whether that machine is ready.

### Check it from the server

```sh
ironclaw remote status
```

The output shows the tailnet address, the owner (your Tailscale login), whether `tailscale serve` is running, and how many devices are paired.

## Connect from ZLO

Open **Settings → Computers** on your laptop. There are three ways to add a computer.

**1. One click, over Tailscale.** Under **Add a computer → Found on your tailnet**, ZLO lists every online machine on your tailnet that runs IronClaw. Click **Connect**. The backend checks your Tailscale login against its owner and gives this Mac a device token. There's nothing to type.

**2. With a pairing code.** Use this when one-click is off on that machine, or the machine belongs to someone else on your tailnet. On the backend computer, run:

```sh
ironclaw pair
```

It prints a code like `7KQ2-M9XD-4TRA`, valid for 5 minutes and usable once, and a link like `https://studio.tail1234.ts.net/pair#code=7KQ2-M9XD-4TRA`. Paste either into **Pairing link or code** and click **Pair**. ZLO on that machine can do the same from **Settings → Computers → Pair a device**.

**3. Over SSH, without Tailscale.** Enter `user@host` (or `user@host:port`) in **Over SSH, without Tailscale**. ZLO runs `ironclaw pair --json` on that machine over SSH, opens a tunnel, and pairs through it. You need:

- an SSH key that logs in without a password (ZLO never asks for one);
- the host already known to SSH. Run `ssh user@host` once in Terminal first, because ZLO refuses unknown host keys;
- `ironclaw` on the remote machine's `PATH`, or in `~/.ironclaw/bin/`.

After pairing, the computer appears under **Other computers** with its state: **Connected**, **Connecting…**, **Asleep** (offline on the tailnet) or **Not connected** with the reason. Click **Connect** to work on it, or **Remove** to forget it on this Mac. Remove deletes only the local token and entry; the computer and its data are untouched.

ZLO tries routes in order: the tailnet address first, then any direct address, then SSH.

## Pairing other devices

On the computer you're connected to, **Settings → Computers** has two more sections:

- **Pair a device.** Makes a one-time code and link to give to another device.
- **Paired devices.** Lists every device with its name, how it paired, and when it was last seen. **Revoke** cuts one off immediately and permanently, including after a restart.

From a terminal on the backend computer:

```sh
ironclaw devices                     # list
ironclaw devices revoke "MacBook Air"   # by name or id
ironclaw devices revoke --all
```

People on the same tailnet who aren't you can't use one-click pairing. They need a code from you, or their own IronClaw.

## Laptop only: turning off the local backend

Once your work lives on another computer, the laptop doesn't need its own backend:

- **Keep the full ZLO and turn it off.** In **Settings → Computers → This Mac**, turn off **Run IronClaw on this Mac**. ZLO then never starts a backend on this Mac and only connects to other computers. Turn it back on any time.
- **Or install ZLO Light**, a build with no backend and no computer-use engine inside:

  ```sh
  sh tools/ironclaw_rune/bundle.sh --light
  ```

  It installs `~/Applications/ZLO Light.app` next to ZLO (it never replaces it). On first launch it goes straight to **Add a computer**.

## What works on another computer

When ZLO is connected to another computer, everything runs there:

| Feature | On another computer |
| --- | --- |
| Chats, profiles, models, tools, skills, memory, MCP | Yes. They live on that machine. |
| Attachments | Uploaded from your Mac as files (within the backend's size limit), never sent as a path |
| Files tab, previews, `@`-mentions of files and folders | Yes, read from the thread's folder on that machine |
| Changes, History: status, diff, stage, unstage, discard, undo the last turn | Yes, git runs there |
| Commit, push, pull | Not yet; disabled with a note |
| Console | Yes, on a single-user setup (the same condition under which the agent gets a shell). Otherwise it says it isn't available. |
| Computer use | Uses that machine's screen and permissions. ZLO shows "Computer use on Studio: ready" or what to grant there (Studio being that computer's name). |
| Open in editor | Opens VS Code over SSH (`vscode://vscode-remote/ssh-remote+<host>`) when ZLO knows the host |
| Updates | See [Keeping machines updated](updates.md) |

Nothing that belongs to the backend runs on your Mac: no local git, shell, file reads or permission prompts for a remote computer.

## How it is secured

- **Only your tailnet can reach the backend.** It stays on loopback; `tailscale serve` is tailnet-only, and ZLO refuses Funnel.
- **Traffic is encrypted end to end.** Tailscale uses WireGuard, and on top of that ZLO uses HTTPS with normal certificate checks. Plain HTTP is only ever used to this Mac. ZLO follows redirects only within the same server, so a token or code can't be sent elsewhere.
- **Pairing codes** are random, valid for 5 minutes, usable once, and stored only as a hash. Wrong codes all get the same answer, and the pairing routes are rate-limited.
- **Device tokens** are stored only as a hash on the backend and in the Keychain on your Mac. Revoking one is permanent and survives restarts.
- **One-click pairing trusts your Tailscale login.** That login is read only on a second loopback port that `tailscale serve` targets. The normal port removes any `Tailscale-*` headers a client sends.
- **One-click pairing is off on shared machines.** If the backend machine has other login accounts, or user accounts from LDAP, SSSD or similar, one-click is off and a code is needed, because another local user could otherwise reach that port. `ironclaw remote enable --trust-local-users` turns it on anyway.
- **A paired device has full operator access**, the same as the backend's own token. Pair only your own devices, and revoke any you lose.

## Troubleshooting

**The computer doesn't show under "Found on your tailnet".**
Check that Tailscale is running on both machines and signed in to the same account (`tailscale status`). On the backend, run `ironclaw remote status`; serve should say "serving". If ZLO can't find Tailscale on your Mac, discovery is hidden, but codes and SSH still work.

**"belongs to someone else on your tailnet" or "One-click pairing is off".**
Run `ironclaw pair` on the backend and use the code.

**"has no secure address yet".**
Run `ironclaw remote enable` on that computer, then `ironclaw service restart`.

**"turned this Mac's token down (it may have been revoked)".**
Pair again with a code.

**"IronClaw on … is older than this ZLO" or "update ZLO".**
The two builds speak different protocol versions. Update the older one; see [updates.md](updates.md).

**SSH: "Set up an SSH key for user@host first".**
ZLO never types passwords. Set up a key with `ssh-copy-id user@host`, then run `ssh user@host` once to accept the host key.

**ZLO says it can't read its list of computers.**
`~/Library/Application Support/ZLO/environments.json` is damaged. Fix or delete it, or click **Use this Mac** on that screen. The file is set aside, not deleted.

**Turn remote access off.**
On the backend, run `ironclaw remote disable`, then `ironclaw service restart`. It removes only IronClaw's own `tailscale serve` entry.

## Command reference

| Command | What it does |
| --- | --- |
| `ironclaw remote enable [--https-port 443] [--trust-local-users] [--json]` | Publish the backend on your tailnet with `tailscale serve`, record you as its owner, write `[remote]` in `config.toml` |
| `ironclaw remote status [--json]` | Address, owner, serve state, paired devices |
| `ironclaw remote disable` | Stop publishing it (only IronClaw's own serve entry) |
| `ironclaw pair [--json]` | A one-time pairing code and link, valid 5 minutes |
| `ironclaw devices [--json]` | List paired devices |
| `ironclaw devices revoke <id-or-name>` / `--all` | Revoke devices |
| `ironclaw service install \| start \| stop \| restart \| status \| uninstall` | Run the backend as a launchd or systemd service |

The `[remote]` section `remote enable` writes to `~/.ironclaw/reborn/config.toml`:

```toml
[remote]
enabled = true
identity_port = 3001                  # loopback port tailscale serve targets
owner_logins = ["you@example.com"]    # Tailscale logins allowed one-click pairing
trust_local_users = false
tailnet_url = "https://studio.tail1234.ts.net"
https_port = 443
```

On your Mac, ZLO keeps its list of computers in `~/Library/Application Support/ZLO/environments.json` (no secrets) and device tokens in the Keychain under the service "ZLO".

The design and wire contract are in [`docs/internal/plans/2026-11-08-zlo-remote-environments.md`](../../../docs/internal/plans/2026-11-08-zlo-remote-environments.md).
