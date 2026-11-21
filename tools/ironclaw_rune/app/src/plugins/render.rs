//! Drawing a plugin's node tree with the app's own pieces.
//!
//! Every node maps onto an existing component at the type sizes, spacing
//! and theme colours of the place it is mounted ([`Density`]): the page kit
//! in Settings, the side pane's denser kit in a chat's tab. A plugin picks
//! only a text `style`, a button `style` and a `tone`; everything else
//! (fonts, colours, radii, padding) is the app's. An unknown node draws as
//! nothing.

use rune::{Color, Cursor, El, TextStyle as Font, col, el, icon, row, text};
use rune_kit::{Kit, Markdown, MarkdownStyle};
use serde_json::Value;
use zlo_client::plugin_ui::{ButtonStyle, ListItem, Node, TextStyle, Tone};

use super::MountKey;
use crate::design::{self, Palette, motion};
use crate::icons;
use crate::pages::common::controls::{Look, Size, button_sized};
use crate::pages::common::{block_fill, ts};
use crate::state::State;

/// How far a mount's actionable rows dim while its plugin answers a press.
const BUSY_DIM: f32 = 0.55;

/// Where a tree is drawn, which sets its type scale and spacing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Density {
    /// A Settings page: 14 pt body, the page kit's buttons and cards.
    Page,
    /// The chat's right pane: 13 pt body, its small buttons and rows.
    Panel,
}

impl Density {
    fn font(self, style: TextStyle) -> Font {
        use crate::design::styles::rem;
        match (self, style) {
            (_, TextStyle::Mono) => ts::MONO_XS,
            (_, TextStyle::Caption) => ts::xs(),
            (Density::Page, TextStyle::Title) => ts::body_semibold(),
            (Density::Page, TextStyle::Body) => ts::sm(),
            (Density::Panel, TextStyle::Title) => Font::new(rem(13.), 600.),
            (Density::Panel, TextStyle::Body) => Font::new(rem(13.), 400.),
        }
    }

    /// The body font with its line height, for paragraphs (markdown).
    fn body_with_leading(self) -> Font {
        use crate::design::styles::rem;
        match self {
            Density::Page => ts::sm().lh(rem(22.)),
            Density::Panel => self.font(TextStyle::Body).lh(rem(20.)),
        }
    }

    fn gap(self) -> f32 {
        match self {
            Density::Page => 8.,
            Density::Panel => 6.,
        }
    }
}

/// The ink of a tone; `None` keeps `fallback`.
pub fn ink(p: &Palette, tone: Option<Tone>, fallback: Color) -> Color {
    match tone {
        None => fallback,
        Some(Tone::Neutral) => p.text_muted,
        Some(Tone::Success) => p.success,
        Some(Tone::Warning) => p.warning,
        Some(Tone::Danger) => p.danger,
        Some(Tone::Accent) => p.accent,
    }
}

/// The app's status tag (`pages::common::tag`) in a plugin's tone, at
/// `size` pt (12 on pages and panes, the sidebar's meta size on rows).
pub fn chip(p: &Palette, label: &str, tone: Tone, size: f32) -> El {
    let ink = ink(p, Some(tone), p.text_muted);
    let wash = if tone == Tone::Neutral { 0.12 } else { 0.15 };
    row()
        .h(design::styles::rem(size + 8.))
        .px(6.)
        .center_y()
        .radius(design::CONTROL_RADIUS)
        .fill(ink.alpha(wash))
        .child(
            text(label.to_string())
                .font(Font::new(design::styles::rem(size), 500.))
                .color(ink)
                .single_line()
                .truncate(),
        )
}

/// A spinner while a mount's first tree is on its way.
pub fn loading(k: &Kit, base: &str) -> El {
    row()
        .key(format!("{base}-loading"))
        .w_full()
        .justify_center()
        .py(24.)
        .child(rune_kit::Spinner::new().size(16.).build(k))
}

