# Keeping machines updated

ZLO is built from source, so updating a machine means pulling this repository and rebuilding. Every machine can do that itself, and ZLO can tell all of them to do it at once.

- [This Mac](#this-mac)
- [Turn on self-update for a machine](#turn-on-self-update-for-a-machine)
- [Update all machines from ZLO](#update-all-machines-from-zlo)
- [From a terminal](#from-a-terminal)
- [What an update refuses to do](#what-an-update-refuses-to-do)
- [Troubleshooting](#troubleshooting)

## This Mac

Run the installer again:

```sh
curl -fsSL https://raw.githubusercontent.com/RiklloD/ZLO/main/install.sh | sh
```

or, from your checkout, `git pull && sh tools/ironclaw_rune/bundle.sh`. Both rebuild ZLO and its backend, reinstall the app, and reopen it.

## Turn on self-update for a machine

Each machine updates from its own source checkout. Tell it where that checkout is in `~/.ironclaw/reborn/config.toml`:

```toml
[update]
method = "source"              # "none" (the default) turns updates off
repo = "/home/you/src/zlo"     # absolute path to the checkout
# branch = "main"              # optional; defaults to the branch checked out
```

Restart the backend afterwards: `ironclaw service restart`, or quit and reopen ZLO on a Mac that runs it.

The settings come only from this file. Nothing a client sends can change the command, path or branch an update uses.

## Update all machines from ZLO

**Settings → Computers → Updates**:

- **Check for updates** asks every saved computer how far behind it is. Each row shows **up to date**, **N commits behind**, or **not set up** when that machine has `method = "none"`. When any machine isn't set up, the card shows the lines to add.
- **Update all machines** updates the other computers in parallel, then This Mac last, because its update reinstalls and reopens ZLO. Each row shows **updating…** with the current step, then **updated** or **failed:** with the reason.

While a machine rebuilds, its backend restarts and drops off for a while. ZLO keeps checking every few seconds, finds it again, and reads the result (it waits up to 45 minutes for a slow build). The progress survives closing the window, but not quitting ZLO.

A computer reached only over SSH can be updated only while it's the computer in use, because its tunnel belongs to that connection.

## From a terminal

On any machine:

```sh
ironclaw update --check     # how far behind, with the newest commit subjects
ironclaw update             # pull and rebuild in the foreground
ironclaw update --json      # machine-readable output
```

What "rebuild" means depends on how the machine runs IronClaw:

| Installed as | An update runs |
| --- | --- |
| ZLO on a Mac (the backend inside `ZLO.app`) | `tools/ironclaw_rune/bundle.sh` from the checkout, which rebuilds, reinstalls and reopens ZLO |
| `ironclaw` on its own (`~/.ironclaw/bin/ironclaw`, a service) | `cargo build --release -p ironclaw`, replaces the binary atomically, then restarts `ironclaw service` if it's installed (otherwise it tells you to restart `ironclaw serve`) |

A remote update runs as a separate process that outlives the backend's restart. It keeps its progress in `~/.ironclaw/reborn/update/`, and only one update runs at a time.

## What an update refuses to do

An update only **fast-forwards**. It stops with a clear reason, and changes nothing, when:

- tracked files in the checkout have local changes;
- the checkout has commits the remote doesn't have (it has diverged);
- the configured branch isn't the one checked out, or the checkout isn't on a branch at all;
- the fetch times out or fails.

Untracked files don't block an update. Commit, stash or move your local work, then try again.

## Troubleshooting

**"not set up"**: add the `[update]` section above on that machine and restart its backend.

**"failed: … local changes"**: someone edited the checkout on that machine. Run `git status` there.

**A machine is stuck on "updating…"**: run `ironclaw update --check` there, and look at `~/.ironclaw/reborn/update/run.json`. A run whose process is gone is reported as failed after 30 seconds.

**After updating, ZLO says a computer is older or newer than it**: the protocol versions differ until both sides run the same build. Update the other side too.
