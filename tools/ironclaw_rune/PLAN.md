# ZLO on Rune: the port plan

Started 07.11.2026. A full rewrite of the GPUI app (`tools/ironclaw_gpui/app`,
121 files, ~55k lines) on [Rune](https://github.com/anomalyco/rune), file by
file. **The GPUI app is not touched**: if Rune disappoints, we go back to it.

- Binary: `zlo-rune` (`cargo run --release` here). The GPUI app stays `zlo`.
- Shared: the GUI-free gateway client `tools/ironclaw_gpui/client` (path
  dependency, unchanged) and the preferences file
  (`~/Library/Application Support/ZLO/prefs.json`, same format).
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
scrolling), `bundle.sh` (a Rune bundle script comes with integration).

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
| Integration | in progress: everything builds and renders wired; open items below |

## Integration checklist (collected from area reports)

Ticked items are done and checked in code; open ones say why.

- [x] B: `voice::start(s)` and `snapshot::start(s)` from `workspace::start`; "Try again" calls `crate::service::start()` first (B merge).
- [x] A/B: `chat::{voice_composer, voice_composer_mut, voice_press_enter, stage_snapshot}` on A's state (Chat → the open chat or its side chat, Draft → B's new-thread field); chat and draft send handlers take `voice::take_submission` and report accepted/queued/refused (the draft's claim rides into `chat::send_text`); `voice::run_ended` on run end. SnapShot routes through `workspace::stage_snapshot` (settings → threads, draft or chat).
- [ ] B: quick composer first in `voice/session.rs::resolve` (PORT-STUB kept). The quick composer's `TextEdit` lives in its own window's state (`QuickState`, another `AppHandle`), so `voice_composer{,_mut}` cannot hand it out; live dictation with a preedit there needs the field moved into `State` (or a voice target that edits through `AppHandle::update`). Decision for B/F.
- [ ] D: local page helpers in `pages/usage/common_stub.rs` and `voice/settings.rs` → `pages::common` (refactor only; behaviour is right).
- [x] G: `design::brands::provider_logo` in profiles pickers, usage history and Providers (`pages/providers/logos.rs` deleted).
- [ ] G: shared hover/row/floating helpers; mono font for the memory and SKILL.md editors (PORT-STUB in `pages/memory.rs`): rune-kit's `TextArea` takes no text style, so this needs a Rune change.
- [x] H: `chat::model_picker::pretty_model` in `common_stub.rs`; `provider_mark` in the profiles pickers.

Remaining `PORT-STUB` markers (`rg -n PORT-STUB app/src`), each an open item
below: `voice/session.rs` (quick composer voice target), `pages/memory.rs`
(mono editor font needs Rune), `pages/usage/common_stub.rs` and
`pages/c_widgets.rs` (helper consolidation into `pages::common` and
`design::kit`).
- [x] A: `chat::approval_mode::{CHOICES, label, key}` in profiles pickers; `approval_mode_changed` from Profiles and Settings.
- [x] A: H's `extras_preview` gone; the real column keeps `orchestration::{composer, tail, tail_summary}`, `computer::view`, `model_picker::pills`; H's hooks (`open_error` for `set_error`, `reload_timeline`, `active_run`, `embedded`, `composer_text`, `set_composer_text`, `set_fork_prefix`, `set_merges`, `stop_run`; picker changes via `chat::picker_changed`); `/goal` via `goals::goal_command`/`run_command`; `orchestration::refresh` on run end and `ensure_poll` otherwise; `orchestration::has_tail` folds the tail row.
- [x] B: `workspace::{thread_title, ensure_titles, window_active}`; ⇧⌘M → `model_picker::open`; the draft screen uses `model_picker::DRAFT` (B merge).
- [x] F→H: `computer.rs` uses `crate::permissions`.
- [x] G: `model_picker::provider_mark` draws `design::brands::provider_logo`; `App::modifier_keys()` is on (`main.rs`).
- [ ] G: fold `chat/extras_ui.rs` into `design::kit` (refactor only).
- [x] B: `look::wallpaper_layer` in window/conversation/sidebar, `look::reshuffle` on a new draft (B merge).
- [ ] B: keep `s.palette`/`s.system_dark` current on a live appearance change; drop G's `LookState::preview` fixture hook. Needs a live window to check.
- [x] D: `look::reload(s)` after wallpaper/theme changes (Appearance).
- [ ] All areas: replace remaining local helpers (`common_stub.rs`, `pages/c_widgets.rs`, `chat/extras_ui.rs`, letter tiles) with `design::kit`/`design::motion`; composers share `.matched(COMPOSER_MATCH)`, rows `.motion(ROW_RESORT)`. Refactor; PORT-STUB notes in `common_stub.rs` and `c_widgets.rs` mark the spots.
- [ ] Foundation: `prefs.translucent` → transparent window so the system material shows (Appearance only saves the pref). Needs a live window.
- [x] B: session import after connecting (`workspace::start_polling` ticks it and the rescan; D's `session_import::schedule` is not needed); titles API is one (`crate::titles`).
- [x] A: `chat::approval_mode_changed` (D and E callers).
- [ ] C, E, F: move local page helpers to `pages::common` (refactor only).
- [ ] Kit: primary buttons use the accent (rune-kit `Submit` renders white; the chat's own cards show it); `design::kit::button(..).primary()` is the reference.
- [ ] C: `pages/c_widgets.rs` → `pages::common` + `design::kit`; Providers' catalog thread from the real inbox is done (`workspace::show`); mono SKILL.md editor (see G).
- [x] A: a failed run's Logs button → `workspace::open_run_logs` → `pages::operator::show_run_logs`.
- [x] S: `chat/side/bridge.rs` wired to the real chat (rows, running, stop, gates, reject editor, composer insert, attach, CLI diff/review, timeline, profile), H (model label, tasks, lineage, cancel, fork, refresh, computer share) and titles; `pub use changes::on_cli_report`; `side/fixtures.rs::chat_view` gone (side scenes fill the real chat).
- [x] A: `side::init` after open, `side::docks` beside a `.grow(1).min_w(0)` column, `side::on_turn_started/ended`, `side::open_link`, `side::adopt_folder` from mention roots, `side::on_cli_report` when the CLI diff or review moves. Side chats: `side::SideChat` owns an embedded `ChatState` (`chat::build_side_chat` + `start_side_chat`), drawn with `chat::embedded_view`.
- [x] B: `side::titlebar_actions` in the breadcrumb (via `chat::titlebar_actions`), ⌥⌘B/⌘E/⌥⌘N; `side/dock.rs::viewport` reads the shell's `window` layout.
- [ ] B: `s.native.covered` while overlays show; `side::dock_widths` for the wallpaper area. Need a live window to see native views.
- [ ] Live test: native views (QuickLook, WKWebView), system notifications, window-frame restore, voice and SnapShot in a real window.
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
- The voice target for the quick composer (see above).
- `#![allow(dead_code)]` in `main.rs` still hides unused port items; drop it
  once the parity review is through.
