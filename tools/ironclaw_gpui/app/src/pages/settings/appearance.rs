//! Settings → Appearance: the design style, theme presets and the window
//! wallpaper. All client-side preferences (`design::prefs`), applied live.

use super::SettingsView;
use crate::design::prefs::{Prefs, Wallpaper};
use crate::design::styles::{Material, Style, StyleId};
use crate::design::{self, Palette, kit, motion, themes};
use crate::ui::*;

/// The four styles as live miniatures of the app: sidebar, a selected row,
/// two messages and the composer, each painted with that style's palette,
/// radii and material.
fn style_gallery(cx: &mut Context<SettingsView>) -> impl IntoElement {
    let current = Style::of(cx).id;
    let p = *Palette::of(cx);
    let s = *Style::of(cx);
    h_flex()
        .flex_wrap()
        .gap_3()
        .children(StyleId::ALL.into_iter().map(|id| {
            let key: SharedString = format!("style-card-{}", id.key()).into();
            let h = motion::hover_t(&key);
            let on = motion::state_t(&format!("{key}:on"), current == id, s.slide, cx.reduce_motion());
            v_flex()
                .id(ElementId::Name(key.clone()))
                .relative()
                .w(px(232.))
                .gap_2()
                .p(px(8.))
                .rounded(px(s.panel_radius.max(8.) + 2.))
                .border_1()
                .border_color(motion::mix(
                    motion::mix(p.border, p.border_strong, h),
                    p.accent,
                    on,
                ))
                .bg(motion::mix(p.hover.opacity(0.), p.hover, h * 0.6))
                .shadow(vec![gpui_kit::BoxShadow {
                    color: p.accent.opacity(0.28 * on),
                    offset: gpui_kit::point(px(0.), px(0.)),
                    blur_radius: px(0.),
                    spread_radius: px(2. * on),
                    inset: false,
                }])
                .cursor_pointer()
                .on_hover(motion::hover_listener(key.clone(), s.hover))
                .active(|st| st.top(px(1.)))
                .on_click(move |_, _, cx| design::set_style(id, cx))
                .child(miniature(id, h))
                .child(
                    v_flex()
                        .px_1()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .child(div().text_sm().font_semibold().child(id.name()))
                                .when(current == id, |d| {
                                    d.child(Icon::new(IconName::Check).xsmall().text_color(p.accent))
                                }),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(p.text_muted)
                                .child(id.blurb()),
                        ),
                )
        }))
}

fn miniature(id: StyleId, h: f32) -> impl IntoElement {
    let st = Style::get(id);
    let t = themes::find(id.theme());
    let p = Palette::from_seed(t);
    let r = |v: f32| px((v * 0.55).max(if v > 0. { 2. } else { 0. }));
    let line = |w: f32, c: gpui_kit::Hsla| div().h(px(4.)).w(px(w)).rounded_full().bg(c);
    let sidebar_row = |w: f32, on: bool| {
        h_flex()
            .relative()
            .h(px(14.))
            .px_1p5()
            .items_center()
            .rounded(r(st.control_radius))
            .when(on, |d| {
                d.bg(match st.material {
                    Material::Line => p.accent.opacity(0.12),
                    Material::Ink => p.surface_card,
                    _ => p.active,
                })
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(3.))
                        .w(px(2.))
                        .h(px(8.))
                        .rounded_full()
                        .bg(p.accent),
                )
            })
            .child(line(w, if on { p.text } else { p.text_faint }))
    };
    let shell = if st.vibrancy {
        // No desktop behind a miniature: suggest the blur with a wash.
        gpui_kit::linear_gradient(
            135.,
            gpui_kit::linear_color_stop(motion::mix(p.surface, p.accent, 0.35), 0.),
            gpui_kit::linear_color_stop(motion::mix(p.surface, design::hex(0xff7ac8), 0.25), 1.),
        )
    } else {
        gpui_kit::Background::from(p.surface)
    };
    let panel_plate = kit::panel_shadows(&st, &p);
    h_flex()
        .h(px(132.))
        .w_full()
        .overflow_hidden()
        .rounded(px(st.panel_radius.max(4.)))
        .bg(shell)
        .border_1()
        .border_color(p.border)
        .child(
            v_flex()
                .w(px(62.))
                .h_full()
                .p_1p5()
                .gap_1()
                .when(st.material == Material::Line, |d| d.border_r_1().border_color(p.border))
                .child(h_flex().gap(px(3.)).pb_1().children(
                    [0xff5f57u32, 0xfebc2e, 0x28c840].map(|c| div().size(px(5.)).rounded_full().bg(design::hex(c))),
                ))
                .child(sidebar_row(30., false))
                .child(sidebar_row(36., true))
                .child(sidebar_row(24., false))
                .child(sidebar_row(32., false)),
        )
        .child(
            div().flex_1().h_full().py(px(st.panel_inset * 0.6)).pr(px(st.panel_inset * 0.6)).child(
                v_flex()
                    .size_full()
                    .rounded(r(st.panel_radius))
                    .when(st.panel_inset > 0., |d| d.border_1().border_color(p.border))
                    .shadow(panel_plate)
                    .bg(p.bg.opacity(st.panel_alpha.max(0.75)))
                    .p_2()
                    .gap_1p5()
                    .child(
                        h_flex().justify_end().child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded(r(st.bubble_radius))
                                .bg(p.surface_raised.opacity(if p.light { 0.7 } else { 0.9 }))
                                .child(line(54., p.text_muted)),
                        ),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child(line(110., p.text_muted))
                            .child(line(90., p.text_muted))
                            .child(line(64. + 6. * h, p.accent.opacity(0.8))),
                    )
                    .child(div().flex_1())
                    .child({
                        let dock = kit::neutral(&st, &p, 0., 1.).apply(
                            h_flex()
                                .h(px(22.))
                                .px_1p5()
                                .items_center()
                                .rounded(r(st.composer_radius))
                                .child(line(48., p.text_faint))
                                .child(div().flex_1()),
                        );
                        dock.child(
                            kit::accent(&st, &p, h).apply(
                                div().size(px(14.)).rounded(r(st.control_radius.max(6.))),
                            ),
                        )
                    }),
            ),
        )
}


