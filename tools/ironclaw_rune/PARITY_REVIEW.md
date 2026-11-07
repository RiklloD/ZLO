# ZLO on Rune: parity review against the GPUI app

Reviewed 07.11.2026 on `rune-port` at `fe0ea85c8`. GPUI paths are under
`tools/ironclaw_gpui/app/src/`, Rune paths under `tools/ironclaw_rune/app/src/`.

## How this was checked

The review compares behaviour, not code shape. Every file got these checks:

- **Gateway calls.** All 160 `async fn`s of `ironclaw_gpui_client` were matched
  call by call. For each one, the GPUI files that call it were set against the
  Rune files that call it. `Backend::subscribe` and `inspector_updates` were
  checked the same way.
- **Copy.** Every user-visible string literal in each GPUI file (tests left
  out) was searched for in the Rune tree.
- **Functions.** Every GPUI `fn` name was searched for in the Rune tree. Each
  name with no match was traced by hand: most turned out to be renames
  (`side_*` became `side::*`, `send_draft_as` became `draft::send`, and so on).
- **Settings and timers.** The `Prefs` fields, key bindings, menus and timer
  constants were diffed.
- **Async replies.** Every `jobs::run`/`blocking`/`after` call was checked for
  a thread-id or `Generation` guard (PLAN rule 2).
- **Stubs and panics.** Searched for `PORT-STUB|todo!|unimplemented!|FIXME|XXX`
  and for `.unwrap()`/`.expect()` outside tests.
- **Visuals.** `cargo build` succeeds, and all 64 scenes in `scenes.rs` render
  headless at 1280x820 without errors. I looked at `chat-gate`, `providers`,
  `settings`, `inbox` and `model-picker` against the GPUI render code.

The risky areas got a closer read: chat stream and timeline, composer keys,
connect and polling, quick composer, drafts, side session, routines, browser,
orchestration guards, skills and the dropped `design/*` files. The rest was
judged on the checks above. A few things need a real window and could not be
checked headless: native views, notifications, window-frame restore, voice and
SnapShot.

## 1. Summary verdict

**Most of the parity claim holds.** Gateway calls match, and the copy is
essentially identical. Async replies are guarded almost everywhere: chats are
looked up again by thread id (`chat_for`, `same_thread`, `side_of`), and drafts
and streams use `Generation`s. There is no `todo!`/`unimplemented!`, and no
`.unwrap()`/`.expect()` outside tests (only `quick.rs`'s `unreachable_init`, a
plain fn Rune requires that never runs).

It falls short in a few concrete places:

- **The "system" color scheme is broken.** `s.system_dark` is never updated,
  so most of the app keeps the dark palette on a light Mac.
- **The new-thread screen lost four features:** the `@` file menu, inline file
  cards, pasting files or pictures, and dropping a thread onto it.
- **Dragging a thread to attach it is gone everywhere.** `chat::actions::attach_thread`
  exists but nothing calls it.
- **The quick composer has no voice.** It has no mic controls, and voice never
  types into it.
- **Native previews are never hidden under overlays.** `native.covered` is
  never set.
- **There is no app bundle for the Rune build.** Without one, notifications,
  voice, open-at-login and the permission prompts cannot work.
- **Three of the four dropped `design/*` files were dropped on a reason that
  does not hold.**

### Counts (121 GPUI files)

| Verdict | Files |
|---|---|
| parity | 96 |
| minor gaps | 19 |
| major gaps | 3 |
| missing | 2 (deliberately dropped, but the stated replacement is not in place) |
| dropped, reason holds | 1 (`design/pulse.rs`) |

## 2. Per-file table

Unless a row says otherwise, "parity" means every check in "How this was
checked" passed and nothing turned up in the reading.

### Area 0 and B: foundation and workspace

