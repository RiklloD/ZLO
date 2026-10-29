# Elastic trackpad scrolling in GPUI apps: post-mortem

Fixed 28.10.2026 in Numoh. Applies to any app on `gpui-kit 0.8.0`
(`gpui-pre 0.3.9`, `gpui-base 0.8.0`, `gpui-component 0.8.0`) that scrolls
through `ScrollBounce`: `MessageScroller`, `overflow_y_scrollbar()` areas,
lists, trees, tables, and text inputs. The fixes themselves are listed in
`PATCHES.md` (items 5 to 7, plus the chat pin in `app/src/chat.rs`).

## Symptom

On a macOS trackpad you swipe, the content moves a little, and when you lift
your finger it springs back to where it was, like a rubber band. It happens
mid-list, far from any edge, and worse near the bottom of a tail-following
chat. Mouse wheels with whole-line steps feel mostly fine. Nothing is logged.

## What did not work

These were tried first. Each one was a real bug, but none of them removed
the snap-back.

| Attempt | What it changed | Why it was not enough |
|---|---|---|
| Patch 1: exact-bottom tail re-arm in `list.rs` layout | Tail following re-armed only at the exact bottom, not within 1 px | Fixed a sub-pixel snap at the bottom. The elastic feel lives in the overscroll engine, which it never touches. |
| Patch 2: 1 px slack only for scrollbar drags in `set_offset_from_scrollbar` | Wheel packets stopped being eaten by the slack | Same: a bottom-edge fix. Mid-list snap-back stayed. |
| Patch 3: removed the persistent axis lock in `scroll_bounce.rs` | Small vertical packets after a slightly sideways start were no longer zeroed | Fixed "stuck at the start of a scroll". Unrelated to release behavior. |
| Patch 4: `max_scroll_offset` includes list padding | A padded list at its tail stopped reporting an offset 16 px past its max | Removed a 16 pt jump plus dead zone at the bottom. Its test passed, but it only checks state after a layout pass. The real bug at the tail happens between frames (cause 3 below). |
| Suspected two scroll owners fighting (list's own wheel handler vs ScrollBounce) | Nothing, ruled out by reading `window.rs` dispatch | ScrollBounce handles the Capture phase and calls `stop_propagation`, so the list's Bubble handler never runs. One owner only. |
| Suspected `MessageScrollerState` recreated every render | Nothing, ruled out | It is created once. Row changes go through a guarded diff. |
| Running the vendored crates' tests as published | `cargo test` on the crates.io sources | Does not compile outside the Zed repo: tests `include_bytes!` fonts, images and a `README.md` that are not in the package, and use `util_macros`. Workaround below. |
| Waiting for one model's diagnosis | One report at a time | Each of 4 models (Opus, GPT, GLM, Gemini) found a different piece. Only the union fixed it. |

Lesson: we kept fixing the bottom edge because that is where the first
recording showed a hitch. The real causes sat in the shared rubber-band engine
and showed up everywhere.

## What did work: the 4 real causes

### 1. A release mid-list throws the band (the main one)

`gpui-base/src/overscroll.rs`, `Overscroll::packet`, `Ended` branch. On
`TouchPhase::Ended` it called `physics.let_go(velocity * CARRY)` even when
nothing was stretched. In `Physics::let_go` (`scroll_bounce.rs`), position 0
times any velocity is `>= 0`, so the finger speed became an outward throw.
The whole view shot up to 36 px ahead and sprang back over about 0.7 s.

The second half is what makes it feel broken: `Physics::begin` grabs a
returning edge where it is ("a returning one included"). So the next swipe,
started while the fake throw was settling, was treated as pulling an edge.
Every packet stretched the band, the list stayed still, and release snapped it
back. Repro: a 100 px swipe moved the list 0 px.

Fix: only throw when `self.offset() != 0.`, otherwise `physics.release()`.

### 2. f32 rounding read as hitting an edge

`gpui-base/src/scroll_bounce.rs` wheel callback, and the same pattern in
`input/base/state.rs` `on_scroll_wheel`. The callback reported how far the
list moved as `after - before` on two absolute f32 offsets. A 0.3 px move came
back as 0.2999878. The engine treats any `unused != 0.` as an edge hit, so the
0.0000122 px leftover started a stretch. From then on `offset() != 0.`, every
packet went to `pull()`, and `pull()` only hands movement back to the list
when the stretch crosses zero, which a one-directional swipe never does.

Fix: if the move was not clamped (`offset.y == target`), return the full `dy`.
A fixed tolerance like `abs() > 0.01` also works but can fail far down long
lists, where f32 spacing grows.

### 3. Tail anchor reads a screen past the max between frames

`gpui-pre/src/elements/list.rs`, `scroll_px_offset_for_scrollbar`. When a
wheel packet reaches the end of a Top-aligned, tail-following list,
`set_offset_from_scrollbar` pins `logical_scroll_top = (item_count, 0)`. The
reader turned that into the full content height, ignoring viewport and
padding: -1200 px against a real max of -1016 px. Layout heals it on the next
draw, but trackpad packets (~125 Hz) arrive faster than frames, so packets in
between saw ~184 px of fake overscroll and saturated the band. Upstream Zed
does not hit this because its lists are Bottom-aligned, which already return
`-max` for the pinned case.

Fix: if `item_ix >= item count`, return `-max_scroll_offset()`.

### 4. App-level pin fighting the user (Numoh-specific)

`app/src/chat.rs`, `reserve_height`. After a send, the "keep the prompt at the
top" reserve pinned the scroll on every frame, about 11 px above the real
list end. Scrolling to the end re-armed tail following, and the pin pulled the
view back on every packet. Fix: the reserve gets a `scrolled` flag, set by a
capture-phase scroll-wheel listener over the transcript, and stops pinning
after the first user scroll. If your app has any "pin while streaming" logic,
check it the same way.

## How to test the vendored crates

The published sources need stubs to compile their tests. Do this in a copy:

```sh
cp -R vendor/gpui-pre /tmp/gpui-pre-test
printf '\n[workspace]\n' >> /tmp/gpui-pre-test/Cargo.toml
# disable test modules that need missing assets: change #[cfg(test)] to
# #[cfg(any())] on the image test module in platform.rs and the test modules
# in randomized_element_tree.rs, svg_renderer.rs and style.rs
cd /tmp/gpui-pre-test && cargo test --features test-support --lib list::

cp -R vendor/gpui-base /tmp/gpui-base-test
printf '\n[workspace]\n[patch.crates-io]\ngpui-pre = { path = "/tmp/gpui-pre-test" }\n' \
  >> /tmp/gpui-base-test/Cargo.toml
# replace include_str!("../../../../README.md") in src/text/text_view.rs with "# stub"
cd /tmp/gpui-base-test && cargo test --features test-support --lib
```

Point gpui-base at the patched gpui-pre copy, or you test the unpatched list.
Results after the fix: list 31/31, gpui-base 402/402.

## If it comes back: trace recipe

Unit tests can only simulate packets. To see the real gesture, log one line
per packet at the top of `Overscroll::packet` and in the ScrollBounce wheel
callback, then reproduce once:

```
phase, dy, before_y, max_y, target_y, clamped_y, returned_used,
unused, band_offset_before/after, dragging, velocity, spent
```

How to read the log:

- `unused` tiny but nonzero on a mid-list packet, then packets that never move
  the list: cause 2.
- `Ended` with `band_offset == 0` followed by a nonzero band offset: cause 1.
- `before_y` past `-max` right after a packet that reached the end: cause 3.
- List offset jumping back with no matching finger movement: app-level pin
  (cause 4) or rows measured mid-scroll.

## Fixed 29.10.2026: jumps while scrolling up

The "still open" lazy-measure case was real. Replies over 4 KiB parse their
markdown in the background, so a reply view rebuilt as it re-entered the
list's 400 px leading overdraw laid out near-empty. The list cached that
height and never re-measured rows above the scroll top. Scrolling up into the
row made it the scroll top, it rendered at full height, and the view jumped by
the difference (scrollbar thumb resizing at the same moment). Patches 8 and 9.

Root of the re-parse: `TextView::markdown` keeps its state with
`use_keyed_state`, which lives only while the element renders on consecutive
frames, so every re-entry parsed again. The app now owns one state per
transcript row (`ui::kept_markdown`, cap 1024), so a reply is parsed once
and keeps its height at any size. Patch 9 still covers first sight of a row.

## Last resort

Make `ScrollBounce::enabled_by_default()` return `false` in
`gpui-base/src/scroll_bounce.rs`. You lose the macOS edge bounce, and the
whole class of bugs goes away, as in upstream Zed.
