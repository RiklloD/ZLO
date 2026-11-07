# ZLO on Rune: the port plan

Started 07.11.2026. A full rewrite of the GPUI app (`tools/ironclaw_gpui/app`,
121 files, ~55k lines) on [Rune](https://github.com/anomalyco/rune), file by
file. **The GPUI app is not touched**: if Rune disappoints, we go back to it.

- Binary: `zlo-rune` (`cargo run --release` here). The GPUI app stays `zlo`.
- App bundle: `bash bundle.sh` builds `target/ZLO Rune.app` and installs it
  to `~/Applications` (`--no-install` builds only; `ZLO_BACKEND=` reuses a
  built backend). Bundle id and login-item LaunchAgent label are both
  `com.numoh.rune`, apart from GPUI's `com.numoh.desktop`, so permissions,
  notifications, defaults and "Open at login" never collide. The backend
  (`zlo-backend`, signed `com.numoh.backend`) and the gateway home are shared.
- Shared: the GUI-free gateway client `tools/ironclaw_gpui/client` (path
  dependency, unchanged) and the preferences file
  (`~/Library/Application Support/ZLO/prefs.json`, same format).
  Every save in this app goes through one ordered writer (`app/src/persist.rs`):
  atomic replace (temp file + rename), latest-wins per file, failed writes
  retried, and quit (`workspace::on_close`) flushes it. A prefs save re-reads
  the file and overwrites only the top-level keys this app changed since it
  last read or wrote them, so a GPUI change to another key survives. What is
  left is last-writer-wins per key: if both apps change the same key, the
  later save wins, and the GPUI app itself still writes the whole file (it
  can put back a key this app changed while it ran). Drafts (`drafts/`, one
  file each) are last-writer-wins per draft across the two apps.
- Rune is pinned to `dev` rev `53c7f60f` in `Cargo.toml`. A local checkout for
  reading lives at `~/src/rune` (`crates/rune/README.md` is the API guide,
  `crates/rune-kit/README.md` the components, `examples/` the patterns).
- Toolchain 1.99.1 (Rune's pin), via this folder's `rust-toolchain.toml`.

## Done when

Every GPUI file below has its Rune counterpart with the same behaviour, the
app builds with zero warnings (`cargo clippy -- -D warnings`), every area has
headless scenes that render, and `PARITY.md` (the GPUI app's gateway route
checklist) holds for this app too.

## Architecture (read before porting anything)

GPUI is retained: many `Entity<View>`s, each with its own state, wired by
subscriptions, `cx.notify()` and `cx.spawn`. Rune is a single state tree:

```rust
fn view(s: &State, cx: &mut Cx<State>) -> El   // re-runs after every handler
el().on_press(|s: &mut State| ...)              // handlers mutate the root state
```

So a port is a restructuring, not a translation:

| GPUI | Rune here |
|---|---|
| `struct FooView` + `Entity<FooView>` | `struct FooState` in a slot of `State` (`state.rs`) |
| `impl Render for FooView` | `pub fn view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El` |
| `cx.listener(|this, ev, window, cx| ..)` | `.on_press(move \|s: &mut State\| ..)` reaching the slot from the root |
| `cx.notify()` | nothing: the view re-runs after every handler and every async landing |
| `jobs::run(backend, cx, f, done)` | `crate::jobs::run(s, "label", f, \|s, result, tx\| ..)` |
| `cx.spawn` + timer | `jobs::after(s, delay, \|s, tx\| ..)`, or `tx.wake_after(d)` + `poll` |
| `cx.background_spawn` | `jobs::blocking(s, work, done)` |
| SSE stream receiver task | a thread/task forwarding each message with `s.rt.handle.update(..)` |
| events between entities (`EventEmitter`) | plain function calls: `crate::workspace::open_thread(s, id)` |
| `FocusHandle` | `TextEdit::focused` and `Tx::blur()` (see "Text fields" in Rune's README) |
| `InputState` | `rune::TextEdit` in state + `rune_kit::TextInput`/`TextArea` |
| popovers, menus, tooltips | rune-kit `Popover`, `Menu`, `context_menu`, `Tooltip`, `Select`, `Combobox`; overlays go into `layer` |
| `uniform_list`/`list` | `.scroll(..)` + `.virtualize(..)`/`virtual_rows` (see "Virtual rows") |
| `TextView::markdown` | `rune_kit::Markdown` (feature on; streaming-aware) |
| animations (`with_animation`) | `.motion(spring)`, `.transition(..)`, growth stacks; nothing animates on the first frame |
| `IconName::ChevronDown` | `rune::icon(&crate::icons::CHEVRON_DOWN, size).color(c)` |
| `Palette::of(cx)` | `s.palette` (`design::Palette`, same tokens, Rune `Color`) |
| `Prefs::update(cx, ..)` | `s.update_prefs(\|p\| ..)` |
| GPUI actions + key bindings | `workspace::on_key` (root `AppSpec::on_key`), native menu items |

Rules:

1. **Slots, not globals.** Each area owns its sub-state type and only its own
   files. The foundation already declared every slot and module (stubs say
   `FOUNDATION STUB`). Replace a stub's body, keep its public functions'
   names and signatures (others call them), add anything you need.
2. **Async replies check they still apply.** `done` gets the whole `State`;
   look up your slot again and compare ids or a `jobs::Generation` before
   writing, because the user may have switched thread or page meanwhile.
3. **No `.unwrap()`/`.expect()`** outside tests; errors go to the log with
   `tracing::warn!` and to the UI as a message, as in the GPUI app.
   `info!` is for user-facing status only.
4. **Keys.** Every element that animates, is clicked, or appears in a list
   gets a stable `.key(..)` (thread id, row id). Rune diffs by key.
5. **Files stay under ~800 lines** (Rune's own rule); split by capability as
   the GPUI app does.
6. **Headless checks.** Add fixture scenes to `scenes.rs` (one line each,
   fixtures live in your module behind `pub fn fixture_*`), set
   `s.fixture = true` in them, and check your views with
   `../target/debug/zlo-rune --snap out.png --scene NAME --size 1280x820`
   (add `--tree` for the element tree). Look at the PNGs. Write unit tests
   with `rune::Runtime` for interactions that matter (see `examples/hello`).
7. **Behaviour parity first, then look.** Same features, same gateway calls,
   same copy. Match the GPUI layout and design tokens; Rune's motion
   replaces GPUI's hand-rolled animation helpers where it does the same job.
8. Cross-area calls you need but that do not exist yet: add a stub with the
   right signature in the other area's module, mark it
   `// PORT-STUB(area X): ...` and list it in your final report. The
   integrator resolves them.

## Areas

Each area is one delegate in its own worktree (branch `rune-port/<area>`),
merged back by the integrator. (Git cannot create `rune-port/<area>` beside a `rune-port` branch, so area branches are `rune-port-<area>`.) GPUI paths are under `tools/ironclaw_gpui/app/src/`.

| Area | GPUI files (lines) | Rune files |
|---|---|---|
| **0 Foundation** (done by the integrator) | `main.rs`, `design/mod.rs`, `design/prefs.rs`, `design/themes.rs`, `jobs.rs` (helpers), connect logic of `workspace.rs` | `main.rs`, `state.rs`, `jobs.rs`, `design/{mod,prefs,themes}.rs`, `icons.rs`, `scenes.rs`, stubs |
| **A Chat core** | `chat.rs`, `chat/{flow,steer,menu,file_links,inline_files,visual,outline,pace,plan_strip,snapshot,question_form,approval_mode,approval_reason,reject_feedback}.rs` (~8.6k) | `chat.rs`, `chat/*` except H and S files |
| **H Chat extras** | `chat/{model_picker,goals,orchestration,computer}.rs` (~4.3k) | same names |
| **S Side pane** | `chat/side.rs`, `chat/side/*` (14 files), `native_view.rs` (~7.6k) | same names |
| **B Workspace** | `workspace.rs`, `workspace/*` (7 files), `alerts.rs`, `drafts.rs`, `titles.rs`, `run_clock.rs`, `window_state.rs`, `logs.rs`, menus and key bindings of `main.rs` (~8.9k) | `workspace.rs`, `workspace/*`, same small modules |
| **C Pages I** | `pages/{providers,providers/catalog,extensions,operator,operator/*,skills,hooks}.rs` (~5.7k) | `pages/` same names |
| **D Pages II** | `pages/{settings,settings/* except voice,automations,automations/*,notifications,files,projects}.rs`, `pages/mod.rs` helpers (~6k) | `pages/` same names, `pages/common.rs` |
| **E Pages III** | `pages/{profiles,profiles/pickers,memory,memory_draft,usage,usage_history,usage_compactions}.rs` (~5.8k) | `pages/` same names |
| **F Platform** | `voice/*` (14 files), `pages/settings/voice.rs`, `snapshot/*` (6 files), `permissions.rs`, `login_item.rs`, `service.rs` (~8.4k) | `voice/*`, `snapshot/*`, same small modules |
| **G Look** | `design/{battery,brands,dock,flip,kit,motion,paint,panels,pulse,smooth_scroll,styles,wallpaper}.rs`, `assets.rs`, `ui.rs` (~5.2k) | `design/look.rs` + `design/*`, shared widgets in `design/kit.rs` |

Not ported: `vendor/` (GPUI patches; Rune does its own frame pacing and
scrolling). `bundle.sh` has its own Rune version here (`ZLO Rune.app`).

## Phases

1. **Foundation** — done: the crate builds, connects to the gateway, routes
   pages, renders headless scenes.
2. **Parallel ports** — areas A, B, C, D, E, F, G, H, S at once, one delegate
   each, in worktrees.
3. **Integration** — merge every branch, resolve `PORT-STUB`s, replace the
   areas' local helpers with G's shared `design/kit.rs` where they overlap,
   zero warnings under clippy, scenes for every page, `bundle.sh` for a
   `ZLO Rune.app`.
4. **Parity review** — walk `PARITY.md` and the GPUI file list; a review
   delegate compares each GPUI file against its port and lists gaps; fix
   them.

## Status

| Area | State |
|---|---|
| 0 | done |
| E, F, H, G, D, C, S, B, A | merged (07.11.2026) |
| Integration | done: all areas wired; builds clean under clippy `-D warnings`, 191 tests |
| Parity review | done: `PARITY_REVIEW.md` (of 121 GPUI files: 96 parity, 19 minor gaps, 3 major, 2 missing) |
| P0 + all eight P1 fixes | merged (07.11.2026) |
| P2 polish (12 of 15 open), live test from `ZLO Rune.app` | open |

## Integration checklist (collected from area reports)

Ticked items are done and checked in code; open ones say why.

- [x] B: `voice::start(s)` and `snapshot::start(s)` from `workspace::start`; "Try again" calls `crate::service::start()` first (B merge).
- [x] A/B: `chat::{voice_composer, voice_composer_mut, voice_press_enter, stage_snapshot}` on A's state (Chat → the open chat or its side chat, Draft → B's new-thread field); chat and draft send handlers take `voice::take_submission` and report accepted/queued/refused (the draft's claim rides into `chat::send_text`); `voice::run_ended` on run end. SnapShot routes through `workspace::stage_snapshot` (settings → threads, draft or chat).
- [x] B/F: quick composer voice (parity review P1-4). The field stays in the quick window; the main state keeps a mirror the quick window's `poll` updates, voice edits run on the mirror and, as the same edit, on the window's field via `AppHandle::update`; the controls are `voice::ui::draw` over a `Controls` copy the main `poll` sends. `voice/session.rs::resolve` uses `workspace::voice_composer` (quick first, `open_quick` opens it); Enter goes through `quick::send`, which takes talk mode's claim and refuses an empty send. Design notes at the top of `workspace/quick.rs`.
- [x] Bundle (parity review P1-6): `bundle.sh` → `ZLO Rune.app`, id and login-item label `com.numoh.rune`; Settings, voice and permission copy name ZLO Rune.
- [ ] D: local page helpers in `voice/settings.rs` → `pages::common` (refactor only; behaviour is right). `pages/usage/common_stub.rs` is gone (5e12d99d7).
- [x] G: `design::brands::provider_logo` in profiles pickers, usage history and Providers (`pages/providers/logos.rs` deleted).
- [x] G: shared hover/row/floating helpers (`pages::common::controls`); mono memory and SKILL.md editors via `design::kit::mono_editor` on Rune's core `text_input` (17ba43bd8).
- [x] H: `chat::model_picker::pretty_model` in `common_stub.rs`; `provider_mark` in the profiles pickers.

No `PORT-STUB` markers remain (`rg -n PORT-STUB app/src` is empty).
`chat/extras_ui.rs` is a one-line re-export of `design::kit::compact` kept for
`chat.rs`, `chat/goals.rs` and `chat/orchestration/view.rs`; point those at
`design::kit::compact` and delete it.
- [x] A: `chat::approval_mode::{CHOICES, label, key}` in profiles pickers; `approval_mode_changed` from Profiles and Settings.
- [x] A: H's `extras_preview` gone; the real column keeps `orchestration::{composer, tail, tail_summary}`, `computer::view`, `model_picker::pills`; H's hooks (`open_error` for `set_error`, `reload_timeline`, `active_run`, `embedded`, `composer_text`, `set_composer_text`, `set_fork_prefix`, `set_merges`, `stop_run`; picker changes via `chat::picker_changed`); `/goal` via `goals::goal_command`/`run_command`; `orchestration::refresh` on run end and `ensure_poll` otherwise; `orchestration::has_tail` folds the tail row.
- [x] B: `workspace::{thread_title, ensure_titles, window_active}`; ⇧⌘M → `model_picker::open`; the draft screen uses `model_picker::DRAFT` (B merge).
- [x] F→H: `computer.rs` uses `crate::permissions`.
- [x] G: `model_picker::provider_mark` draws `design::brands::provider_logo`; `App::modifier_keys()` is on (`main.rs`).
- [x] G: `chat/extras_ui.rs` folded into `design::kit::compact` (5e12d99d7; the re-export shim above is the leftover).
- [x] B: `look::wallpaper_layer` in window/conversation/sidebar, `look::reshuffle` on a new draft (B merge).
- [ ] B: keep `s.palette`/`s.system_dark` current on a live appearance change; drop G's `LookState::preview` fixture hook. Needs a live window to check.
- [x] D: `look::reload(s)` after wallpaper/theme changes (Appearance).
- [ ] All areas: replace remaining local helpers (letter tiles) with `design::kit`/`design::motion` (`common_stub.rs`, `c_widgets.rs`, `extras_ui.rs` done in 5e12d99d7); composers share `.matched(COMPOSER_MATCH)` (rows `.motion(ROW_RESORT)` and the dock tab pill are done).
- [x] Foundation: `prefs.translucent` → transparent window at launch (`main.rs`, `design::styles::set_window_translucent`), system material on the shell root; turning it on mid-session takes effect at the next launch (Appearance says so). Live check under "Manual checks".
- [x] B: session import after connecting (`workspace::start_polling` ticks it and the rescan; D's `session_import::schedule` is not needed); titles API is one (`crate::titles`).
- [x] A: `chat::approval_mode_changed` (D and E callers).
- [ ] F: move local page helpers to `pages::common` (refactor only; C and E done in 5e12d99d7).
- [x] Kit: primary buttons use the accent: `design::kit()` sets rune-kit's contrast/submit tokens from the accent (8afe83ba8).
- [x] C: `pages/c_widgets.rs` → `pages::common` (5e12d99d7); Providers' catalog thread from the real inbox is done (`workspace::show`); mono SKILL.md editor (17ba43bd8).
- [x] A: a failed run's Logs button → `workspace::open_run_logs` → `pages::operator::show_run_logs`.
- [x] S: `chat/side/bridge.rs` wired to the real chat (rows, running, stop, gates, reject editor, composer insert, attach, CLI diff/review, timeline, profile), H (model label, tasks, lineage, cancel, fork, refresh, computer share) and titles; `pub use changes::on_cli_report`; `side/fixtures.rs::chat_view` gone (side scenes fill the real chat).
- [x] A: `side::init` after open, `side::docks` beside a `.grow(1).min_w(0)` column, `side::on_turn_started/ended`, `side::open_link`, `side::adopt_folder` from mention roots, `side::on_cli_report` when the CLI diff or review moves. Side chats: `side::SideChat` owns an embedded `ChatState` (`chat::build_side_chat` + `start_side_chat`), drawn with `chat::embedded_view`.
- [x] B: `side::titlebar_actions` in the breadcrumb (via `chat::titlebar_actions`), ⌥⌘B/⌘E/⌥⌘N; `side/dock.rs::viewport` reads the shell's `window` layout.
- [x] B: native views hide while the overlay layer is non-empty (`native_view::cover` from `workspace/shell.rs`; unit test). Live check under "Manual checks".
- [ ] B: `side::dock_widths` for the wallpaper area.
- [x] Wheel glide: `design::smooth_scroll` (`.wheel_glide(key)` on every vertical viewport) restores GPUI's `smooth_scroll.rs` feel for mouse wheels.
- [ ] Live test (from `ZLO Rune.app`): native views (QuickLook, WKWebView), system notifications, window-frame restore, voice (including dictation into the quick composer from another app) and SnapShot in a real window.

### Manual checks (cannot run headless)

- **Native views under overlays:** open an HTML or PDF file in the side
  pane, then press ⌘K, open the model picker, and delete a thread from a
  sidebar row's menu (the confirm dialog). The preview must vanish while
  each shows and come back when it closes.
- **Translucent window:** turn on Settings → Appearance → Translucent
  window; the row adds "Takes effect when ZLO next opens." Relaunch: the
  desktop shows blurred through the sidebar and, fainter, the panel. Turn
  it off: the window turns opaque at once.
- **Wheel glide:** with a notched mouse, one tick in the transcript, the
  inbox and a settings page glides about 40 pt over ~0.2 s, and a fast spin
  is one motion; a trackpad still tracks the fingers 1:1; ⌘ + wheel is not
  smoothed.
- [x] B merge: stubs B wrote into F/G/H/E files replaced by adapters onto the real APIs.
- [ ] B: `workspace/inbox{,/view}.rs` over 800 lines (split).
- [x] Clippy: `SCENES` uses a `Scene` type alias; `cargo clippy --all-targets -- -D warnings` is clean.
- [ ] Interaction tests (`rune::Runtime`) for voice controls, settings and the chat composer.

### Known gaps after integration

- Side chats are full chats, extras (H) included: H's modules address a
  chat by thread id (`chat::chat_for`, `model_picker::Slot::Chat`), so a
  side chat has its own model picker, goal, queue (⌥Enter), fork and
  computer share (fixed in ad52f3c35). The docks (S) stay the open chat's,
  as in GPUI.
- `#![allow(dead_code)]` in `main.rs` still hides unused port items; drop it
  once the parity review is through.

### Known inaccuracy in the shared spec (`tools/ironclaw_gpui/PARITY.md`)

`PARITY.md` marks `operator.run_setup` and `get_user_model_preference` done,
but neither app calls them: `Client::run_operator_setup` and
`Client::model_preference` have no caller in GPUI or here (the Setup panel only
reads the state; the model preference is only written). The GPUI app is the
spec and is not edited from this port, so the line stays wrong there until a
GPUI change calls them or marks them read-only (parity review P2-15).
