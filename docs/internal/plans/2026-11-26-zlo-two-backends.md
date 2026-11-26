# ZLO: several computers at once

Status (2026-11-26): being built. Follows
`2026-11-08-zlo-remote-environments.md` (its "supervisors for inactive
computers" item). All code is in `tools/ironclaw_rune/` (app + client); the
backend does not change.

## Goal

The person has a Linux box that runs most threads and a Mac whose backend
runs the light ones (computer use). Today ZLO talks to one computer at a
time: the sidebar shows only the active computer's threads, the Mac's
backend runs only while This Mac is active, and nothing says where a new
thread will run. After this work:

1. Every saved computer (and This Mac's backend) stays connected at once.
2. One sidebar lists the threads of all of them, each tagged with its computer.
3. Opening a thread on another computer just works, with no connecting screen.
4. A new thread says where it runs and lets the person choose.
5. Notifications and working states arrive from every computer.

## The model: focus follows the thread

The app's state stays single-backend. `State::rt.backend`, `State::session`,
the inbox list, the open chat, every settings page: all still belong to one
computer, the **focused** one (`registry.active`, unchanged on disk). What
is new is that every *other* usable computer is a **peer**: a background
connection that keeps a warm `Backend`, its session, its inbox list and its
unread notifications.

- Opening a peer's thread moves the focus to that peer **by handing its warm
  connection over**: no descriptor fetch, no connecting screen, no retry
  timer. The computer that had the focus becomes a peer and keeps its
  backend handle and its last inbox list.
- Runs keep going on a computer that lost the focus (the backend runs them);
  its peer feed shows them as working and posts their notifications.
- Pages that are not about a thread (Settings, Profiles, Usage, …) show the
  focused computer, as now. The status bar already names it.

This is deliberate: it reuses the tested single-backend app, and makes the
second computer feel present everywhere a thread appears.

## Stage 1: the engine (`app/src/remote/peers.rs` and friends)

### This Mac's backend stays up

`Registry::runs_local_backend` (client `environments.rs`) and
`remote::local_backend_allowed` stop requiring `active == ThisMac`: the
switch "Run IronClaw on this Mac" alone decides (still never in a Light
install, with `IRONCLAW_DESKTOP_URL`, an unreadable registry, or a
`gateway.json` not yet moved). Update the doc comments and tests that state
the old rule.

### Peers

```rust
// app/src/remote/peers.rs  (State: s.remote.peers)
pub struct Peers { /* by computer key, ordered as the registry lists them */ }

pub enum PeerConn { Connecting, Ready, Asleep, Failed(String) }

pub struct Peer {
    pub computer: Computer,          // onboarding::gateway::Computer
    pub conn: PeerConn,
    pub session: Option<SessionInfo>,
    pub inbox: Option<InboxListResponse>,
    pub notifications: Vec<ProductNotification>,   // its unread page, as the workspace's poll reads it
    // private: backend handle, descriptor, tunnel key, route rank, generation, retry state
}
```

Stable API the later stages build on (names fixed; add private helpers freely):

| Function | Does |
|---|---|
| `peers::sync(s)` | Reconcile the peer set with the registry and the focus: one peer per saved computer that is not focused, plus This Mac when its backend may run and it is not focused. Starts missing connections, drops removed ones (handles dropped on the UI thread). Call it after every connect/switch landing, registry change (pair, remove, relabel), and local-backend switch change. |
| `peers::refresh(s)` | Re-read every ready peer's inbox and notifications now. Call it wherever the focused inbox refreshes on an event (window comes to the front, a run ends, a mark). |
| `peers::poll(s, tx)` | The module's share of the app poll: retries and the working-row poll (rules below). |
| `peers::list(s) -> Vec<&Peer>` | Peers in registry order. |
| `peers::get(s, key) -> Option<&Peer>` | One peer by computer key. |
| `peers::run(s, key, label, f, done)` | `jobs::run` on a peer's client: `done` is dropped when that peer's connection has changed since (generation). For marks (pin, snooze, settle) on a peer's row. |
| `peers::focus(s, key)` | Move the focus to a computer. Ready peer: instant handover (below). Otherwise the existing `connect::switch_to`. |
| `peers::focus_thread(s, key, thread_id)` | `focus`, then open that thread once the focus has landed (immediately on a handover). |
| `peers::usable(s) -> Vec<ComputerChoice>` | The focused computer first, then every peer: key, label, `PeerConn`/ready, whether it is This Mac. For menus. |

### Connections

- A peer resolves exactly as a focused connect does (`connect::resolve`:
  token from the store, routes in order, descriptor checked, learned routes
  only to tailnet peers), then reads `session` and `inbox`. Reuse that code;
  do not copy it. Refactor `connect.rs` so the resolving half takes no
  screen state.
- SSH routes: `tunnel.rs` keeps one tunnel today. Make it keep one per
  computer key (the focused one's and each peer's), closed with its
  connection and all on quit.
- A peer never shows the connecting or failed screen and never touches
  `s.conn`. Its problem is `PeerConn::Failed(reason)` (blocked: token,
  protocol, wrong machine) or it retries quietly (unreachable) with the
  same backoff the focused connect uses, capped at 60 s. `Asleep` when its
  tailnet peer is offline (the Computers page already knows how to tell).
- Every reply carries the peer's generation and is dropped when stale
  (README rule 1). Fixtures (`s.fixture`) and tests never open real
  connections; fixtures fill `Peers` directly.

### Energy rules (README rule 4)

No standing timers. A peer's inbox is read: when it connects, on
`peers::refresh` events, and on a 5 s poll **only while** one of its rows is
working and the window is in front (the focused inbox's own rule, `inbox.rs`
`POLL`). Notifications ride on the same reads. Retries stop while the
battery saver holds or the window is hidden, and resume on focus.

### Handover (`peers::focus` on a ready peer)

One UI-thread step, no network:

1. Remember the open thread for the old computer (`last_thread`, as now).
2. The old focused connection (its `Backend`, session, current inbox list,
   tunnel key, route rank, descriptor) becomes a ready peer.
3. The peer's connection becomes the focused one through the existing
   `connect::switched` path (`Mode::Switch`): a new `ConnTag`, state reset,
   its inbox applied, registry `active` saved, failback rescheduled.
4. `peers::sync` runs. The peer's last thread reopens as a switch does today.

A handover while a switch or connect is in flight cancels that attempt
(bump `connect_gen`). If the focused connection later fails and a peer is
ready, the failed screen still offers what it offers now; do not auto-switch.

### Tests (unit, over `jobs::harness` and `test_gateway`)

- the peer set follows the registry, the focus and the local-backend switch;
- two scripted gateways: both connected, both inboxes held;
- handover swaps the two connections with zero requests sent, and a reply
  asked before it is dropped;
- a peer's failure never changes `s.conn`; it retries; a stale reply is dropped;
- `peers::run` reaches the peer's gateway, not the focused one;
- the working-row poll starts and stops with a working row and window focus;
- This Mac's backend is allowed while another computer has the focus.

## Stage 2a: one sidebar

- The inbox merges the focused list with every ready peer's rows: one list
  by recency, same Pinned / Working / Snoozed / Settled shelves, same
  search and filters, ⌘1…9 over the merged order.
- A row from another computer carries that computer's label as a quiet text
  tag (muted text, no new colours, no pills with fills: the flat look).
  Tags appear only when more than one computer is connected; with one
  computer the sidebar is pixel-identical to today.
- Clicking a peer row calls `peers::focus_thread`. The row menu's marks
  (pin, snooze, settle, …) go to the row's own computer (`peers::run`) and
  refresh that peer's list.
- Row identity is `(computer key, thread id)` everywhere a key or a
  selection is derived, so equal thread ids on two computers never collide.
- A peer that is connecting, asleep or failed shows one quiet line at the
  sidebar's foot ("Studio is asleep"), which opens Settings → Computers.
- Settings → Computers shows each computer's live state from `Peers`
  (Connected for peers too), and "Connect" on a ready peer is the handover.
- Scenes: `inbox-two-computers`, `inbox-two-computers-light`,
  `inbox-peer-asleep`, `computers-both-connected`; tests over `rune::Runtime`
  for the merged order, the tag rule, the click, and a mark's route.

## Stage 2b: where a thread runs, and notifications

- The new-thread screen shows a "Runs on" control in the composer's action
  row, next to the worktree control and in its style: the focused
  computer's label; its menu lists `peers::usable`. Picking one moves the
  focus (`peers::focus`) and keeps the draft's text and staged files. The
  profile and model fall back to that computer's defaults when it does not
  have the chosen ones. Hidden with one computer.
- A preference "New threads start on" (Settings → Computers; values: the
  computer last used, or one named computer), saved with the other prefs.
  Opening a new draft moves the focus there when that computer is ready;
  when it is not, the draft stays where it is and says so in one line.
- Computer use: when the person turns computer use on (draft or chat) on a
  computer whose backend cannot do it and another usable computer can, the
  existing "not available" note gains one action, "Run on <label>", which
  moves the draft there. On a thread that already exists it only explains.
- Notifications: each ready peer's unread items become system
  notifications exactly as the focused ones do (`alerts.rs`: primed at
  first read, never replayed), titled with the computer's label when more
  than one computer is connected. A click opens the thread on its computer
  (`peers::focus_thread`). The bell's count includes peers. The
  Notifications page lists peers' items after the focused ones, tagged;
  opening one focuses its computer.
- Scenes: `draft-runs-on`, `draft-runs-on-menu`, `computers-default-pref`;
  tests for the pick keeping the draft, the default applied and skipped,
  the alert click's route, and priming per computer.

## Out of scope

Syncing settings, profiles or memory between computers; showing two
computers' pages side by side; moving a thread between computers; commit,
push and pull on a remote backend.

## Rules for everyone building this

- `tools/ironclaw_rune/README.md` rules apply (no `.unwrap()` outside
  tests, keys, no standing timers, files under ~800 lines, headless scenes).
- Both platforms: `cargo clippy -p ironclaw_rune -p zlo_client --all-targets -- -D warnings`
  and `cargo test -p ironclaw_rune -p zlo_client` on macOS, and
  `sh tools/ironclaw_rune/linux-check.sh` (Docker through colima; run
  `colima start` if Docker is not answering).
- Motion: read `~/.zlo-smooth/MOTION-HELPERS.md` before animating anything.
- Never touch the person's real data: no real backend, Keychain, registry
  (`~/Library/Application Support/ZLO`) or `~/.zlo` in tests or checks.