| GPUI file | Rune file(s) | Verdict | Gaps |
|---|---|---|---|
| `main.rs` | `main.rs`, `workspace/keys.rs` | minor gaps | **(a)** GPUI's System scheme follows macOS live (`main.rs:122-128` `observe_window_appearance` → `design::reapply`). Rune has no such hook; see `design/mod.rs`. **(b)** GPUI's native app menu has "Quit ZLO" (`main.rs:92`). Rune handles ⌘Q in `on_key` (`main.rs:69-74`) but adds no menu item. **(c)** The startup log line reads "ZLO (Rune) started" (fine). |
| `design/mod.rs` | `design/mod.rs`, `state.rs`, `workspace/shell.rs` | **major gaps** | **(a)** GPUI re-resolves the palette when the macOS appearance flips (`design/mod.rs` `update_appearance`/`reapply`). Rune's `s.system_dark` is set once to `true` (`state.rs:95`) and never written again (`rg "system_dark *="` finds nothing). Only the shell root re-resolves from `cx.system_dark()` (`workspace/shell.rs:25-29`); the 231 call sites that read `s.palette` keep the dark palette. With scheme "system" on a light Mac the window is a mix of light and dark. ⇧⌘Y (`workspace.rs:182-185`) also reads the stale value, so its first press does nothing visible. **(b)** GPUI's `set_translucent`/`apply_window` make the window transparent. Rune has no counterpart: Appearance only saves `prefs.translucent` (`pages/settings/appearance.rs:524`), as the PLAN checklist says. |
| `design/prefs.rs` | `design/prefs.rs` | parity | The `Prefs` struct matches the GPUI one exactly, so the file format is shared. |
| `design/themes.rs` | `design/themes.rs` | parity | |
| `jobs.rs` | `jobs.rs` | parity | GPUI's `done` ran only while its view lived (weak handle); Rune's always runs, so every caller guards itself (checked, see section 1). |
| `workspace.rs` | `workspace.rs`, `workspace/{shell,status,keys,ui}.rs` | minor gaps | **(a)** `toggle_scheme` uses the stale `system_dark` (`workspace.rs:183`). **(b)** GPUI's `restore_composer` (`workspace.rs:670`) refocuses only when nothing else has focus. Rune's `window_focus` (`workspace.rs:644-660`) always refocuses the composer when the window comes to the front. **(c)** GPUI hides native previews while the command menu shows (`workspace.rs:1544-1545` `frame_begin(window, command.is_some())`). Rune never sets `s.native.covered` (`native_view.rs:106`, read at `:194`; no writer), so QuickLook/WebKit views draw over the command menu and popovers. **(d)** The mouse-wheel glide is gone (`workspace.rs:1548`); see `design/smooth_scroll.rs`. Polling intervals (15 s unread, 10 s status, 15 min session-import rescan, 150 ms startup poll inside a 25 s grace), connect retries, `delete_thread` confirm-then-remove, alerts and `run_ended` all match. |
| `workspace/command.rs` | `workspace/command.rs` | parity | |
| `workspace/context.rs` | `workspace/context.rs` | parity | Replies are guarded by `active_id` (`:202`). |
| `workspace/draft.rs` | `workspace/draft.rs` | **major gaps** | **(a)** GPUI's draft has the `@` file menu with project-folder ranking (`draft.rs:36-38, 97-118, 482`). Rune has none; its header says it "joins this screen at integration" (`draft.rs:7-9`), and it never did. **(b)** GPUI places staged files as inline cards at the caret (`draft.rs:405` `inline_files::insert`). Rune shows chips above the text (`draft.rs:475, 535`). **(c)** GPUI accepts pasted files or pictures (`draft.rs:415` `paste_into_draft`). Rune's field has no `on_input_paste`. **(d)** Dropping a sidebar thread onto the draft attaches its transcript in GPUI (`draft.rs:343-389, 704-708`, `thread_attachment`). Rune has no drop target, and `thread_attachment` is never called from `workspace/`. Send, worktree switch, profile menu, voice refusal and the late-attach `Generation` are present. One copy change: "A new thread is still being created; try again." became "A new thread is already being created." (`draft.rs:223`). |
| `workspace/draft/snapshot.rs` | `workspace/draft/snapshot.rs` | parity | |
| `workspace/inbox.rs` | `workspace/inbox.rs`, `workspace/inbox/view.rs` | minor gaps | **(a)** GPUI rows are drag sources for `DraggedThread` (`inbox.rs:1446-1450`). Rune rows have no `on_drag`, so a thread can no longer be dragged into a chat or the draft. **(b)** GPUI resorts rows with FLIP motion. Rune never uses `motion::ROW_RESORT` (`design/motion.rs:79`). **(c)** The filter button's accessibility label is "Filters" in GPUI (`inbox.rs:939`); Rune has only the tooltip (`inbox/view.rs:186`). The context menu, shelves, Show more, ⌘1–9 badges and the snooze menu all render (`inbox` scene). The file is over 800 lines (PLAN rule 5). |
| `workspace/inbox/subagents.rs` | `workspace/inbox/subagents.rs` | parity | |
| `workspace/message_search.rs` | `workspace/message_search.rs` | parity | |
| `workspace/quick.rs` | `workspace/quick.rs` | **major gaps** | **(a)** GPUI's card has the voice controls (`quick.rs:124` `voice::ui::composer_controls`). Rune's card has none (`quick.rs:173-205`). **(b)** GPUI's Enter passes talk mode's claim with the text (`quick.rs:58-66` `take_submission` / `submission_refused`). Rune's `submit` (`quick.rs:159-171`) drops it, and `draft::quick_send(s, text)` (`draft.rs:154`) takes no claim. **(c)** GPUI voice types into the quick composer, and opens it when the main window is not in front (`workspace.rs:689-722`). Rune's `voice::session::resolve` skips it (`voice/session.rs:301` PORT-STUB) and brings the main window forward instead. **(d)** Placement is 180 px from the top in Rune versus a fifth of the screen height in GPUI (`quick.rs:192-195`); cosmetic. The global ⇧⌘Space, Esc, close on losing focus and send through the draft all match. |
| `alerts.rs` | `alerts.rs`, `alerts/macos.rs` | parity | Needs an app bundle to show anything, and there is no Rune bundle (P1-6). |
| `drafts.rs` | `drafts.rs` | parity | Same directory and format, the same 300 ms debounce, and the same write on quit. |
| `titles.rs` | `titles.rs` | parity | |
| `run_clock.rs` | `run_clock.rs` (+ caller `chat.rs:544`) | minor gaps | GPUI calls `run_clock::start` on every fold while a run is live (`chat.rs:930-937`). That records a run id that arrives late, picks up the inbox's `working_since`, and restarts the clock for a back-to-back run. Rune calls it only while `run_started` is `None` (`chat.rs:544`), so the "Weaving… 2m 33s" line can keep a stale start. |
| `window_state.rs` | `window_state.rs` | minor gaps | GPUI records the frame on every bounds change (`main.rs:136` `observe_window_bounds` → `record`). Rune saves it only in `on_close` (`workspace/keys.rs:49`), so a crash or kill loses moves and resizes. The file is the same `window.json`. |
| `logs.rs` | `logs.rs` | parity | Writes `rune.log` instead of `desktop.log`, on purpose. |

