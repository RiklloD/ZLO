# ZLO desktop app (Rune)

The native window onto the IronClaw gateway, for macOS and Linux: `app/` is
the binary `zlo` (package `ironclaw_rune`), `client/` the UI-free gateway
client (`zlo_client`). It is its own cargo workspace; `Cargo.toml` says how
it is built against a pinned rev of upstream Rune. `bundle.sh` builds
`ZLO.app` on macOS and `install-linux.sh` installs it on Linux. Feature notes
live in `docs/`.

## How the app is put together

Rune is a single state tree: `view(s: &State, cx) -> El` re-runs after every
handler and every async landing, and handlers mutate the root `State`
(`app/src/state.rs`).

| Need | Use |
|---|---|
| a screen's state | a struct in a slot of `State`, owned by that screen's module |
| a gateway request | `jobs::run(s, "label", f, \|s, result, tx\| ..)` |
| blocking work (disk, decoding) | `jobs::blocking(s, work, done)` |
| a poll or retry later | `jobs::after(s, delay, \|s, tx\| ..)` |
| per-frame or debounce timing | a deadline in state, `tx.wake_after(d)` and the module's `poll` |
| a live stream | a task forwarding each message with `s.rt.handle.update(..)` |
| one screen calling another | a plain function call, e.g. `workspace::open_thread(s, id)` |
| popovers, menus, dialogs | rune-kit components pushed into the view's `layer` |
| a preference change | `s.update_prefs(\|p\| ..)` |
| shortcuts | `workspace::on_key`, native menu items |

## Rules

1. **Async replies check they still apply.** `done` gets the whole `State`:
   look up your slot again and compare ids or a `jobs::Generation` before
   writing, because the person may have switched thread or page meanwhile.
2. **No `.unwrap()`/`.expect()`** outside tests. Errors go to the log with
   `tracing::warn!` and to the UI as a message; `info!` is for user-facing
   status only.
3. **Keys.** Every element that animates, is clicked or appears in a list
   gets a stable `.key(..)` (thread id, row id). Rune diffs by key.
4. **Nothing wakes the app on a standing timer.** A poll asks for one wake
   at its next deadline and stops asking when it has none; pollers rest
   while the battery saver holds (`design::look::saving`). Check with the
   energy meter (`app/src/energy.rs`, `ZLO_ENERGY`).
5. **Split a file by capability before it grows past about 800 lines.**
6. **Every save goes through the one ordered writer** (`app/src/persist.rs`):
   atomic replace, latest wins per file, flushed on quit.
7. **Headless checks.** A screen has fixture scenes in `app/src/scenes.rs`
   (the fixture sets `s.fixture = true`); render one with
   `zlo --snap out.png --scene NAME --size 1280x820` (add `--tree` for the
   element tree) and look at the PNG. Interactions that matter get a unit
   test over `rune::Runtime` (`app/src/workspace/shell.rs` has the pattern).

8. **Two platforms.** Every change compiles and passes its tests on macOS
   and Linux: `sh linux-check.sh` runs the Linux side (natively on Linux, in
   a Docker container elsewhere). Platform differences live behind small
   seams, never scattered `cfg`s: `app/src/system.rs` (opening files and
   links, the shell, the computer's name, programs on the PATH, the
   shortcut key), `native_view` (`mac.rs` / `none.rs`), and macOS-only
   modules gated whole (voice, SnapShots, permissions, notifications'
   AppKit side). Shortcuts are written `mod+…` (⌘ on macOS, Ctrl on Linux),
   never `cmd+…`, which Rune reads as the Super key on Linux.

## Linux

`sh install-linux.sh` builds the backend and the app and installs them for
this user (`~/.local/share/zlo/app`, an app-menu entry, `zlo-desktop` on the
PATH); `--light` installs the window only, for a backend on another
computer. The header of `install-linux.sh` lists the packages it needs.

What differs from macOS:

