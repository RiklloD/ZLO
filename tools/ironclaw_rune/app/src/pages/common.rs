//! Shared page helpers: the page column, its title row, settings groups and
//! rows, cards, tags and the small controls the pages share. Areas C, D and E
//! build their pages from these; keep the API small and stable. [`ts`] holds
//! the text styles, [`controls`] the sized and keyed controls and the smaller
//! pieces.

pub mod controls;
pub mod ts;

use rune::{Color, El, ScrollSpec, TextStyle, Tx, col, el, icon, row, text};
use rune_kit::{
    Button, ButtonVariant, ControlSize, IconButton, IconButtonVariant, Kit, SegmentedControl,
    Switch,
};

use crate::design::{Palette, motion};
use crate::state::State;

/// Zeron's settings column (`widgets::page_column`).
pub const PAGE_MAX_WIDTH: f32 = 760.;

/// Geist at `size` px (at the 16 px base, scaled to the UI font size) and
/// `weight` (400 regular, 500 medium, 600 semibold).
pub fn font(size: f32, weight: f32) -> TextStyle {
    TextStyle::new(crate::design::styles::rem(size), weight)
}

/// Text sizes of the app's rem scale (16 px base); [`font`] and the
/// helpers built on it scale them to the UI font size.
pub const XS: f32 = 12.;
pub const SM: f32 = 14.;
pub const BASE: f32 = 16.;
pub const XL: f32 = 20.;

/// A line of text in one size, weight and colour.
pub fn label(s: impl Into<String>, size: f32, weight: f32, color: Color) -> El {
    text(s).font(font(size, weight)).color(color)
}

/// Like [`label`], wrapping.
pub fn para(s: impl Into<String>, size: f32, color: Color) -> El {
    text(s).font(font(size, 400.)).color(color).wrap()
}

/// Title row with page-level actions on the right: Zeron's 20 px medium
/// page title, flush with the cards and text below it so every page keeps
/// one left edge.
pub fn header(p: &Palette, title: &str, actions: El) -> El {
    row()
        .key("page-header")
        .w_full()
        .justify_between()
        .center_y()
        .pb(8.)
        .child(label(title, XL, 500., p.text))
        .child(actions)
}

/// A row of header actions, 8 px apart.
pub fn actions(items: impl IntoIterator<Item = El>) -> El {
    row().gap(4.).center_y().children(items)
}

/// A settings group's label, Zeron's `section_label`: 16 px semibold text,
/// flush with the page's left edge, 32 px above the group, with an optional
/// line of explanation.
pub fn group_header(p: &Palette, title: &str, note: Option<&str>) -> El {
    let mut d = col()
        .key(format!("group-{title}"))
        .w_full()
        .pt(32.)
        .pb(8.)
        .gap(2.)
        .child(
            label(title, BASE, 600., p.text)
                .font(font(BASE, 600.).lh(crate::design::styles::rem(22.))),
        );
    if let Some(note) = note {
        d = d.child(para(note, XS, p.text_muted));
    }
    d
}

/// Zeron's block fill: a faint wash of the theme's ink, no outline.
pub fn block_fill(p: &Palette) -> Color {
    if p.light {
        Color::BLACK.alpha(0.045)
    } else {
        Color::WHITE.alpha(0.045)
    }
}