### Area A: chat core

| GPUI file | Rune file(s) | Verdict | Gaps |
|---|---|---|---|
| `chat.rs` | `chat.rs`, `chat/{actions,composer,rows,view,pending,glyphs}.rs` | minor gaps | **(a)** GPUI's transcript is a drop target for a dragged thread (`chat.rs:1567-1568` → `attach_thread`). Rune ported `actions::attach_thread` (`chat/actions.rs:58`), but nothing calls it and there is no drop target. **(b)** The `run_clock::start` call happens once only (`chat.rs:544`); see `run_clock.rs`. Everything else matches: the stream burst-drain with a `stream_gen` guard (`chat.rs:384-470`), `timeline_through`, ⌘↵/⇧⌘↵ approve, ↑ recall, `/` commands with `OpenThread`, "Live updates stopped", the run-end voice and alert flow, and reveal-message. |
| `chat/flow.rs` | `chat/flow.rs` | parity | |
| `chat/steer.rs` | `chat/steer.rs` | parity | |
| `chat/menu.rs` | `chat/menu.rs` | parity | The failure log "composer: mention chip not placed" is gone; Rune's pills have no failure path. |
| `chat/file_links.rs` | `chat/file_links.rs` | parity | |
| `chat/inline_files.rs` | `chat/inline_files.rs` | parity | The same note applies to the two debug log strings. |
| `chat/visual.rs` | `chat/visual.rs` | parity | |
| `chat/outline.rs` | `chat/outline.rs` | parity | |
| `chat/pace.rs` | `chat/pace.rs` | parity | |
| `chat/plan_strip.rs` | `chat/plan_strip.rs` | parity | |
| `chat/snapshot.rs` | `chat/snapshot.rs` | parity | |
| `chat/question_form.rs` | `chat/question_form.rs` | parity | |
| `chat/approval_mode.rs` | `chat/approval_mode.rs` | parity | |
| `chat/approval_reason.rs` | `chat/approval_reason.rs` | parity | |
| `chat/reject_feedback.rs` | `chat/reject_feedback.rs` | parity | |