/// The plugin's reason it has nothing to show: a quiet muted line, not an
/// alarm (the rest of the app is fine).
pub fn error(p: &Palette, base: &str, reason: &str, density: Density) -> El {
    // The mount pads it as it pads a tree.
    let _ = density;
    row()
        .key(format!("{base}-error"))
        .w_full()
        .py(8.)
        .gap(6.)
        .items_start()
        .child(
            icon(&icons::CIRCLE_ALERT, 14.)
                .color(p.text_faint)
                .a11y_hidden(),
        )
        .child(
            text(reason.to_string())
                .font(ts::xs())
                .color(p.text_muted)
                .wrap()
                .grow(1.)
                .min_w(0.),
        )
}

struct Ctx<'a> {
    s: &'a State,
    k: &'a Kit,
    mount: &'a MountKey,
    density: Density,
    busy: bool,
}

/// Draw `node` for `mount` at `density`.
pub fn tree(s: &State, k: &Kit, mount: &MountKey, node: &Node, density: Density) -> El {
    let busy = s.plugins.mounts.get(mount).is_some_and(|m| m.busy);
    let cx = Ctx {
        s,
        k,
        mount,
        density,
        busy,
    };
    let base = mount.el_key();
    col()
        .key(base.clone())
        .w_full()
        .child(draw(&cx, node, &base, false))
}

fn draw(cx: &Ctx, node: &Node, path: &str, in_row: bool) -> El {
    let p = &cx.s.palette;
    match node {
        Node::Col { gap, children } | Node::Row { gap, children } => {
            let is_row = matches!(node, Node::Row { .. });
            let mut out = if is_row {
                row().items_center()
            } else {
                col().w_full()
            }
            .key(path.to_string())
            .gap(gap.unwrap_or(cx.density.gap()));
            if is_row {
                out = out.w_full().min_w(0.);
            }
            for (ix, child) in children.iter().enumerate() {
                if matches!(child, Node::Unknown) {
                    continue;
                }
                out = out.child(draw(cx, child, &format!("{path}-{ix}"), is_row));
            }
            out
        }
        Node::Text {
            text: t,
            style,
            tone,
        } => {
            let fallback = match style {
                TextStyle::Caption => p.text_muted,
                _ => p.text,
            };
            let el = text(t.clone())
                .key(path.to_string())
                .font(cx.density.font(*style))
                .color(ink(p, *tone, fallback))
                .selectable(true)
                .wrap();
            if in_row {
                el.min_w(0.).shrink()
            } else {
                el.w_full()
            }
        }
        Node::Badge { text: t, tone } => el().key(path.to_string()).child(chip(p, t, *tone, 12.)),
        Node::Button {
            label,
            action,
            payload,
            style,
        } => button(cx, path, label, action, payload, *style),
        Node::List { items } => list(cx, path, items),
        Node::Markdown { source } => el().key(path.to_string()).w_full().child(
            Markdown::new(source.clone())
                .style(MarkdownStyle::from_kit(cx.k).body_font(cx.density.body_with_leading()))
                .selectable(true)
                .build(cx.k),
        ),
        Node::Divider => el()
            .key(path.to_string())
            .w_full()
            .h(1.)
            .my(2.)
            .fill(p.border.alpha(p.border.a * 0.6))
            .a11y_hidden(),
        Node::Unknown => el().key(path.to_string()),
    }
}

fn press(cx: &Ctx, action: &str, payload: &Value) -> impl Fn(&mut State) + 'static {
    let (key, action, payload) = (cx.mount.clone(), action.to_string(), payload.clone());
    move |s: &mut State| super::event(s, key.clone(), action.clone(), payload.clone())
}