pub fn render(cx: &mut Context<SettingsView>) -> impl IntoElement {
    let prefs = Prefs::of(cx).clone();
    let p = *Palette::of(cx);

    let swatches = themes::THEMES.iter().enumerate().map(|(ix, t)| {
        let selected = prefs.theme == t.id;
        let dot = |c: u32| div().size(px(10.)).rounded_full().bg(design::hex(c));
        v_flex()
            .id(("theme", ix))
            .w(px(132.))
            .gap_1p5()
            .p_2()
            .rounded(px(design::CONTROL_RADIUS + 2.))
            .border_1()
            .border_color(if selected { p.accent } else { p.border })
            .bg(design::hex(t.bg))
            .cursor_pointer()
            .hover(|s| s.border_color(p.border_strong))
            .on_click(move |_, _, cx| design::set_theme(t.id, cx))
            .child(
                h_flex()
                    .gap_1()
                    .child(dot(t.accent))
                    .child(dot(t.success))
                    .child(dot(t.warning))
                    .child(dot(t.danger)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(design::hex(t.text))
                    .truncate()
                    .child(t.name),
            )
    });

    let choice = |id: &'static str, label: &'static str, active: bool, pick: Wallpaper| {
        Button::new(id)
            .label(label)
            .small()
            .when(active, |b| b.primary())
            .when(!active, |b| b.ghost())
            .on_click(move |_, _, cx| design::set_wallpaper(pick.clone(), cx))
    };
    let picture_name = match &prefs.wallpaper {
        Wallpaper::Image { path } => path.file_name().map(|n| n.to_string_lossy().to_string()),
        _ => None,
    };

    v_flex()
        .gap_3()
        .child(div().font_semibold().child("Style"))
        .child(style_gallery(cx))
        .child(div().pt_2().font_semibold().child("Palette"))
        .child(h_flex().flex_wrap().gap_2().children(swatches))
        .child(
            card(cx).child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().font_semibold().child("Wallpaper"))
                            .child(div().text_xs().text_color(p.text_muted).truncate().child(
                                picture_name.unwrap_or_else(|| {
                                    "Drawn once behind the window; no cost while idle.".into()
                                }),
                            )),
                    )
                    .child(choice(
                        "wallpaper-none",
                        "None",
                        prefs.wallpaper == Wallpaper::None,
                        Wallpaper::None,
                    ))
                    .child(choice(
                        "wallpaper-aurora",
                        "Aurora",
                        prefs.wallpaper == Wallpaper::Aurora,
                        Wallpaper::Aurora,
                    ))
                    .child(
                        Button::new("wallpaper-pick")
                            .label("Picture…")
                            .small()
                            .when(matches!(prefs.wallpaper, Wallpaper::Image { .. }), |b| {
                                b.primary()
                            })
                            .when(!matches!(prefs.wallpaper, Wallpaper::Image { .. }), |b| {
                                b.ghost()
                            })
                            .on_click(cx.listener(|_, _, window, cx| pick_picture(window, cx))),
                    ),
            ),
        )
}

fn pick_picture(window: &mut Window, cx: &mut Context<SettingsView>) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: true,
        directories: false,
        multiple: false,
        prompt: Some("Use as wallpaper".into()),
    });
    cx.spawn_in(window, async move |_, cx| {
        if let Ok(Ok(Some(mut paths))) = picked.await
            && let Some(path) = paths.pop()
        {
            let _ = cx.update(|_, cx| design::set_wallpaper(Wallpaper::Image { path }, cx));
        }
    })
    .detach();
}