### Area H: chat extras

| GPUI file | Rune file(s) | Verdict | Gaps |
|---|---|---|---|
| `chat/model_picker.rs` | `chat/model_picker.rs`, `chat/model_picker/view.rs` | parity | The provider-rail drag is ported (`view.rs:212`). Side chats get no picker; see `chat/side/session.rs`. |
| `chat/goals.rs` | `chat/goals.rs` | parity | |
| `chat/orchestration.rs` | `chat/orchestration.rs`, `chat/orchestration/view.rs` | parity | Every reply goes through `same_thread`. |
| `chat/computer.rs` | `chat/computer.rs`, `chat/computer/view.rs` | parity | |

### Area S: side pane

| GPUI file | Rune file(s) | Verdict | Gaps |
|---|---|---|---|
| `chat/side.rs` | `chat/side.rs`, `chat/side/{bridge,dock,ui}.rs` | parity | Dock drag, widths, tabs, badges and link routing are present. |
| `chat/side/session.rs` | `chat/side/session.rs` | minor gaps | GPUI side chats are full `ChatView`s, with their own model picker, goal, queue, fork-from-message and computer share. Rune draws them with `chat::embedded_view`: no picker pill (`chat/composer.rs:252-255`), no goal, no queue (⌥Enter steers instead), no fork or computer share. GPUI's Esc also closes a side chat's open picker (`focused_picker`, `session.rs:148`); Rune has no counterpart. All of this is listed under "Known gaps" in PLAN. |
| `chat/side/activity.rs` | same | parity | |
| `chat/side/agents.rs` | same | parity | |
| `chat/side/browser.rs` | same | minor gaps | The poll `tick` (`browser.rs:69-91`) reads whichever chat is open, not the one that started it. After a thread switch, a pending tick carries on into the new chat, and a second `start` there runs two loops. Harmless today (there is one WKWebView), but it is an unguarded timer. |
| `chat/side/changes.rs` | `chat/side/changes.rs`, `changes/{list,notes}.rs` | parity | |
| `chat/side/changes/codex.rs` | same | parity | |
| `chat/side/console.rs` | same | parity | |
| `chat/side/git.rs` | same | parity | |
| `chat/side/history.rs` | same | parity | |
| `chat/side/outputs.rs` | same | parity | |
| `chat/side/preview.rs` | same | parity | |
| `chat/side/project.rs` | same | parity | |
| `chat/side/routines.rs` | same | parity | The lists are global, so not guarding them is fine. Action errors go to whichever chat is open (`routines.rs:68, 109, 131`), the same as GPUI. |
| `native_view.rs` | `native_view.rs` | minor gaps | `covered` exists but is never set (see `workspace.rs` (c)). A live-window check is still open (PLAN). |

### Areas C, D, E: pages