fn button(
    cx: &Ctx,
    path: &str,
    label: &str,
    action: &str,
    payload: &Value,
    style: ButtonStyle,
) -> El {
    let look = match style {
        ButtonStyle::Primary => Look::Primary,
        ButtonStyle::Secondary => Look::Outline,
        ButtonStyle::Ghost => Look::Ghost,
    };
    let size = match cx.density {
        Density::Page => Size::Small,
        Density::Panel => Size::XSmall,
    };
    // A row keeps it at its own width inside a stretching column.
    row().key(path.to_string()).child(
        button_sized(label, look, size)
            .disabled(cx.busy)
            .on_press(press(cx, action, payload))
            .build(cx.k)
            .press_scale(motion::PRESS),
    )
}

fn list(cx: &Ctx, path: &str, items: &[ListItem]) -> El {
    let p = &cx.s.palette;
    let rows = items
        .iter()
        .enumerate()
        .map(|(ix, item)| list_item(cx, &format!("{path}-{ix}"), item));
    match cx.density {
        // The pages' block: the wash fill, hairlines between rows.
        Density::Page => {
            let mut card = col()
                .key(path.to_string())
                .w_full()
                .radius(12.)
                .fill(block_fill(p))
                .clip_rounded(12.);
            for (ix, r) in rows.enumerate() {
                if ix > 0 {
                    card = card.child(
                        el().key(format!("{path}-rule-{ix}"))
                            .mx(16.)
                            .h(1.)
                            .fill(p.border.alpha(p.border.a * 0.6))
                            .a11y_hidden(),
                    );
                }
                card = card.child(r);
            }
            card
        }
        // The pane's plain rows, as its other lists draw them.
        // Their wash reaches past the text, which lines up with its siblings.
        // (Stretched, not `w_full`, so the margins widen it.)
        Density::Panel => col().key(path.to_string()).mx(-8.).gap(1.).children(rows),
    }
}

fn list_item(cx: &Ctx, path: &str, item: &ListItem) -> El {
    let p = &cx.s.palette;
    let (title_font, pad_x, pad_y) = match cx.density {
        Density::Page => (ts::sm_medium(), 16., 10.),
        Density::Panel => (cx.density.font(TextStyle::Body), 8., 6.),
    };
    let mut words = col().grow(1.).min_w(0.).gap(1.).child(
        text(item.title.clone())
            .font(title_font)
            .color(p.text)
            .truncate(),
    );
    if let Some(sub) = &item.subtitle {
        words = words.child(
            text(sub.clone())
                .font(ts::xs())
                .color(p.text_muted)
                .truncate(),
        );
    }
    let mut out = row()
        .key(path.to_string())
        .w_full()
        .px(pad_x)
        .py(pad_y)
        .gap(8.)
        .items_center()
        .radius(match cx.density {
            Density::Page => 0.,
            Density::Panel => design::ROW_RADIUS,
        })
        .child(words);
    if let Some(trailing) = &item.trailing
        && !matches!(**trailing, Node::Unknown)
    {
        out = out.child(
            el().shrink()
                .child(draw(cx, trailing, &format!("{path}-end"), true)),
        );
    }
    if let Some(action) = &item.action {
        out = out
            .role("button")
            .label(item.title.clone())
            .cursor(Cursor::Pointer)
            // While the plugin answers a press the rows that act dim, and
            // light again as the answer lands, on the slide spring.
            .opacity(if cx.busy { BUSY_DIM } else { 1. })
            .opacity_motion(motion::SLIDE);
        if !cx.busy {
            // The hover wash fades and a press sinks the row a hair, as
            // the app's own rows do.
            out = out
                .hover(design::kit::hover_wash(p))
                .press(design::kit::hover_wash(p))
                .hover_motion(motion::HOVER, motion::HOVER)
                .press_scale(motion::PRESS_CARD)
                .on_press(press(cx, action, &item.payload));
        }
        out = out.child(
            icon(&icons::CHEVRON_RIGHT, 14.)
                .color(p.text_faint)
                .a11y_hidden(),
        );
    }
    out
}