| Feature | On Linux |
|---|---|
| Chats, settings, remote computers, files, git, console | Same |
| Shortcuts | Ctrl where macOS uses ⌘ |
| Device tokens of paired computers | `~/.config/ZLO/device-tokens.json`, mode 0600 (no Keychain) |
| Notifications | `notify-send` (libnotify); clicking one does not open the thread |
| Open at login | `~/.config/autostart/zlo.desktop` |
| Open in | the editors' commands (`code`, `cursor`, `zed`, …), the file manager, a terminal |
| Browser tab | pages open in the system browser |
| File previews (PDF, Office, media) | not drawn in the window; open them in their app |
| Voice, SnapShots, computer use | not available (macOS frameworks) |
| Window | the window manager draws the title bar; no traffic-light gap |

Headless renders (`cargo run -p ironclaw_rune -- --snap …`; Rune keeps `--snap` out of release builds) work without a display through Mesa's
software Vulkan (`mesa-vulkan-drivers`), so an agent on a Linux box can look
at the UI it changes.

## Manual checks (cannot run headless)

Run these in the installed app after a change to the area they name.

- **Native previews** (`app/src/native_view.rs`): open an HTML or PDF file
  in the side pane and a page in the Browser tab.
  - Press ⌘K, open the model picker, and delete a thread from a sidebar
    row's menu (the confirm dialog). The preview must vanish while each
    shows and come back when it closes, fading in only after the picker's
    own fade has finished (never over it).
  - Switch to another tab of the same dock and back, open another page of
    the app and return, close and reopen the dock: the preview goes and
    comes back with its tab, and appears once the dock has slid open.
  - Resize the window and drag the dock's edge: the preview follows.
  - Leave the Browser tab for over two minutes and return: the page comes
    back where it was (its view was ended meanwhile).
  - The Browser follows the page without polling: it watches the web
    view (key-value observing) and reads it at most four times a second.
    Click a link in the page, use Back/Forward in the bar and in the page
    (a mouse's side button), submit a form with Return, and navigate a
    single-page app (its address changes without a load). The address
    field, Back/Forward and the load bar keep up each time.
  - Without touching anything, open a page that redirects after 5 s
    (`data:text/html,<meta http-equiv=refresh content="5;url=https://example.com">`
    saved as a file, or any delayed redirect): the address field and Back
    update on their own when it redirects.
  - Open a page whose script changes its address every second
    (`setInterval(() => history.pushState({}, '', '?t=' + Date.now()), 1000)`):
    the address field follows each change; ZLO's CPU stays low.
  - Leave a loaded, quiet page alone: Activity Monitor's "Idle Wake Ups"
    for ZLO stays near zero.
  - Leave the Browser tab for over two minutes (its view ends) and quit
    ZLO, or close the page: no crash (the observer is removed with the
    view).
- **Translucent window:** turn on Settings → Appearance → Translucent
  window; the row adds "Takes effect when ZLO next opens." Relaunch: the
  desktop shows blurred through the sidebar and, fainter, the panel. Turn
  it off: the window turns opaque at once.
- **Window frame saved as it changes:** move and resize the window, wait a
  second, then check `~/Library/Application Support/ZLO/window.json` holds
  the new frame (top-left origin). Enter full screen and quit: the next
  launch reopens at the last windowed frame. Force-quit after a move: the
  frame is still the moved one.
- **Quit ZLO:** the app menu shows "Quit ZLO ⌘Q"; picking it (and ⌘Q)
  saves drafts and the frame, quits, and stops the backend's launchd job.
- **Quick composer placement:** ⇧⌘Space opens the card centred, a fifth of
  the way down the primary screen.
- **Composer refocus:** click into the side pane's Browser address field,
  switch to another app and back: the caret stays in the address field.
  With nothing focused, coming back puts it in the composer.
- **Wheel glide:** with a notched mouse, one tick in the transcript, the
  inbox and a settings page glides about 40 pt over ~0.2 s, and a fast spin
  is one motion; a trackpad still tracks the fingers 1:1; ⌘ + wheel is not
  smoothed.
- **System pieces:** system notifications, voice (including dictation into
  the quick composer from another app) and SnapShot work only in a real
  window.