| GPUI file | Rune file(s) | Verdict | Gaps |
|---|---|---|---|
| `pages/mod.rs` | `pages/mod.rs`, `pages/common.rs` | parity | |
| `pages/providers.rs` | `pages/providers.rs`, `providers/{view,fixtures}.rs` | parity | |
| `pages/providers/catalog.rs` | same | parity | |
| `pages/extensions.rs` | `pages/extensions.rs`, `extensions/view.rs` | parity | |
| `pages/operator.rs` | `pages/operator.rs`, `operator/status.rs` | parity | |
| `pages/operator/config.rs` | same | parity | |
| `pages/operator/extensions.rs` | same | parity | |
| `pages/operator/inspector.rs` | `operator/inspector.rs`, `inspector/view.rs` | parity | |
| `pages/operator/logs.rs` | same | parity | |
| `pages/operator/setup.rs` | same | parity | Reads the setup state only, as GPUI does; see section 3 for `run_setup`. |
| `pages/skills.rs` | same | minor gaps | GPUI's SKILL.md editor uses the mono font; Rune's does not, because rune-kit's `TextArea` takes no text style (PLAN). |
| `pages/hooks.rs` | same | parity | |
| `pages/settings.rs` | same | minor gaps | The "Open at login" copy still points at `tools/ironclaw_gpui/bundle.sh` (`pages/settings.rs:189`). The Rune app has no bundle script (P1-6). |
| `pages/settings/appearance.rs` | same | minor gaps | The Translucent switch saves the pref but does nothing (`appearance.rs:524`; PLAN open item). |
| `pages/settings/compaction.rs` | same | parity | |
| `pages/settings/session_import.rs` | same (+ ticks in `workspace.rs:510-535`) | parity | The 15-minute rescan is kept. |
| `pages/settings/snapshots.rs` | same | parity | |
| `pages/settings/tools.rs` | same | parity | |
| `pages/settings/traces.rs` | same | parity | |
| `pages/automations.rs` | same | parity | |
| `pages/automations/form.rs` | same | parity | |
| `pages/automations/schedule_rows.rs` | same | parity | |
| `pages/notifications.rs` | same | parity | |
| `pages/files.rs` | same | parity | Mount, path and selection are guarded. |
| `pages/projects.rs` | same | parity | |
| `pages/profiles.rs` | same | minor gaps | The hover button's accessibility label "New thread on this profile" (GPUI `profiles.rs:507`) is missing; the action works. |
| `pages/profiles/pickers.rs` | same | parity | |
| `pages/memory.rs` | `pages/memory.rs`, `memory/supermemory.rs` | minor gaps | **(a)** The memory editor's mono font is missing (`memory.rs:523` PORT-STUB, needs a Rune change). **(b)** The archive button's accessibility label "Remove document" (GPUI `memory.rs:710`) is missing. |
| `pages/memory_draft.rs` | `pages/memory/draft.rs` | parity | |
| `pages/usage.rs` | same | parity | |
| `pages/usage_history.rs` | `pages/usage/history.rs` | parity | |
| `pages/usage_compactions.rs` | `pages/usage/compactions.rs` | parity | |

### Area F: platform

| GPUI file | Rune file(s) | Verdict | Gaps |
|---|---|---|---|
| `voice/mod.rs` | `voice/mod.rs`, `voice/session.rs`, `voice/settings.rs` | minor gaps | The quick composer is never a voice target (`voice/session.rs:301` PORT-STUB); see `workspace/quick.rs`. |
| `voice/apple.rs` | same | minor gaps | The refusal copy points at `tools/ironclaw_gpui/bundle.sh` (`apple.rs:97`). With no Rune bundle, voice cannot work in this app (P1-6). |
| `voice/composer.rs`, `voice/coordinator.rs`, `voice/engine.rs`, `voice/mic.rs`, `voice/ptt.rs`, `voice/speakable.rs`, `voice/transcript.rs`, `voice/turn.rs`, `voice/ui.rs`, `voice/wake.rs` | same names | parity (10 files) | The logic is a near copy; the line counts match. |
| `pages/settings/voice.rs` | `voice/settings.rs` | parity | |
| `snapshot/mod.rs` | same | parity | |
| `snapshot/ax.rs`, `snapshot/capture.rs`, `snapshot/cf.rs` | same | parity (3 files) | |
| `snapshot/hotkey.rs` | same | parity | The HIToolbox registration was replaced by Rune's global shortcut, on purpose. |
| `permissions.rs` | same | parity | |
| `login_item.rs` | same | minor gaps | It uses the same LaunchAgent label as GPUI, `com.numoh.desktop` (`login_item.rs:9`). Turning on "Open at login" in one app overwrites the other's plist, and it only works from a `.app`, which Rune does not build (P1-6). |
| `service.rs` | same | parity | Some `info!` became `debug!`, which is correct under the logging rule. Both apps own the same backend, so quitting either one stops it for both (by design, but worth knowing while the two run side by side). |

### Area G: look

