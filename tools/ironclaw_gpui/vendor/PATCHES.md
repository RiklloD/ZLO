# Vendored GPUI crates

Copies of `gpui-pre 0.3.9` and `gpui-base 0.8.0` from crates.io (sources only,
example and bench targets removed), wired in through `[patch.crates-io]` in
`../Cargo.toml`. Every change is marked `Numoh patch:`.

Fixes the trackpad "stuck at the start of a scroll" feel:

1. `gpui-pre/src/elements/list.rs`, layout: tail following re-armed within
   1 px of the bottom, so a sub-pixel first packet got snapped back.
2. `gpui-pre/src/elements/list.rs`, `set_offset_from_scrollbar`: the same
   1 px snap applied to every ScrollBounce wheel packet, which then fed the
   rubber band instead of the list. Slack kept for real scrollbar drags.
3. `gpui-base/src/scroll_bounce.rs`, wheel path: removed the persistent axis
   lock, which zeroed vertical packets under 6 px after a sideways start.
4. `gpui-pre/src/elements/list.rs`, `max_scroll_offset`: include list
   padding, so a padded list's tail offset stays within its max (ScrollBounce
   dead zone at the bottom of the chat).

Fixes the "elastic, springs back on release" scroll:

5. `gpui-base/src/overscroll.rs`, `packet` `Ended` branch: only a stretched
   edge is thrown on release. Mid-list releases threw the whole view up to
   36 px, and the next swipe grabbed that phantom edge instead of scrolling.
6. `gpui-base/src/scroll_bounce.rs` wheel callback and
   `gpui-base/src/input/base/state.rs` `on_scroll_wheel`: an unclamped move
   reports all of `dy`. f32 residue from subtracting offsets read as an edge.
7. `gpui-pre/src/elements/list.rs`, `scroll_px_offset_for_scrollbar`: the
   tail-following "at the end" anchor reads as `-max`, not the full content
   height (up to a viewport past the max until the next layout).

On a gpui-kit bump: re-copy both crates at the new versions and re-apply.

Fixes the "scrolling up jumps higher or lower, scrollbar thumb resizes":

8. `gpui-pre/src/elements/list.rs`, `layout_items` leading overdraw: rows
   above the scroll top are measured every frame instead of trusted from
   cache. A row first measured there before its content settled kept the
   stale height until it became the scroll top, then jumped the view.
9. `gpui-base/src/text/state.rs`, `MAX_SYNC_FULL_REPLACE_BYTES`: 64 KiB, was
   4 KiB. Longer markdown parsed in the background and laid out near-empty
   on its first frame, which is what the list measured.
