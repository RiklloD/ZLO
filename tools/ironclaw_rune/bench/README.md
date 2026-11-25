# ZLO frame bench

A repeatable frame-time benchmark for the Rune app. Each interaction runs
through Rune's headless driver: no window, no gateway, the same frames every
run.

```sh
cd tools/ironclaw_rune
bench/run.py run                       # build, run all 8 interactions 5×, write bench/results/latest.json
bench/run.py run tools typing --runs 3 # some interactions only
bench/run.py run --no-build --no-pixels  # reuse the binary, skip the wasted-frame pass
bench/run.py compare bench/baseline-2026-11-20.json bench/results/latest.json
bench/run.py show bench/baseline-2026-11-20.json
```

`run` builds `target/release/zlo` with Rune's phase clocks turned on
(`cargo build -p ironclaw_rune --release --features rune/frame-profile`). That
enables a Rune feature from the command line, so the app's manifest gains no
feature. A full run takes about 5 minutes on an M5. The thin-LTO release
build takes about 5 minutes more.

## How it works

| piece | where |
|---|---|
| scenes `bench-*`: the window in a fixed state, with default prefs | `app/src/bench.rs`, listed in `app/src/scenes.rs` |
| what the gateway would do (reply bursts, a switched thread's timeline, runs starting and ending), on the frame clock | `bench::poll`, called from the app's `poll` (no-op outside bench scenes) |
| what the person does (taps, ⌘B, ⌥⌘B, typing, wheel notches, trackpad swipes) | `bench/scripts/NAME.json`, Rune's `--script` format |
| per-frame CPU and phase split | Rune `frame-profile`: `RUNE_FRAME_LOG` JSON lines |
| why each frame kept animating | Rune motion trace (`--trace`), `quality.why` = AnimSource bits (`runtime/paint/cycle.rs`, `runtime/types.rs`) |
| wasted frames | one extra run with `--out` that dumps every frame's pixels, which are then hashed |

Every run is `--realtime` at 120 Hz, 1280×820 @2x. The frame clock keeps
pace with the wall clock, so the app's own `Instant`-based timers see real
time: the stream pacer, the draft-save debounce and the run clock. The
switch scene's timeline lands one frame after the thread opens, so the
gateway's latency is left out and only the UI's cost is measured.

| interaction | scene | input |
|---|---|---|
| `stream` | 40-turn chat; a ~2.6k-char reply (Markdown, table, 30-line code block) arrives in 6–48-char bursts every 45–75 ms, then the run ends | none |
| `switch` | two 150-turn threads (code blocks, tables) | 4 sidebar taps alternating t2/t1, 1 s apart |
| `sidebar` | 40-turn chat | ⌘B ×4 |
| `scroll` | 500 messages | 20 wheel notches (40 pt, not precise, `notch` set, as Rune marks a real mouse wheel on every platform), then 3 trackpad swipes with fling |
| `dock` | 40-turn chat, right pane closed | ⌥⌘B ×3 |
| `typing` | 40-turn chat | 200 chars into the composer, 30 ms apart |
| `shelf` | 40-turn chat | none: 3 runs start (cards move to the Working shelf), then end |
| `tools` | a finished turn with 1000 tool calls, group collapsed | tap the group open, then 3 trackpad swipes through it |

## Columns

- **drawn**: frames a real window would draw. Frame *k* counts when input
  arrived, an update committed, or frame *k−1* was still animating. The
  mount frame (k = 0) is left out; `mount_ms` in the JSON has it.
- **p50 / p99 / max ms**: thread CPU per drawn frame (update + encode/submit).
  Each cell is the median of 5 runs, with the min–max over those runs in
  parentheses.
- **>8.3ms**: drawn frames over the 120 Hz budget (median of runs).
- **layout p50/p99**: CPU in Rune's layout phase (it includes text shaping).
- **paint p50/p99**: CPU in Rune's paint phase.
- **submit p50**: wall time from the end of the update to submit (GPU
  encoding).
- The JSON also has `view_cpu_*` (the app's `view`), `commit_cpu_*` (diffing
  and mounting, which includes layout), `text_cpu_*` and `cpu_mean`.
- **wasted**: drawn frames whose pixels are identical to the previous
  frame's, out of drawn frames, from the pixel pass. Real time drifts a
  little while the pixel pass dumps PNGs, so its drawn count can differ
  slightly from the timing runs' (`typing`: 416 vs 455).
- **cycle**: frames whose pixels changed although nothing above asked for a
  frame. These are paint-only display-clock cycles: Rune's loop step for
  spinners and word fades. A real window draws them through a cheap paint
  step, not a full frame, and their CPU is **not** in the p50/p99 columns.
- **why**: for each drawn frame, the AnimSource reasons (`tracks`, `growth`,
  `scroll`, `virtual-fill`, …) that kept the previous frame animating, plus
  `input` or `update` (a commit with no input). `wasted:` gives the same
  breakdown over wasted frames only. The JSON also has Rune's commit reasons
  (`commit_why`: `input`, `dirty`, `instant`, `poll`, `layout`, `route`).

`compare` prints base vs new medians and their delta. It stars (`*`) a delta
that falls outside the base's min–max spread.

## Noise

Five runs on an idle M5 (2026-11-20). p50 spread is within ±0.05 ms except
`sidebar` (0.37–1.18 ms; the two clusters are runs where the slide's tail
frames do or don't land on the same frames as the commit). p99 spread is
within ±0.15 ms for every interaction except `sidebar` (1.5–3.6 ms) and
`tools` (8.6–20.9 ms). The `tools` maxima vary most (22–42 ms), since they
are single frames that shape 1000 rows. Frame counts and wasted counts are
deterministic to ±1 frame. A background build or another bench on the same
machine moves p50 by 2–3×, so compare only runs from an otherwise idle
machine.

## Baseline (main @ 69eefdb73, 2026-11-20)

`baseline-2026-11-20.json`. `bench/run.py show` prints it:

```
interaction | drawn | p50 ms           | p99 ms            | max ms              | >8.3ms | layout p50/p99 | paint p50/p99 | submit p50 | wasted  | cycle
------------+-------+------------------+-------------------+---------------------+--------+----------------+---------------+------------+---------+------
stream      | 168   | 1.09 (1.07–1.15) | 1.57 (1.50–1.60)  | 1.61 (1.58–2.09)    | 0      | 0.12/0.24      | 0.25/0.39     | 0.05       | 0/170   | 464
switch      | 481   | 0.35 (0.34–0.36) | 1.30 (1.26–1.39)  | 2.86 (2.78–2.95)    | 0      | 0.00/0.11      | 0.23/0.30     | 0.04       | 92/481  | 0
sidebar     | 277   | 1.13 (0.37–1.18) | 3.26 (1.48–3.59)  | 3.98 (2.07–4.60)    | 0      | 0.00/0.07      | 0.66/0.97     | 0.12       | 52/277  | 0
scroll      | 570   | 0.31 (0.29–0.38) | 1.23 (1.13–1.38)  | 1.47 (1.35–1.60)    | 0      | 0.00/0.19      | 0.16/0.30     | 0.04       | 11/570  | 0
dock        | 245   | 0.29 (0.28–0.40) | 1.65 (1.64–2.32)  | 2.74 (2.40–3.44)    | 0      | 0.00/0.19      | 0.17/0.66     | 0.04       | 39/245  | 0
typing      | 455   | 0.75 (0.73–0.81) | 1.32 (1.27–1.41)  | 1.59 (1.46–2.47)    | 0      | 0.00/0.10      | 0.20/0.27     | 0.03       | 154/416 | 0
shelf       | 9     | 1.01 (0.98–1.02) | 1.12 (1.10–1.18)  | 1.12 (1.10–1.18)    | 0      | 0.06/0.10      | 0.24/0.28     | 0.05       | 1/9     | 0
tools       | 410   | 1.78 (1.60–2.56) | 9.18 (8.58–20.94) | 24.60 (22.12–42.31) | 13     | 0.00/0.06      | 1.29/1.85     | 0.07       | 91/410  | 0
```

## Hotspots seen in the baseline

Each was traced from the frame log and the motion trace's `tk` (the tracks
each frame touched) to app code:

1. **1000 tool calls are neither virtualized nor memoized.** Opening the
   group shapes all 1003 rows in one frame: 34 ms CPU, of which 21 ms is
   layout and 14 ms text. After that, each trackpad event re-runs `view` and
   commits the whole 1000-leaf tree: 10–24 ms frames, 13 frames over budget.
   The leaf loop is `app/src/chat/flow.rs:623` (`render_activity`). In
   `app/src/chat/view/transcript.rs:208`, `Item::Activity` is rendered with
   no `memo`, unlike finished replies (`transcript.rs:167`). Fix: memoize
   finished groups, and use a nested virtual list for the tree (#956).
2. **Every wheel event rebuilds the whole view.** The `smooth_scroll`
   wrapper's `on_wheel(&mut S, …)` handler (`app/src/design/smooth_scroll.rs:93`,
   attached at `chat/view/transcript.rs:372`) counts as a state change. Each
   notch and trackpad delta therefore commits with `why = input`, and in
   `tools` 57 of those frames change nothing on screen. Dropping it for
   Rune's native glide (#1150, workstream A2) removes the rebuilds.
3. **Draft-save debounce fires a full update 300 ms after each keystroke.**
   `jobs::after` always uses `handle.update` (`app/src/jobs.rs:131`), so the
   draft save that `drafts::record` schedules (`app/src/drafts.rs:357`) re-runs
   view and commit with nothing visible: `dirty`/`instant` commits in
   145 of 154 wasted typing frames. Fix: `update_if` that returns whether
   the view changed (false for a save). The keystroke frame itself is
   ~4 ms max: 1 ms view plus 2.9 ms commit (`route`), from the whole root
   view re-running per key.
4. **A thread switch flies the composer onto itself.** Every docked chat
   composer carries `matched(COMPOSER_MATCH)` (`app/src/chat/composer.rs:297`),
   so switching t1→t2 runs a matched flight between two composers at the
   same spot. Its `travel` track keeps about 23 frames per switch running
   with no pixel change (92 of 481 frames wasted). Fix: match only across
   the draft→chat hand-off (key the match by thread, or set it only on the
   send's commit).
5. **Spring and presence tails after a slide.** The sidebar's `main-column`
   `frame` track (rune-kit split, driven by `panels::REVEAL`,
   `app/src/design/panels.rs:29`, set at `app/src/workspace/shell/frame.rs:81`)
   keeps ~13 sub-pixel frames per toggle: 52 of 277 frames wasted. The right
   pane's children (`app/src/chat/side/dock.rs:156–173`: title, body, border,
   edge) keep `presence` tracks running inside a clip that is already shut
   or still held by `CLOSE_HOLD` (`dock.rs:41`): 37 of 245 frames wasted.
   Fix: settle the springs at a half-pixel rest threshold, and use
   `Transition::none()` on the pane's children.

Also note: while a reply streams, 464 of the 900 frames are paint-only
cycles (the Thinking/working spinner and word fades: the `cycle` column).
Each one is cheap, but they double the number of frames the window draws
during `stream` (B1's territory). Every frame in the other scenes stays well
under 2 ms p99.
