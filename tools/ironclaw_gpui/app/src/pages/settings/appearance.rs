//! Settings → Appearance, laid out after Zeron's page
//! (`crates/ui/src/settings/appearance.rs`, MIT), one decision per row:
//! the color scheme, a theme for each side of it, an accent, then the
//! new-chat wallpaper and its effect, motion and layout. All client-side
//! preferences (`design::prefs`), applied live.

use std::rc::Rc;

use super::SettingsView;
use crate::design::prefs::{Prefs, Wallpaper};
use crate::design::styles::Style;
use crate::design::{self, Palette, kit, motion, paint, themes};
use crate::pages::{block_fill, group_header, rows_card, setting_row};
use crate::ui::*;

const CARD_HEIGHT: f32 = 148.;
const CARD_RADIUS: f32 = 6.;

fn rem(px_at_16: f32) -> gpui_kit::Rems {
    gpui_kit::rems(px_at_16 / 16.)
}

/// A small painting of the app in palette `p`: sidebar, a selected row, a
/// message, some answer lines and the composer pill.
fn preview(p: Palette) -> gpui_kit::Div {
    let line = |w: f32, c: gpui_kit::Hsla| div().h(px(4.)).w(px(w)).rounded_full().bg(c);
    h_flex()
        .size_full()
        .bg(p.surface)
        .child(
            v_flex()
                .w(px(54.))
                .h_full()
                .p(px(7.))
                .gap(px(6.))
                .child(
                    h_flex().gap(px(3.)).pb(px(2.)).children(
                        [0xff5f57u32, 0xfebc2e, 0x28c840]
                            .map(|c| div().size(px(5.)).rounded_full().bg(design::hex(c))),
                    ),
                )
                .child(line(30., p.text_faint))
                .child(
                    div()
                        .px(px(3.))
                        .py(px(3.))
                        .rounded(px(4.))
                        .bg(p.active)
                        .child(line(28., p.text)),
                )
                .child(line(24., p.text_faint))
                .child(line(32., p.text_faint)),
        )
        .child(
            v_flex()
                .flex_1()
                .h_full()
                .my(px(5.))
                .mr(px(5.))
                .rounded(px(4.))
                .bg(p.bg)
                .p(px(8.))
                .gap(px(6.))
                .child(
                    h_flex().justify_end().child(
                        div()
                            .px(px(6.))
                            .py(px(4.))
                            .rounded(px(6.))
                            .bg(p.text.opacity(0.08))
                            .child(line(40., p.text_muted)),
                    ),
                )
                .child(line(90., p.text_muted))
                .child(line(70., p.text_muted))
                .child(line(46., p.accent))
                .child(div().flex_1())
                .child(
                    h_flex()
                        .h(px(18.))
                        .px(px(6.))
                        .items_center()
                        .rounded(px(8.))
                        .border_1()
                        .border_color(p.border_strong)
                        .child(line(36., p.text_faint))
                        .child(div().flex_1())
                        .child(div().size(px(10.)).rounded_full().bg(p.text)),
                ),
        )
}

/// Zeron's option card: the preview is the control, a quiet accent edge
/// marks the choice and the caption sits underneath.
fn option_card(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    selected: bool,
    body: gpui_kit::Div,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let p = *Palette::of(cx);
    let s = *Style::of(cx);
    let t = motion::state_t(&format!("{id}:on"), selected, s.slide, cx.reduce_motion());
    let key = SharedString::from(id);
    let h = motion::hover_t(&key);
    let caption = motion::mix(p.text_muted, p.accent, t);
    v_flex()
        .id(id)
        .flex_1()
        .min_w_0()
        .items_center()
        .gap(px(8.))
        .cursor_pointer()
        .on_hover(motion::hover_listener(key, s.hover))
        .child(
            div()
                .h(px(CARD_HEIGHT))
                .w_full()
                .flex_none()
                .rounded(px(CARD_RADIUS))
                .overflow_hidden()
                .border_1()
                .border_color(motion::mix(
                    motion::mix(p.border, p.border_strong, h),
                    p.accent,
                    t,
                ))
                .child(body.rounded(px(CARD_RADIUS - 1.))),
        )
        .child(
            h_flex()
                .gap(px(6.))
                .items_center()
                .text_size(rem(13.))
                .when(selected, |d| d.font_weight(gpui_kit::FontWeight::MEDIUM))
                .text_color(caption)
                .child(Icon::new(icon).size(px(16.)).text_color(caption))
                .child(label),
        )
}

