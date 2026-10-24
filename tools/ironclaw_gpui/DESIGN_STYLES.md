# Design styles

The desktop app ships four complete looks. A style owns geometry, material,
type and motion (`app/src/design/styles.rs`); a palette preset owns color.
Picking a style loads its signature palette, and any palette can be chosen
afterwards.

Switch live: Settings → Appearance → Style, or `Cmd+Shift+Y` (View → Next
Design Style) to cycle all four.

| | Forge | Glass | Paper | Console |
|---|---|---|---|---|
| Mode | dark | dark, see-through | light | dark |
| Accent | ember `#FF8A3D` on graphite | sky `#8FB8FF` on indigo | ink blue `#2F5BEA` on paper | teal `#5EEAD4` on black |
| Material | machined plates: gradient, lit top edge, short drop | frosted white over the macOS blur | opaque fill, soft ambient shadow | flat fill, 1 px line |
| Window | opaque | `Blurred` (NSVisualEffectView) | opaque | opaque, no inset panel |
| Radii (panel/control) | 10 / 7 | 16 / 10 | 12 / 8 | 0 / 3 |
| Type | Geist 14 | Geist 14 | Geist 14 | Geist Mono 13 everywhere |
| Hover fade | 140 ms | 180 ms | 200 ms | 60 ms |
| Popovers | spring, damping 0.82 | spring, damping 0.62 (overshoots) | critical spring | 110 ms, no bounce |

## What moves

- Hover washes fade instead of snapping (every kit button, row, chip, card).
- The sidebar selection is one pill that glides between rows and fades out
  when a page without a nav row is open.
- Inbox cards and menu rows grow an accent tick when selected.
- Buttons sink 1 px while pressed; primary buttons spread an accent halo on hover.
- The composer is one floating dock; focus morphs its rim to the accent and
  grows a 3 px ring.
- Popovers (profile picker, filters, snooze, model menu, `/` menu) and open
  approvals arrive on the style's spring.
- "Working" is three breathing dots and a shimmering label, in the chat and
  on inbox cards.
- Page and thread switches fade and rise by the style's distance.

## Cost

The tween stores run on wall time and the window root asks for a frame only
while one is mid-flight (`motion::pump`), so an idle window draws nothing.
The battery saver's `reduce_motion` snaps every tween and freezes the
repeating loaders. Vibrancy is composited by macOS, not by the app.

## Research behind it

| App (license) | Taken |
|---|---|
| Zeron / comet (MIT) | wall-clock hover fades and state morphs, glass plate recipes, sliding indicator. Code adapted, see THIRD_PARTY_NOTICES.md |
| Sonora (GPL-3.0) | idea only: named spring presets per interaction kind. No code |
| Waku (GPL-3.0) | idea only: quiet sidebar, single composer dock. No code |
| Arbor, Loungy, Hummingbird, Runner (MIT/Apache) | read for layout density and window chrome. No code |

Not possible on gpui-pre 0.3.9: div scale transforms, backdrop blur inside
the window (only the window-level blur), edge-fade masks. Zeron gets those
from its own gpui fork.

## Not restyled yet

Pages still render their gpui-kit buttons, inputs, tables and switches.
They follow each style's palette and radii through the theme, without the
kit's motion and materials.