/// Related settings in one block: 12 px radius, the wash fill, hairlines
/// between rows inset 16 px.
pub fn rows_card(p: &Palette, rows: impl IntoIterator<Item = El>) -> El {
    let mut card = col()
        .w_full()
        .radius(12.)
        .fill(block_fill(p))
        .clip_rounded(12.);
    for (ix, r) in rows.into_iter().enumerate() {
        if ix > 0 {
            card = card.child(
                el().key(format!("rule-{ix}"))
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

/// One setting, Zeron's `card_row`: a 14 px medium title and a 12 px muted
/// description on the left, the control on the right, at least 60 px tall.
pub fn setting_row(p: &Palette, title: &str, description: Option<&str>, control: El) -> El {
    controls::keyed_setting_row(p, &format!("setting-{title}"), title, description, control)
}

/// Centered, width-capped page body that scrolls.
pub fn page_body(key: &str, content: El) -> El {
    col()
        .key(key.to_string())
        .w_full()
        .h_full()
        .min_h(0.)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            row()
                .w_full()
                .justify_center()
                .px(40.)
                .pt(28.)
                .pb(48.)
                .child(col().w_full().max_w(PAGE_MAX_WIDTH).child(content)),
        )
}

/// [`page_body`] for a page that ends in a long list: the page is one
/// virtual list of `count` rows, row `i` keyed `key(s, i)` and built by
/// `build(s, i)` only while it is near the viewport (row 0 is usually the
/// page's head). Each row sits in the page's centred column; rows carry
/// their own vertical spacing.
pub fn page_body_virtual(
    page_key: &str,
    count: usize,
    estimate: f32,
    key: impl Fn(&State, usize) -> String + 'static,
    build: impl Fn(&State, usize) -> El + 'static,
) -> El {
    col()
        .key(page_key.to_string())
        .w_full()
        .h_full()
        .min_h(0.)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            col()
                .key("page-rows")
                .w_full()
                .growth(rune::GrowthSpec::default())
                .whole_rows()
                .virtual_rows(
                    count,
                    estimate,
                    move |s: &State, i| key(s, i).into(),
                    move |s: &State, i| {
                        row()
                            .w_full()
                            .justify_center()
                            .px(40.)
                            .pt(if i == 0 { 28. } else { 0. })
                            .pb(if i + 1 == count { 48. } else { 0. })
                            .child(col().w_full().max_w(PAGE_MAX_WIDTH).child(build(s, i)))
                    },
                ),
        )
}

/// A quiet centered line for empty lists and placeholders. It fades in and
/// out (`motion::fade`) as the list empties or fills.
pub fn empty_state(p: &Palette, msg: impl Into<String>) -> El {
    row()
        .transition(motion::fade())
        .w_full()
        .py(48.)
        .justify_center()
        .child(para(msg, SM, p.text_muted).align(rune::Align::Center))
}

/// A request's error in danger red. It wipes open from its top and fades
/// (`motion::reveal`) when it appears after a press, and out when cleared;
/// key it at the call site (`.key("save-error")`) if its siblings come and
/// go.
pub fn error_line(p: &Palette, msg: &str) -> El {
    col()
        .w_full()
        .pb(12.)
        .transition(motion::reveal())
        .child(para(msg, SM, p.danger))
}

/// A status line (success, warning) under the header; arrives and leaves
/// as [`error_line`] does.
pub fn notice_line(msg: &str, color: Color) -> El {
    col()
        .w_full()
        .pb(12.)
        .transition(motion::reveal())
        .child(para(msg, SM, color))
}

/// The ghost refresh icon every page header carries.
pub fn refresh_button(k: &Kit, on_press: impl Fn(&mut State) + 'static) -> El {
    pressable(
        icon_button(&crate::icons::REFRESH_CW, "Refresh")
            .on_press(on_press)
            .build(k),
    )
}

/// [`refresh_button`] that breathes its arrows while `loading` (a list
/// reloading after the press) and takes no press meanwhile, instead of
/// sitting still until the rows land. Key it as [`controls::keyed_refresh`]
/// is keyed. The breath runs at 15 fps only while loading and holds still
/// under Reduce Motion. It stands 24 pt tall, the height of the header
/// controls it sits beside (segmented switches, push buttons).
pub fn refresh_busy(
    k: &Kit,
    key: &str,
    loading: bool,
    on_press: impl Fn(&mut State) + 'static,
) -> El {
    let ink = crate::design::kit::icon_ink(k, IconButtonVariant::Ghost);
    let glyph = crate::design::kit::busy_glyph(
        "refresh-glyph",
        &crate::icons::REFRESH_CW,
        loading,
        16.,
        ink,
    );
    crate::design::kit::glyph_button_sized(
        k,
        key,
        IconButtonVariant::Ghost,
        ControlSize::Regular,
        "Refresh",
        glyph,
        move |s: &mut State, _: &mut Tx| {
            if !loading {
                on_press(s);
            }
        },
    )
}

/// The card: the neutral raised plate (a faint lit fill, a hairline
/// rim), 12 px padding, rows 8 px apart, on continuous corners. A card the
/// person can press also takes `.press_scale(motion::PRESS_CARD)`.
pub fn card(p: &Palette) -> El {
    let (fill, rim) = if p.light {
        (p.surface_card, Color::BLACK.alpha(0.07))
    } else {
        (Color::WHITE.alpha(0.06), Color::WHITE.alpha(0.095))
    };
    col()
        .w_full()
        .gap(8.)
        .padding(12.)
        .radius(crate::design::PANEL_RADIUS)
        .continuous_corners()
        .fill(fill)
        .border(1., rim)
}

/// What a tag says: success, warning or neutral.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Success,
    Warning,
    Danger,
    Secondary,
}

/// A small status tag: a tinted pill, no rim, one line. Every status in the
/// app (Active, Paused, Needs setup, Ready, On) wears this one look. A tone
/// change on a kept tag morphs its tint (`motion::SLIDE`).
pub fn tag(p: &Palette, s: &str, tone: Tone) -> El {
    let ink = match tone {
        Tone::Success => p.success,
        Tone::Warning => p.warning,
        Tone::Danger => p.danger,
        Tone::Secondary => p.text_muted,
    };
    row()
        .h(20.)
        .px(6.)
        .center_y()
        .radius(crate::design::CONTROL_RADIUS)
        .continuous_corners()
        .fill(ink.alpha(if tone == Tone::Secondary { 0.12 } else { 0.15 }))
        .motion(motion::SLIDE)
        .child(label(s, XS, 500., ink).single_line())
}

/// Button flavours.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Btn {
    /// The default button: the neutral raised plate.
    Secondary,
    Primary,
    Ghost,
    Outline,
    Danger,
    /// A ghost with muted ink: the unpicked side of a [`choice`].
    Quiet,
}

