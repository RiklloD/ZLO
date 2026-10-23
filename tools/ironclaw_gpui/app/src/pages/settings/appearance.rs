//! Settings → Appearance: theme presets and the window wallpaper. Both are
//! client-side preferences (`design::prefs`), applied instantly.

use super::SettingsView;
use crate::design::prefs::{Prefs, Wallpaper};
use crate::design::{self, Palette, themes};
use crate::ui::*;

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
