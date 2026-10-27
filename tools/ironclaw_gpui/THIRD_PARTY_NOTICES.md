# Third-party notices

## Zeron

Parts of `app/src/design/` (the oklch color math, the dark palette values, the
cubic-bezier solver, the motion catalog, the hover-fade and state-morph
tween stores and the glass plate recipes in `kit.rs`), the transcript layout
and motion in `app/src/chat/flow.rs` (tool fold, task tree, working line,
rotating verbs, gradient glyph), the composer pill, the Appearance page and
settings block metrics, the session row, and the light theme and accent
presets in `design/themes.rs` are adapted from Zeron,
https://github.com/zeronsh/zeron (read at commit 4231cb7). The sidebar
glyph, panel reveal timings and titlebar arrangement in `design/panels.rs`,
and the dither, halftone, ASCII and scanline effects and wallpaper crossfade
in `design/paint.rs` and `design/wallpaper.rs`, follow its `icons.rs`,
`motion.rs`, `shell/panel_layout.rs` and `new_thread_background_effects.rs`
(read at commit 3d2f1e6). From the same commit, `design/smooth_scroll.rs` is
adapted from its `smooth_scroll.rs`, the two-way composer hand-off in
`design/dock.rs` follows `composer_dock.rs`, and the tool-row cascade timings
in `chat/flow.rs` follow `transcript.rs`.

MIT License. Copyright (c) 2026 Wing.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.

## MonoCode

Parts of the motion code (`design/pulse.rs`, `chat/pace.rs` and the other
files that credit it) are adapted from MonoCode
(https://github.com/hardbeat920/monocode), under the MIT license. The right
pane (`chat/side.rs` and `chat/side/`) follows the layout, git calls and
graph lanes of MonoCode Native (branch `mc/migrate-monocode-to-gpui`,
`right_panel.rs`, `changes.rs`, `diff_view.rs`, `history.rs`,
`session_review.rs`, `monocode_core::git`):

```
MIT License

Copyright (c) 2026 Nick

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Geist and Geist Mono

`app/assets/fonts/` bundles Geist and Geist Mono, copyright 2024 The Geist
Project Authors (https://github.com/vercel/geist-font), under the SIL Open
Font License 1.1. The full license is in `app/assets/fonts/Geist-OFL.txt`.

## Provider icons

`app/assets/providers/` holds provider logos.

- `openai.svg`, `claude.svg`, `cursor.svg`, `grok.svg`, `opencode.svg`,
  `pi.svg`, `muse.svg`, `antigravity.png`: from T3 Code
  (github.com/pingdotgg/t3code), MIT License, Copyright (c) 2026 T3 Tools Inc.
- Every other file: from lobe-icons (github.com/lobehub/lobe-icons,
  `@lobehub/icons-static-svg` 1.98.0), MIT License, Copyright (c) 2023 LobeHub.

The logos are trademarks of their owners and identify the providers only.