| GPUI file | Rune file(s) | Verdict | Gaps |
|---|---|---|---|
| `design/battery.rs` | same | parity | |
| `design/brands.rs` | same | parity | |
| `design/kit.rs` | `design/kit.rs`, `design/kit/loops.rs` | minor gaps | Primary buttons render white instead of the accent. The `chat-gate` scene shows "Approve ⌘↵" white; GPUI's kit primary uses the accent. This is an open PLAN item; the local helpers in `pages/c_widgets.rs`, `pages/usage/common_stub.rs` and `chat/extras_ui.rs` are not folded in yet. |
| `design/motion.rs` | `design/motion.rs` | parity | |
| `design/paint.rs` | same | parity | |
| `design/panels.rs` | same | parity | |
| `design/styles.rs` | same | parity | |
| `design/wallpaper.rs` | same | parity | |
| `design/dock.rs` | (dropped) | **missing** | The stated replacement is `El::matched(COMPOSER_MATCH)` on both composers (`design/motion.rs:21, 86-88`). `COMPOSER_MATCH` is defined and never used (`rg "matched\("` finds no call), so the hero composer no longer glides into the dock when a thread opens. |
| `design/flip.rs` | (dropped) | minor gaps | Tab and pill slides are done with `.motion(TAB_SLIDE/PILL)` (`design/kit.rs:555, 583`). The resorted-row glide (`motion::ROW_RESORT`, `motion.rs:79`) is never used, so inbox rows jump when they resort. |
| `design/pulse.rs` | (dropped) | dropped, reason holds | Loops use `El::looping`/`El::cycle` (14 call sites), and Rune's clock runs frames only while they are visible. |
| `design/smooth_scroll.rs` | (dropped) | **missing** | The stated reason, "Rune scrolls smoothly itself (README 'Smooth scrolling')" (`design/motion.rs:23`), does not hold. That README section is about commit cost, and Rune's scroll is "wheel 1:1" (`~/src/rune/crates/rune/README.md:327`). Mouse-wheel notches jump again, where GPUI glides them app-wide (`workspace.rs:1547-1548`). |
| `assets.rs` | `icons.rs`, `design/brands.rs` | parity | |
| `ui.rs` | `rune`/`rune_kit` directly, `rune_kit::Markdown` | parity | |

## 3. PARITY.md route check

Every route marked `done` in `tools/ironclaw_gpui/PARITY.md` was mapped to its
client call and searched for in the Rune app.

| Route group | Rune callers | Result |
|---|---|---|
| get_session, list_threads, create_thread, delete_thread | `session` (`workspace.rs`, `voice/mic.rs`), `list_threads` (`automations/form.rs`, `operator/inspector.rs`), `create_thread` (`workspace/draft.rs`, `automations/form.rs`), `delete_thread` (`workspace.rs`) | ok |
| session_channel_message, get_timeline, stream_events | `send_message`/`send_message_with_roots` (`chat/actions.rs`, `chat/steer.rs`, `workspace/context.rs`), `timeline`/`timeline_through` (`chat.rs`), `Backend::subscribe` (`chat.rs:394`) | ok |
| cancel_run, resolve_gate, retry_run | `chat/actions.rs`, `chat/steer.rs`, `chat/reject_feedback.rs`, `chat/side/activity.rs` | ok |
| list_commands, execute_command | `chat/menu.rs`, `chat/actions.rs` | ok |
| get_attachment (+ attachments on send) | `attachment_bytes` (`chat/actions.rs`); the staged files ride on send | ok |
| user model catalog / preference / policy | `model_catalog`, `set_model_preference`, `set_model_policy`, `set_model_picker` (`pages/providers.rs`, `chat/model_picker.rs`) | ok, except **get_user_model_preference**: `Client::model_preference` is called by neither app. PARITY.md's `done` was already wrong for GPUI. |
| notifications (list, mark read, mark all, archive) | `pages/notifications.rs`, `workspace.rs` | ok |
| notification channels, delivery targets, setup status | `pages/notifications.rs` | ok |
| automations (list, pause/resume/run/delete, rename) | `pages/automations.rs`, `chat/side/routines.rs` | ok |
| list_fs_mounts, browse_fs_dir, read_fs_file | `fs_mounts`, `fs_list`, `fs_preview` (`pages/files.rs`) | ok. **stat_fs_path** has no client call in either app; the listing carries the stat. |
| list/create/update/delete project | `pages/projects.rs` | ok |
| list_project_files, read_project_file, stat_project_file | `thread_files`, `thread_file_bytes` (`chat/actions.rs`, `chat/side/outputs.rs`). `thread_file_stat` is unused in both apps, matching PARITY.md ("the panel lists and opens files without a stat step"). | ok |
| extensions (list, registry, install, remove, setup, pairing, import, hosted MCP) | `pages/extensions.rs` | ok |
| operator extension configuration | `pages/operator/extensions.rs` | ok |
| skills (all eight) | `pages/skills.rs` | ok |
| settings tools, auto-approve, per-tool permission | `pages/settings.rs`, `settings/tools.rs`, `profiles.rs`, `chat/approval_mode.rs` | ok |
| llm config, provider key, active, models, delete, test, codex login | `pages/providers.rs` | ok |
| operator.get_setup | `pages/operator/setup.rs` | ok |
| **operator.run_setup** | `Client::run_operator_setup` is called by **neither** app (`client/src/operator.rs:209`). The Setup panel only reads the state. | **fails**, in GPUI as well |
| operator list/get/set config | `pages/operator/config.rs` | ok |
| operator status, diagnostics, service status/stop | `pages/operator/status.rs` | ok |
| logs (per conversation), operator.logs | `thread_logs` (`operator/inspector.rs`), `operator_logs` (`operator/logs.rs`) | ok |
| inspector snapshot/prompt/tool/updates | `pages/operator/inspector.rs` (`inspector_updates` at `:265`) | ok |
| TraceCommons (all four) | `pages/settings/traces.rs` | ok |