/// A small labelled button; chain `on_press`, `disabled`, `icon` and
/// `build(k)`. It returns kit's builder, so the press shrink goes on the built
/// element: `button(..).on_press(f).build(k).press_scale(motion::PRESS)`, or
/// use [`pressable`]. The keyed `controls::press_*` helpers already do.
pub fn button(s: &str, kind: Btn) -> Button {
    Button::new(s)
        .variant(match kind {
            Btn::Secondary => ButtonVariant::Neutral,
            Btn::Primary => ButtonVariant::Submit,
            Btn::Ghost => ButtonVariant::Ghost,
            Btn::Outline => ButtonVariant::Outline,
            Btn::Danger => ButtonVariant::Danger,
            Btn::Quiet => ButtonVariant::GhostMuted,
        })
        .control_size(ControlSize::Regular)
}

pub fn button_xs(s: &str, kind: Btn) -> Button {
    button(s, kind).control_size(ControlSize::Small)
}

/// The kit's loading look, not pressable, while `on`.
pub fn loading(b: Button, on: bool) -> Button {
    if on {
        b.variant(ButtonVariant::Loading).disabled(true)
    } else {
        b
    }
}

/// The pages' choice chips: the picked one sits on the neutral raised
/// plate, the rest are quiet ghosts. Built, it takes the press shrink as
/// [`button`] does ([`pressable`]). Picking is not an action, so it never
/// wears the accent (that stays for the page's one primary button). For a
/// fixed set of a few options prefer [`tab_bar`] / [`segmented`].
pub fn choice(s: &str, on: bool) -> Button {
    button(s, choice_look(on))
}