/// Zeron's select: a 32 px button naming the choice, opening a menu of the
/// options under it. `open` is this select's id when its menu is showing.
fn select(
    id: &'static str,
    current: String,
    options: Vec<(String, String)>,
    open: bool,
    pick: impl Fn(&str, &mut App) + 'static,
    cx: &mut Context<SettingsView>,
) -> AnyElement {
    let p = *Palette::of(cx);
    let pick = Rc::new(pick);
    let label = options
        .iter()
        .find(|(v, _)| *v == current)
        .map(|(_, l)| l.clone())
        .unwrap_or_else(|| current.clone());
    let key = SharedString::from(format!("select-{id}"));
    let h = motion::hover_t(&key);
    let button = h_flex()
        .id(key.clone())
        .role(Role::Button)
        .aria_label(label.clone())
        .h(px(32.))
        .min_w(px(140.))
        .px(px(12.))
        .gap(px(8.))
        .items_center()
        .justify_between()
        .rounded(px(8.))
        .bg(motion::mix(block_fill(cx), p.hover, h))
        .border_1()
        .border_color(p.border)
        .text_size(rem(13.))
        .text_color(p.text)
        .cursor_pointer()
        .on_hover(motion::hover_listener(key, Style::of(cx).hover))
        .child(div().truncate().child(label))
        .child(
            Icon::new(IconName::ChevronsUpDown)
                .size(px(13.))
                .text_color(p.text_muted),
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            this.appearance_menu = if this.appearance_menu == Some(id) {
                None
            } else {
                Some(id)
            };
            cx.notify();
        }));
    let menu = open.then(|| {
        let pop = kit::popover(SharedString::from(format!("{id}-menu")), cx);
        let rows = options.into_iter().map(|(value, label)| {
            let on = value == current;
            let pick = pick.clone();
            kit::row(SharedString::from(format!("{id}-opt-{value}")), on, cx)
                .gap_2()
                .text_size(rem(13.))
                .child(div().flex_1().truncate().child(label))
                .when(on, |d| {
                    d.child(
                        Icon::new(IconName::Check)
                            .size(px(13.))
                            .text_color(p.text_muted),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.appearance_menu = None;
                    pick(&value, cx);
                    cx.notify();
                }))
        });
        // Anchored under the button's right edge; near the window bottom it
        // slides up so every option stays on screen.
        gpui_kit::deferred(
            div().absolute().top(px(36.)).right_0().child(
                gpui_kit::anchored()
                    .anchor(gpui_kit::Anchor::TopRight)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .w(px(220.))
                            .occlude()
                            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                                this.appearance_menu = None;
                                cx.notify();
                            }))
                            .child(pop(v_flex().w_full().child(
                                div()
                                    .id(SharedString::from(format!("{id}-scroll")))
                                    .max_h(px(300.))
                                    .overflow_y_scroll()
                                    .p_1()
                                    .children(rows),
                            ))),
                    ),
            ),
        )
        .with_priority(1)
    });
    div()
        .relative()
        .child(button)
        .children(menu)
        .into_any_element()
}

/// The accent row: the theme's own accent, then Zeron's seven presets.
fn accent_swatches(prefs: &Prefs, cx: &App) -> impl IntoElement {
    let p = *Palette::of(cx);
    let light = p.light;
    let theme_accent = Palette::from_seed(if light {
        themes::find_for(&prefs.light_theme, true)
    } else {
        themes::find_for(&prefs.theme, false)
    })
    .accent;
    let choices = std::iter::once(("", "Theme default", theme_accent)).chain(
        themes::ACCENTS
            .iter()
            .map(|(id, name, d, l)| (*id, *name, design::hex(if light { *l } else { *d }))),
    );
    h_flex()
        .gap(px(8.))
        .children(choices.map(|(id, name, color)| {
            let on = prefs.accent == id;
            let id_owned = id.to_string();
            div()
                .id(SharedString::from(format!(
                    "accent-{}",
                    if id.is_empty() { "theme" } else { id }
                )))
                .role(Role::Button)
                .aria_label(name)
                .size(px(30.))
                .flex_none()
                .rounded_full()
                .p(px(3.))
                .border_2()
                .border_color(if on {
                    p.text.opacity(0.85)
                } else {
                    gpui_kit::transparent_black()
                })
                .cursor_pointer()
                .hover(|d| d.border_color(p.text.opacity(0.35)))
                .on_click(move |_, _, cx| {
                    let id = id_owned.clone();
                    design::update_appearance(cx, |p| p.accent = id);
                })
                .child(div().size_full().rounded_full().bg(color))
        }))
}