## 4. Prioritised fix list

Each item is sized for one focused fix.

### P0: broken or data loss

1. **The "system" scheme ignores the macOS appearance.** Keep `s.system_dark`
   current: set it from `cx.system_dark()` (or a Rune appearance callback) and
   re-run `s.palette = design::resolve(..)` when it changes. Then drop the local
   re-resolve in `workspace/shell.rs:25-29`. Right now the whole app except the
   shell root stays dark on a light Mac, and ⇧⌘Y (`workspace.rs:183`) reads
   the stale value. (`state.rs:95`, `design/mod.rs:136-147`.)

### P1: missing feature

1. **Thread drag-and-drop.** Make inbox rows drag sources
   (GPUI `workspace/inbox.rs:1446`). Make the chat transcript or composer a drop
   target that calls the existing, unused `chat::actions::attach_thread`
   (`chat/actions.rs:58`; GPUI `chat.rs:1567-1568`). Give the draft screen a
   drop target that calls `thread_attachment` (GPUI `workspace/draft.rs:343-389, 704-708`).
2. **Draft screen `@` menu.** Wire `chat::menu::ComposerMenu` into
   `workspace/draft.rs`, with files from the last project folder and no
   commands (GPUI `draft.rs:36-38, 97-118, 482`).
3. **Draft screen inline file cards and paste.** Use
   `inline_files::insert`/`pasted` in place of the chips (`draft.rs:475, 535`),
   and add `on_input_paste` (GPUI `draft.rs:405, 415`).
4. **Voice in the quick composer.** Add `voice::ui::composer_controls` to the
   quick card. Pass talk mode's claim through `quick::submit` and
   `draft::quick_send`, refusing when there is nothing to send. Make the quick
   composer the first voice target, and have `open_quick` open it for a key
   press from another app. This needs the field in `State` or a target that
   edits through `AppHandle::update`. (`workspace/quick.rs:159-205`,
   `voice/session.rs:301`; GPUI `quick.rs:58-66, 124`, `workspace.rs:689-722`.)
5. **Hide native previews under overlays.** Set `s.native.covered` while the
   command menu or a popover shows (`native_view.rs:106, 194`; GPUI
   `workspace.rs:1544-1545`).
   **FIXED in fbfd7aefc**: `workspace/shell.rs` calls
   `native_view::cover(s, !layer.is_empty())` as it assembles the overlay
   layer, so ⌘K and every menu, popover and dialog hide the views. Test:
   `workspace::shell::tests::overlays_cover_the_native_previews`. The native
   side still needs a live window (PLAN "Manual checks").
