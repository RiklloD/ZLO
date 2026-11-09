# Keeping machines updated

ZLO is built from source, so updating a machine means pulling this repository and rebuilding. Every machine can do that itself, and ZLO can tell all of them to do it at once.

- [This Mac](#this-mac)
- [Turn on self-update for a machine](#turn-on-self-update-for-a-machine)
- [The everyday loop](#the-everyday-loop)
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

Each machine updates from its own source checkout. A machine that built ZLO from source already knows where that checkout is, so turning updates on takes one click or one command:

- **In ZLO:** Settings → Computers. A computer with updates off shows **Updates are off** and a **Turn on** button.
- **In a terminal on that machine:** `zlo update enable`.

Both take effect at once; nothing restarts.

To turn updates off again, click **Turn off** on that computer's row, or run `zlo update disable` there. The computer stops checking and is left out of **Update all machines**. It remembers its checkout, so **Turn on** brings back the same one.

**Turn on** is only offered when it will work: the checkout ZLO was built from is still there, still holds the commit this build came from, belongs to you, and is on a branch with a remote. If not (ZLO came from a prebuilt binary, or the checkout moved or was replaced), it isn't offered. Clone the source there and name it:

```sh
zlo update enable --repo /home/you/src/zlo     # add --branch <name> to follow another branch
```

This writes the `[update]` section of `~/.zlo/config.toml`, which you can also edit by hand:

```toml
[update]
method = "source"              # "none" (the default) turns updates off
repo = "/home/you/src/zlo"     # absolute path to the checkout
# branch = "main"              # optional; defaults to the branch checked out
```

The checkout always comes from the machine itself. Nothing a client sends can change the command, path or branch an update uses: **Turn on** carries no path, and works only from the checkout the backend was built from.

## The everyday loop

1. Change the code and push it to the branch the machines follow.
2. Within a couple of minutes ZLO's status bar shows **Update available**. Each backend checks its own source every two minutes, and ZLO reads the answer once a minute.
3. Click it. Every computer that is behind updates, This Mac last.

A machine only rebuilds when the new commits change something it runs:

| The commits touch | ZLO on a Mac | A backend on its own |
| --- | --- | --- |
| the backend (`crates/`) | rebuilds | rebuilds |
| the desktop app (`tools/ironclaw_rune/`) | rebuilds | nothing to do, keeps running |
| only docs, guidance files, CI or tests | nothing to do | nothing to do |

So an app-only change never restarts a remote backend or interrupts its chats, and its row stays **up to date**.

The status bar must be visible for the prompt (it is by default); Settings → Computers always shows each computer's state.

## Update all machines from ZLO

**Settings → Computers → Updates**:

- **Check for updates** asks every saved computer to fetch right now, instead of waiting for its next check. Each row shows **up to date**, **N commits behind**, or **Updates are off**.
- **Update all machines** updates the other computers in parallel, then This Mac last, because its update reinstalls and reopens ZLO. Each row shows **updating…** with the current step, then **updated** or **failed:** with the reason. The **Update available** prompt in the status bar does the same thing.

While a machine rebuilds, its backend restarts and drops off for a while. ZLO keeps checking every few seconds, finds it again, and reads the result (it waits up to 45 minutes for a slow build). The progress survives closing the window, but not quitting ZLO.

A computer reached only over SSH can be updated only while it's the computer in use, because its tunnel belongs to that connection.

## From a terminal

On any machine:

```sh
zlo update enable      # turn self-update on (see above)
zlo update --check     # how far behind, with the newest commit subjects
zlo update             # pull and rebuild in the foreground
zlo update --json      # machine-readable output
```

What "rebuild" means depends on how the machine runs IronClaw:

| Installed as | An update runs |
| --- | --- |
| ZLO on a Mac (the backend inside `ZLO.app`) | `tools/ironclaw_rune/bundle.sh` from the checkout, which rebuilds, reinstalls and reopens ZLO |
| `zlo` on its own (`~/.zlo/bin/zlo`, a service) | `cargo build --release -p ironclaw --bin zlo`, replaces the binary atomically, then restarts `zlo service` if it's installed (otherwise it tells you to restart `zlo serve`) |

A remote update runs as a separate process that outlives the backend's restart. It keeps its progress in `~/.zlo/update/`, and only one update runs at a time.

**The first update after the rename, on a machine that runs the backend on its own.** A pre-rename `ironclaw update` builds the new code but then looks for `target/release/ironclaw`, which the build no longer produces. Update such a machine once by hand: `git pull && cargo build --release -p ironclaw`, `mkdir -p ~/.zlo/bin && cp target/release/zlo ~/.zlo/bin/`, then `~/.zlo/bin/zlo service install`. That installs the `zlo` service and retires the old `ironclaw` one; later updates work from ZLO again. ZLO on a Mac needs nothing: its update runs `bundle.sh`, which knows the new name.

## What an update refuses to do

An update only **fast-forwards**. It stops with a clear reason, and changes nothing, when:

- tracked files in the checkout have local changes;
- the checkout has commits the remote doesn't have (it has diverged);
- the configured branch isn't the one checked out, or the checkout isn't on a branch at all;
- the fetch times out or fails.

Untracked files don't block an update. Commit, stash or move your local work, then try again.

## Troubleshooting

**"Updates are off"**: click **Turn on**, or run `zlo update enable` on that machine (with `--repo` when it doesn't know its checkout).

**No "Update available" although you pushed**: the machine follows the branch its checkout is on, so check that you pushed to that branch. A checkout with local changes or unpushed commits can't be checked in the background; **Check for updates** shows the reason.

**"failed: … local changes"**: someone edited the checkout on that machine. Run `git status` there.

**A machine is stuck on "updating…"**: run `zlo update --check` there, and look at `~/.zlo/update/run.json`. A run whose process is gone is reported as failed after 30 seconds.

**After updating, ZLO says a computer is older or newer than it**: the protocol versions differ until both sides run the same build. Update the other side too.