pub fn render(menu: Option<&'static str>, cx: &mut Context<SettingsView>) -> impl IntoElement {
    let prefs = Prefs::of(cx).clone();
    let dark_now = design::scheme_is_dark(cx);
    let dark_palette = Palette::from_seed(themes::find_for(&prefs.theme, false));
    let light_palette = Palette::from_seed(themes::find_for(&prefs.light_theme, true));

    let scheme = |id: &'static str, value: &'static str, icon, label, body| {
        let on = prefs.scheme == value;
        option_card(id, icon, label, on, body, cx)
            .on_click(move |_, _, cx| design::update_appearance(cx, |p| p.scheme = value.into()))
    };
    let system_body = h_flex()
        .size_full()
        .child(
            div()
                .w_1_2()
                .h_full()
                .overflow_hidden()
                .child(preview(light_palette)),
        )
        .child(
            div()
                .w_1_2()
                .h_full()
                .overflow_hidden()
                .child(preview(dark_palette)),
        );
    let cards = h_flex()
        .w_full()
        .items_start()
        .gap(px(16.))
        .child(scheme(
            "scheme-system",
            "system",
            IconName::Settings2,
            "System",
            system_body,
        ))
        .child(scheme(
            "scheme-light",
            "light",
            IconName::Sun,
            "Light",
            preview(light_palette),
        ))
        .child(scheme(
            "scheme-dark",
            "dark",
            IconName::Moon,
            "Dark",
            preview(dark_palette),
        ));

    let themes_of = |light: bool| {
        themes::THEMES
            .iter()
            .filter(|t| t.is_light() == light)
            .map(|t| (t.id.to_string(), t.name.to_string()))
            .collect::<Vec<_>>()
    };
    let light_select = select(
        "light-theme",
        themes::find_for(&prefs.light_theme, true).id.to_string(),
        themes_of(true),
        menu == Some("light-theme"),
        design::set_theme,
        cx,
    );
    let dark_select = select(
        "dark-theme",
        themes::find_for(&prefs.theme, false).id.to_string(),
        themes_of(false),
        menu == Some("dark-theme"),
        design::set_theme,
        cx,
    );
    let color_rows = rows_card(
        [
            setting_row(
                "Light theme",
                Some("Used when the scheme is light.".into()),
                light_select,
                cx,
            ),
            setting_row(
                "Dark theme",
                Some("Used when the scheme is dark.".into()),
                dark_select,
                cx,
            ),
            setting_row(
                "Accent color",
                Some("Links, selection and the one loud button.".into()),
                accent_swatches(&prefs, cx),
                cx,
            ),
        ],
        cx,
    );

    let wallpaper_value = match &prefs.wallpaper {
        Wallpaper::None => "none".to_string(),
        Wallpaper::Aurora => "auto".to_string(),
        Wallpaper::Scene { name } => name.clone(),
        Wallpaper::Image { .. } => "image".to_string(),
        Wallpaper::Folder { .. } => "folder".to_string(),
    };
    let mut wallpaper_options = vec![("auto".to_string(), "Painted sky".to_string())];
    wallpaper_options.extend(
        paint::SCENES
            .iter()
            .map(|(id, name)| (id.to_string(), name.to_string())),
    );
    wallpaper_options.extend([
        ("image".to_string(), "Picture…".to_string()),
        ("folder".to_string(), "Folder…".to_string()),
        ("none".to_string(), "None".to_string()),
    ]);
    let wallpaper_detail = match &prefs.wallpaper {
        Wallpaper::Image { path } | Wallpaper::Folder { path } => path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        Wallpaper::Scene { name } if name != "auto" => {
            "Painted in the app, sized to the window.".into()
        }
        Wallpaper::None => "A plain panel with a soft accent glow.".into(),
        _ => "Sky in light mode, Dusk in dark mode.".into(),
    };
    let wallpaper_select = select(
        "wallpaper",
        wallpaper_value,
        wallpaper_options,
        menu == Some("wallpaper"),
        |v, cx| match v {
            "none" => design::set_wallpaper(Wallpaper::None, cx),
            "image" => pick_picture(false, cx),
            "folder" => pick_picture(true, cx),
            name => design::set_wallpaper(Wallpaper::Scene { name: name.into() }, cx),
        },
        cx,
    );
    let effect_select = select(
        "wallpaper-effect",
        prefs.wallpaper_effect.clone(),
        paint::EFFECTS
            .iter()
            .map(|(id, name)| (id.to_string(), name.to_string()))
            .collect(),
        menu == Some("wallpaper-effect"),
        |v, cx| {
            let v = v.to_string();
            crate::design::prefs::Prefs::update(cx, |p| p.wallpaper_effect = v);
            design::wallpaper::reload(cx);
        },
        cx,
    );
    // shalldie's vscode-background settings: where, how strong, how it
    // fits, how often a folder moves on.
    let area_select = select(
        "wallpaper-area",
        prefs.wallpaper_area.clone(),
        vec![
            ("window".into(), "Whole window".into()),
            ("conversation".into(), "Conversation".into()),
            ("sidebar".into(), "Sidebar".into()),
            ("pane".into(), "Right pane".into()),
        ],
        menu == Some("wallpaper-area"),
        |v, cx| {
            let v = v.to_string();
            crate::design::prefs::Prefs::update(cx, |p| p.wallpaper_area = v);
        },
        cx,
    );
    let opacity_select = select(
        "wallpaper-opacity",
        format!("{}", (prefs.wallpaper_opacity * 100.).round() as u32),
        [5, 10, 15, 20, 25, 30, 40, 50, 60]
            .into_iter()
            .map(|n| (n.to_string(), format!("{n} %")))
            .collect(),
        menu == Some("wallpaper-opacity"),
        |v, cx| {
            let n = v.parse::<f32>().unwrap_or(20.) / 100.;
            crate::design::prefs::Prefs::update(cx, |p| p.wallpaper_opacity = n);
        },
        cx,
    );
    let fit_select = select(
        "wallpaper-fit",
        prefs.wallpaper_fit.clone(),
        vec![
            ("cover".into(), "Fill".into()),
            ("contain".into(), "Fit".into()),
        ],
        menu == Some("wallpaper-fit"),
        |v, cx| {
            let v = v.to_string();
            crate::design::prefs::Prefs::update(cx, |p| p.wallpaper_fit = v);
            design::wallpaper::reload(cx);
        },
        cx,
    );
    let interval_select = select(
        "wallpaper-interval",
        prefs.wallpaper_interval.to_string(),
        vec![
            ("0".into(), "New chat".into()),
            ("1".into(), "1 min".into()),
            ("5".into(), "5 min".into()),
            ("15".into(), "15 min".into()),
            ("60".into(), "1 hour".into()),
        ],
        menu == Some("wallpaper-interval"),
        |v, cx| {
            let n = v.parse().unwrap_or(0);
            crate::design::prefs::Prefs::update(cx, |p| p.wallpaper_interval = n);
            design::wallpaper::reload(cx);
        },
        cx,
    );
    let folder = matches!(prefs.wallpaper, Wallpaper::Folder { .. });
    let translucent = Switch::new("translucent-window")
        .checked(prefs.translucent)
        .accessibility_label("Translucent window")
        .on_click(|on: &bool, _, cx| design::set_translucent(*on, cx));
    let material_rows = rows_card(
        [
            setting_row(
                "Wallpaper",
                Some(wallpaper_detail.into()),
                wallpaper_select,
                cx,
            ),
            setting_row(
                "Effect",
                Some("Redraws the wallpaper as dots, print or a screen.".into()),
                effect_select,
                cx,
            ),
            setting_row(
                "Show on",
                Some("The part of the window the wallpaper lies over.".into()),
                area_select,
                cx,
            ),
            setting_row(
                "Opacity",
                Some("Lower keeps text sharper.".into()),
                opacity_select,
                cx,
            ),
            setting_row(
                "Size",
                Some("Fill crops the picture to the area, Fit shows all of it.".into()),
                fit_select,
                cx,
            ),
        ]
        .into_iter()
        .chain(folder.then(|| {
            setting_row(
                "Next picture",
                Some("When the folder moves on to another picture.".into()),
                interval_select,
                cx,
            )
        }))
        .chain([setting_row(
            "Translucent window",
            Some("The desktop shows through the sidebar, blurred by macOS.".into()),
            translucent,
            cx,
        )]),
        cx,
    );

    let motion_select = select(
        "reduce-motion",
        prefs.reduce_motion.clone(),
        vec![
            ("system".into(), "System".into()),
            ("on".into(), "On".into()),
            ("off".into(), "Off".into()),
        ],
        menu == Some("reduce-motion"),
        |v, cx| {
            let v = v.to_string();
            design::update_appearance(cx, |p| p.reduce_motion = v);
        },
        cx,
    );
    let motion_rows = rows_card(
        [setting_row(
            "Reduce motion",
            Some("On holds every animation still. System lets the battery saver pause motion when idle.".into()),
            motion_select,
            cx,
        )],
        cx,
    );

    let size_now = if prefs.ui_font_size > 0. {
        prefs.ui_font_size
    } else {
        Style::of(cx).font_size
    };
    let size_select = select(
        "ui-size",
        format!("{}", size_now as u32),
        [12, 13, 14, 15, 16, 17, 18, 20]
            .into_iter()
            .map(|n| (n.to_string(), format!("{n} px")))
            .collect(),
        menu == Some("ui-size"),
        |v, cx| {
            let n: f32 = v.parse().unwrap_or(0.);
            design::update_appearance(cx, |p| p.ui_font_size = n);
        },
        cx,
    );
    let width_select = select(
        "width",
        format!("{}", prefs.conversation_width as u32),
        vec![
            ("680".into(), "Narrow".into()),
            ("800".into(), "Default".into()),
            ("920".into(), "Wide".into()),
            ("1080".into(), "Widest".into()),
        ],
        menu == Some("width"),
        |v, cx| {
            let n: f32 = v.parse().unwrap_or(800.);
            design::update_appearance(cx, |p| p.conversation_width = n);
        },
        cx,
    );
    let layout_rows = rows_card(
        [
            setting_row(
                "Interface size",
                Some("Menus and conversations.".into()),
                size_select,
                cx,
            ),
            setting_row(
                "Conversation width",
                Some("How wide the reading column runs.".into()),
                width_select,
                cx,
            ),
        ],
        cx,
    );

    v_flex()
        .child(group_header(
            "Color scheme",
            Some(if dark_now {
                "Showing the dark theme."
            } else {
                "Showing the light theme."
            }),
            cx,
        ))
        .child(cards)
        .child(div().h(px(16.)))
        .child(color_rows)
        .child(group_header("Wallpaper and window", None, cx))
        .child(material_rows)
        .child(group_header("Motion", None, cx))
        .child(motion_rows)
        .child(group_header("Fonts and layout", None, cx))
        .child(layout_rows)
}

/// Ask for a picture (or a folder of them) and use it as the wallpaper.
fn pick_picture(folder: bool, cx: &mut App) {
    let picked = cx.prompt_for_paths(PathPromptOptions {
        files: !folder,
        directories: folder,
        multiple: false,
        prompt: Some(
            if folder {
                "Use this folder"
            } else {
                "Use as wallpaper"
            }
            .into(),
        ),
    });
    cx.spawn(async move |cx| {
        if let Ok(Ok(Some(mut paths))) = picked.await
            && let Some(path) = paths.pop()
        {
            let choice = if folder {
                Wallpaper::Folder { path }
            } else {
                Wallpaper::Image { path }
            };
            cx.update(|cx| design::set_wallpaper(choice, cx));
        }
    })
    .detach();
}
