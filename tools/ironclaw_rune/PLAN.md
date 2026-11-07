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
| E, F, H, G, D | merged (07.11.2026) |
| A B C S | in progress |

## Integration checklist (collected from area reports)

- [ ] B: call `voice::start(s)` and `snapshot::start(s)` from `workspace::start`; "Try again" on the connection screen calls `crate::service::start()` first.
- [ ] A/B: fill `chat.rs` PORT-STUBs `voice_composer`, `voice_composer_mut`, `voice_press_enter`, `stage_snapshot` (Chat → A, Draft/Quick → B); send handlers call `voice::take_submission` and report accepted/queued/refused; `voice::run_ended` on run end.
- [ ] B: quick composer first in `voice/session.rs::resolve` (PORT-STUB).
- [ ] D: replace local page helpers in `pages/usage/common_stub.rs` and `voice/settings.rs` with `pages::common`.
- [ ] G: `design::brands::provider_logo` in profiles pickers; shared hover/row/floating helpers; mono font for the memory editor.
- [ ] H: `chat::model_picker::pretty_model` in `common_stub.rs`.
- [ ] A: `chat::approval_mode::{CHOICES, label, key, changed}` in profiles.
- [ ] A: replace H's `extras_preview` placeholder column in `chat.rs` with the real chat, keeping its calls (`orchestration::tail/composer`, `computer::view`, `model_picker::pills`); fill H's hooks in `chat.rs` (`set_error`, `resubscribe`, `reload_timeline`, `active_run`, `embedded`, `composer_text`, `set_composer_text`, `set_fork_prefix`, `set_merges`, `stop_run`); `/goal` via `goals::goal_command`/`run_command`; `orchestration::refresh` on run end.
- [ ] B: `workspace::{thread_title, ensure_titles, window_active}` (H stubs); ⇧⌘M → `model_picker::open`; draft screen uses `ModelPickerState::new/load/pills_for`.
- [ ] F→H: `computer.rs` local `permissions` stub → `crate::permissions`.
- [ ] G: `model_picker::provider_mark` logos; fold `chat/extras_ui.rs` into `design::kit`; enable `App::modifier_keys()` for ⌘-digit badges.
- [ ] B: `look::wallpaper_layer(s, area)` as the last child of window/conversation/sidebar/pane; `look::reshuffle` on new chat; keep `s.palette`/`s.system_dark` current on appearance change; drop G's `LookState::preview` fixture hook once the shell places the wallpaper.
- [ ] D: `look::reload(s)` after wallpaper/theme changes; pickers from `wallpaper::AREAS`, `paint::SCENES`, `paint::EFFECTS`.
- [ ] All areas: replace local helpers (`common_stub.rs`, `chat/extras_ui.rs`, letter tiles) with `design::kit`, `design::brands::provider_logo`, `design::motion`; composers share `.matched(COMPOSER_MATCH)` (dock), rows `.motion(ROW_RESORT)` (flip).
- [ ] Foundation: transparent window when `prefs.translucent` so the system material shows.
- [ ] B: call `pages::settings::session_import::schedule(s)` after connecting; implement D's stubs `workspace::{titles_ensure, title_of, set_unread}` together with H's `thread_title/ensure_titles` (one API).
- [ ] A: `chat::approval_mode_changed` (D and E stubs).
- [ ] C, E, F: move local page helpers to `pages::common` (now merged).
- [ ] Kit: primary buttons use the accent (rune-kit `Submit` renders white); `design::kit::button(..).primary()` is the reference.
- [ ] Foundation: `prefs.translucent` toggles the window transparency (Appearance only saves the pref).
- [ ] Clippy: `SCENES` type complexity in `scenes.rs`.
- [ ] Interaction tests (`rune::Runtime`) for voice controls and settings.
