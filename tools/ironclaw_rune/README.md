# ZLO desktop app (Rune)

The native macOS window onto the IronClaw gateway: `app/` is the binary `zlo`
(package `ironclaw_rune`), `client/` the UI-free gateway client
(`zlo_client`). It is its own cargo workspace; `Cargo.toml` says how it is
built against the local Rune checkout and `bundle.sh` builds `ZLO.app`.
Feature notes live in `docs/`.

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

## Manual checks (cannot run headless)

Run these in the installed app after a change to the area they name.

- **Native previews** (`app/src/native_view.rs`): open an HTML or PDF file
  in the side pane and a page in the Browser tab.
  - Press ⌘K, open the model picker, and delete a thread from a sidebar
    row's menu (the confirm dialog). The preview must vanish while each
    shows and come back when it closes.
  - Switch to another tab of the same dock and back, open another page of
    the app and return, close and reopen the dock: the preview goes and
    comes back with its tab, and appears once the dock has slid open.
  - Resize the window and drag the dock's edge: the preview follows.
  - Leave the Browser tab for over two minutes and return: the page comes
    back where it was (its view was ended meanwhile).
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