6. **A Rune `bundle.sh` (`ZLO Rune.app`)** with its own bundle id, the usage
   strings, and a LaunchAgent label different from `com.numoh.desktop`
   (`login_item.rs:9`). Point the copy in `pages/settings.rs:189` and
   `voice/apple.rs:97` at it. Without a bundle, system notifications, voice,
   open-at-login and the permission prompts cannot work in this app.
7. **Side chats' own extras:** model picker pill, goal, queue,
   fork-from-message, computer share, and Esc closing the side chat's picker
   (PLAN "Known gaps"; GPUI `chat/side/session.rs:148, 173-207`). This means
   making H's modules work by thread id.
8. **Translucent window.** Make the window transparent when
   `prefs.translucent` is on (GPUI `design/mod.rs` `set_translucent`/`apply_window`;
   Rune `pages/settings/appearance.rs:524` only saves it).
   **FIXED in fbfd7aefc**: Rune fixes transparency when it creates the window,
   so `main.rs` reads the pref at launch (`WindowOptions { transparent }`) and
   the shell root draws the system material under its fill at `shell_alpha`.
   Turning it on mid-session shows after a relaunch (the Appearance row then
   adds "Takes effect when ZLO next opens."); off applies at once. Scene
   `appearance-translucent`; needs a live-window check.

### P2: polish

1. Re-add the mouse-wheel glide that `design/smooth_scroll.rs` provided, as a
   Rune feature or an app-level wheel filter, and correct the claim at
   `design/motion.rs:23`.
   **FIXED in fbfd7aefc**: `design/smooth_scroll.rs` (`.wheel_glide(key)` on all
   19 vertical viewports) takes imprecise, unmodified notches in `on_wheel`
   and glides with `Tx::scroll_to_with` on a 200 ms spring, moving the target
   on mid-glide; trackpads stay 1:1. The `motion.rs` claim is corrected.
2. Put `.matched(COMPOSER_MATCH)` on the hero and docked composers, so the
   draft-to-chat glide comes back (`design/motion.rs:86-88`; GPUI
   `design/dock.rs`).
3. Put `.motion(ROW_RESORT)` on the keyed inbox rows (`design/motion.rs:79`;
   GPUI `design/flip.rs`).
   **FIXED in fbfd7aefc**: card and slim rows; the side dock's tabs also get
   GPUI's single gliding pill (`.motion(PILL)`), the other `flip::` user.
4. Call `run_clock::start` on every live fold, not only the first
   (`chat.rs:544`; GPUI `chat.rs:930-937`).
5. Record the window frame as it changes, not only in `on_close`
   (`workspace/keys.rs:49`; GPUI `main.rs:136`).
6. Use the accent for kit primary buttons (Approve, Save and so on), and fold
   `pages/c_widgets.rs`, `pages/usage/common_stub.rs` and `chat/extras_ui.rs`
   into `design::kit`/`pages::common` (the remaining PORT-STUBs).
7. Use the mono font in the memory and SKILL.md editors (needs rune-kit
   `TextArea` text styles; `pages/memory.rs:523`).
8. Refocus the composer on window focus only when nothing else has focus
   (`workspace.rs:644-660`; GPUI `restore_composer`, `workspace.rs:670`).
9. Guard the browser poll `tick` with the thread that started it
   (`chat/side/browser.rs:69-91`).
10. Restore three accessibility labels: "Filters" (`workspace/inbox/view.rs:186`),
    "Remove document" (memory archive) and "New thread on this profile"
    (profiles row).
11. Restore the draft copy "A new thread is still being created; try again."
    (`workspace/draft.rs:223`).
12. Add a "Quit ZLO" app-menu item (GPUI `main.rs:92`).
13. Place the quick composer a fifth of the way down the screen, not 180 px
    from the top (`workspace/quick.rs:192-195`).
14. Split `workspace/inbox.rs` and `workspace/inbox/view.rs`, which are over
    800 lines (PLAN rule 5).
15. PARITY.md, shared with GPUI: either call `run_operator_setup` from the
    Setup panel or mark `operator.run_setup` as read-only. Do the same for
    `get_user_model_preference` (`model_preference` is unused in both apps).
