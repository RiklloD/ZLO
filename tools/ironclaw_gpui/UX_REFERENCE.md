# UX reference: Zeron, T3 Code, Hermes (24.10.2026)

What the reference apps do, measured, so IronClaw Desktop can copy the flow
instead of guessing. Sizes are in points (1x). Screenshots of Lorik's
3024x1964 captures were divided by 2.

## What the apps are built with

| App | Stack | How we know |
|---|---|---|
| Zeron 0.2.121 | Native Rust on GPUI (a fork, `zui`) + `zeronsh/gpui-component` (gpui-base 0.5.2), Geist font, `gpui-thinking-orbs` | 155 MB Mach-O binary; panic paths `crates/gpui/src/...`, `crates/ui/src/{motion,transcript,composer_dock,settings/*,roll_text,edge_fade}.rs`; THIRD_PARTY_NOTICES |
| T3 Code (Nightly) | Electron + React + Tailwind v4 | `app.asar`, `apps/server/dist/client/assets/main-*.css` |
| Hermes desktop | Electron/web | screenshot only |

Zeron uses the same toolkit family as us, so its behaviour is reachable in
GPUI. Our `design/motion.rs` already ports its curves. No Zeron source is in
the bundle, only compiled code, so the timings below come from frames
captured during a live turn, not from code.

## Zeron: one full turn, in order

Window 1280x853. Content column **714 wide** (x 407 to 1121), centered.

1. **Empty state.** No heading, no suggestions. Only the composer, centered
   vertically: 714x100, radius ~18, two rows (text on top; attach left,
   model + effort + send on the right). After the first keystroke a row of
   small pickers ("No project", device) appears 18 px above the composer, right-aligned.
2. **Send (Enter).** In one motion, about 300 ms:
   - the composer glides from the center to the bottom dock and collapses
     to a single-row pill, 714x46, radius 23 (fully round);
   - the conversation fades in with an ~8 px rise (frame caught at ~40 %
     opacity, user text 8 px below its rest position);
   - the user message lands **top-right** as a pill: height 40, radius 20,
     padding 15 h, fill one step lighter than the page, no border;
   - the sidebar gets the new row at the top ("New session", "now"); the
     title updates later to a generated name.
3. **Pending.** 56 px under the bubble, left-aligned with the text column:
   a 10 px animated glyph, the label "Conjuring…" (12 px, muted) and an
   elapsed counter "0s", "1s"… whose digits roll (`roll_text.rs`). The
   label changes while waiting ("Conjuring…", "Zeroning…").
4. **Reasoning.** Arrives as a disclosure "› Thought process" (12 px, muted,
   chevron 10 px). While the model thinks, it is **open**: indented 28 px,
   a tree connector (1 px line, rounded elbow) to a sub-row with a bubble
   icon, then the reasoning text: bold 12 px heading from the summary, body
   12 px, line height 18, muted, max width ~510. The pending row stays
   pinned **under** the newest content and moves down as content grows.
5. **First answer token.** The reasoning block auto-collapses to the single
   "› Thought process" line (chevron caught mid-rotation, ~200 ms). Answer
   text appears in place, unboxed, 14 px, line height 21, words fading in
   from ~30 % opacity. No relayout jumps: the column above never moves.
6. **Tools.** Between paragraphs, each burst of tool work is one collapsed
   line: "› Ran 1 command", "› Ran 1 command · called 6 tools". Narration
   ("I'll check…") is plain answer text, not a tool row.
7. **Scrolling.** On send, the new user message is pinned near the top of
   the viewport with empty space reserved below; text streams down into
   that space, so nothing scrolls during the turn. When the turn ends the
   reserve goes away and the conversation settles to the bottom, just
   above the composer.
8. **Finished.** The pending row disappears, Stop turns back into Send. A
   meta row (timestamp "Oct 24, 2:47 PM" + copy icon, 11 px, faint) sits
   under each message and **only shows on hover**.

Spacing in the transcript: user bubble to next block 56 px; disclosure line
to text 20 px; paragraph gap 14 px; between turns ~60 px.

## Zeron settings