/// A built kit button (or icon button) that shrinks while pressed, as
/// every ZLO control does: `pressable(choice("Week", on).on_press(f).build(k))`.
pub fn pressable(built: El) -> El {
    built.press_scale(motion::PRESS)
}

/// [`choice`]'s look, for callers that build their own button.
pub fn choice_look(on: bool) -> Btn {
    if on { Btn::Secondary } else { Btn::Quiet }
}

/// A ghost icon button with a tooltip that also names it.
pub fn icon_button(i: &'static rune::Icon, tip: &str) -> IconButton {
    IconButton::new(i)
        .variant(IconButtonVariant::Ghost)
        .control_size(ControlSize::Small)
        .help(tip)
        .label(tip)
}

/// A switch with an accessible name (no visible label).
pub fn switch(
    k: &Kit,
    checked: bool,
    disabled: bool,
    name: &str,
    on_toggle: impl Fn(&mut State) + 'static,
) -> El {
    let mut sw = Switch::new(checked)
        .label(name)
        .hide_label()
        .control_size(ControlSize::Small);
    if disabled {
        sw = sw.disabled(true);
    } else {
        sw = sw.on_toggle(on_toggle);
    }
    sw.build(k)
}

/// Equal segments, one picked.
pub fn segmented(
    k: &Kit,
    labels: &[&str],
    selected: usize,
    disabled: bool,
    on_pick: impl Fn(&mut State, usize, &mut Tx) + Clone + 'static,
) -> El {
    // 54 px segments are too narrow for the kit's font, so each fits its widest label.
    let mut seg = SegmentedControl::new(labels, Some(selected)).fit();
    let seg = if disabled {
        for i in 0..labels.len() {
            seg = seg.segment_state(i, rune_kit::State::Disabled);
        }
        seg.build(k)
    } else {
        seg.on_pick(on_pick).build(k)
    };
    // A row keeps it at its own width inside stretching columns.
    row().child(seg)
}

/// A page's tab bar (Operator's tabs, Memory's stores, Files' mounts, a
/// provider list's kinds): the kit's segmented control, so every switch
/// between views of one page looks and moves the same way: the raised
/// highlight glides, resizing as it goes, from the old tab to the picked one
/// (the kit's equal-column indicator, moving on its own leaf). The accent
/// stays for the page's one primary action.
pub fn tab_bar<T: Copy + PartialEq + 'static>(
    k: &Kit,
    key: &str,
    tabs: &[(T, &str)],
    current: T,
    on_pick: impl Fn(&mut State, T) + Clone + 'static,
) -> El {
    let labels: Vec<&str> = tabs.iter().map(|(_, l)| *l).collect();
    let values: Vec<T> = tabs.iter().map(|(t, _)| *t).collect();
    let selected = values.iter().position(|t| *t == current).unwrap_or(0);
    // Equal segments at the widest label's width while the page has room,
    // sharing the column's width when it does not (a narrow window, a large
    // UI font): the bar never runs off the page. Equal columns filling the
    // wrapper, not the kit's fill mode: that one paints the picked segment
    // itself, so a switch jumps.
    let seg = SegmentedControl::new(&labels, Some(selected))
        .fit()
        .on_pick(move |s: &mut State, ix, _: &mut Tx| {
            if let Some(t) = values.get(ix) {
                on_pick(s, *t);
            }
        })
        .build(k)
        .w_full();
    let widest = labels.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f32;
    let natural = labels.len() as f32 * (widest * 7.4 + 28.) * crate::design::styles::type_scale();
    el().key(key.to_string())
        .w_full()
        .max_w(natural)
        .min_w(0.)
        .child(seg)
}

/// An icon at `size` in `color`.
pub fn glyph(i: &'static rune::Icon, size: f32, color: Color) -> El {
    icon(i, size).color(color).a11y_hidden()
}