- Clicking the gear swaps the **sidebar** into a settings list: "← Back",
  then sections in gap-separated groups (16 px gap between groups):
  General, Appearance, Notifications, Voice, Shortcuts | Providers,
  Devices | Files, Appshots, Archived sessions. Row pitch 34 px, icon 14 px,
  label 13 px, selected row = filled pill radius 8.
- The page: a **660 wide** column, title 18 px semibold at the top, then
  cards. Group headers ("Links", "Git") are 12 px muted, 16 px above their
  card. Cards: radius 10, fill one step darker than the page, no border.
  Rows inside: 58 px tall, padding 16 h, hairline dividers inset 16 px.
  Each row: label 13 px medium + description 12 px muted on the left, the
  control (switch, select, button) right-aligned and vertically centered.
- One topic per page. Nothing longer than ~2 screens; long areas get their
  own sidebar entry rather than a longer page.

## T3 Code (base proportions to match)

From its CSS: base `text-sm` 14 px, `text-xs` 12 px, `text-2xs` 11 px,
spacing unit 4 px, radius 10 px (sm 6, md 8, xl 14). Default transition
150 ms `cubic-bezier(.4,0,.2,1)`; entrances 500 ms `cubic-bezier(.22,1,.36,1)`
(our EASE_OUT_QUINT); live tool shimmer 2.2 s; status pulse 2 s.

Measured from the screenshot (points):

| Element | T3 Code | Hermes | IronClaw today |
|---|---|---|---|
| Sidebar width | 310 | 260 | 265 |
| Search | 15 px text, 18 px icon, no box | 14 px text, 18 px icon | 13 px text in a 21 px boxed field |
| Thread row pitch (2-3 lines) | 88 | 90 | 49 |
| Thread title | 15 px | 14 px | 13 px |
| Thread meta line | 13 px | 13 px | 11 px |
| Footer icons (gear etc.) | 15 px on a 38 px pitch, 28 px hit target | 14 px | 11 px on a 25 px pitch |
| Composer | 782 x 153 (empty state), radius 18 | 1170 x 210, radius 12 | 780 x 87, radius 14 |
| Composer text | 15 px | 15 px | 13 px |
| Model pill text | 15 px, icon 16 | 13 px | 13 px |
| Send button | 30 round | 30 round | 22 round |
| Breadcrumb | 15 px | 13 px caps tabs | 13 px |

## What we copy, and the GPUI mapping

| Behaviour | Our implementation |
|---|---|
| Composer glides center to bottom on first send | Chat composer slides in from the draft's center offset on mount (`motion::slide_in`), 420 ms EASE_OUT_EXPO (style `enter`) |
| Conversation fades in, 8 px rise | `motion::fade_in` with style `enter` |
| User pill top-right, pinned on send, reserve below | `scroll_to_item(user)` + a trailing spacer row sized to the viewport; it glides to 0 when the run ends |
| Pending row under the newest content, elapsed seconds | trailing list item; pulse dots + shimmer label + `Ns` |
| Reasoning open while live, auto-collapse on first answer token | activity rows grouped into one disclosure item; open while it is the live tail, collapsed after; manual toggle wins |
| Tool bursts as one line "Ran N tools" | same grouping; summary label counts tools, skills |
| Streamed words fade in | gpui-component `stream_fade` (280 ms, 10 ms stagger), already on |
| Copy on hover | group-hover meta row under assistant text |
| Settings: sidebar list in groups, cards with headers and descriptions | `settings_pages` gains group breaks; `kit::section`/`kit::setting_row`; pages split |
| Base size | style tokens: font 14 to 15 base, row height 30 to 34, icons 14 to 16 |

Battery: every animation above is either a `with_animation` (plays once,
honors reduce_motion) or a `motion::state_t`/`glide` tween that stops
requesting frames when settled. The pending row is the only looping
animation and exists only while a run is live.

## Zeron source numbers (round 2, read from zeronsh/zeron 4231cb7)

Zeron is open source (MIT). These come from its code, not from captures.
File paths are under `crates/ui/src/`.

### Transcript (`transcript.rs`)

| Thing | Value |
|---|---|
| Gap above a turn (user message) | 16 (`SPACE_LG`) |
| Gap around a tool group | 12 (`SPACE_MD`) |
| Gap between other rows | 8 (`SPACE_SM`); markdown blocks 12 |
| Body text | 14 px on 22 px (`MD_TEXT_SIZE`, `MD_LINE_HEIGHT`); code 12.5/18 |
| User bubble | wash 8 % white (dark) / 4 % black (light), radius 16, padding 16×10, max 80 % of column, pending at 65 % opacity |
| Group header | 26 tall, 12 px muted, hover to text, chevron 14 in a 22 px slot, rotates -90° closed |
| Group summary | "Thought process · ran 2 commands · edited 1 file · called 2 tools · 1 failed", first letter capitalized |
| Tree row | 32 tall; gutter 48; trunk x 12.5; bend radius 6; branch to x 28; icon 16 at x 32; text 8 after gutter |
| Tree line color | hairline 12 % |
| Fold | 140 ms `ease-out` (0, 0, 0.58, 1), height tween with clip |
| Row reveal | 360 ms ease-out-expo; first row 90 ms late, 65 ms stagger; trunk draws 0–62 %, elbow 58–100 %, icon fades with the elbow, text rises 4 px |
| Live title shimmer | 3.4 s sweep, highlight half-width 36 % of the title |
| Working line | 16 above; gradient glyph (3×3, 2.5 px cells, rows #B6D3EF #EDB185 #F888A0, 750 ms wave, rest 10 %); verb 12 px muted + "…"; seconds 11 px faint, 1 px lower |
| Verbs | 21 words, rotate every 7 s, seeded per chat (FNV-1a of chat id) |
| Rolling text | 550 ms ease-out-quint, 0.35 em travel, 30 % stagger per glyph, slight blur and tilt |
| Elapsed format | "12s", "3m 4s", "1h 2m" |

### Composer (`composer.rs`)

| Thing | Value |
|---|---|
| Docked (in a thread) | one line, 49 tall, radius 22 |
| Open | radius 26, text area min 76 (20 vertical padding), action row 32 + 8 bottom |
| Max width | 768 |
| Input | 14 px on 22.75 |
| Attach / send | 28 px circles; send = text color fill, 14 px arrow in background color; stop = 11 px rounded square |
| Fill | input glass (dark: a little darker than the panel, measured #2a2a2a on #303030) |

### Settings (`settings/widgets.rs`, `settings/appearance.rs`)

| Thing | Value |
|---|---|
| Page column | max 760, padding 40 |
| Page title | 20 px medium |
| Section label | 13/17 muted, inset 8, 32 above |
| Block | radius 12, fill = ink 4.5 %, no border |
| Row | min 60, padding 16×12, gap 16, divider = border at 60 % |
| Row title / description | 13 medium / 12 muted |
| Option card | preview 148 tall, radius 6, border mixes to accent when selected; caption 13 px with 16 px icon, accent when selected |
| Select | 32 tall |
| Switch | 44.8 × 28.8, track 20.8, 180 ms cubic ease-out |
| Appearance order | Color scheme cards → light theme, dark theme, accent → Material and background → Motion → Theme library → Fonts and layout |
| Accents | Violet #8b7cf6/#5b43e8, Orange #fb923c/#c2410c, Amber #fbbf24/#a16207, Green #4ade80/#15803d, Cyan #22d3ee/#0e7490, Blue #60a5fa/#2563eb, Pink #f472b6/#be185d (dark/light) |

### Sidebar (`shell.rs`)

| Thing | Value |
|---|---|
| Session row | 45 tall (29 compact), radius 8, padding 8×6, gap 2 |
| Selected | wash 11 % (dark) / 6 % (light), no ring or tick |
| Hover | one step under, 150 ms fade; rest text at 80 % |
| Harness icon | 13 px at 50 % muted |
| Footer | 28 px buttons, 4 apart; avatar 16 |
| Titlebar | 38 tall, content 4 px low; controls 24 wide, 2 apart, groups 8 apart |
