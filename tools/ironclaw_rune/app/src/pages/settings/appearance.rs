//! Settings → Appearance, laid out after Zeron's page
//! (`crates/ui/src/settings/appearance.rs`, MIT), one decision per row:
//! the color scheme, a theme for each side of it, an accent, then the
//! new-chat wallpaper and its effect, motion and layout. All client-side
//! preferences (`design::prefs`), applied live through `State::update_prefs`.
//! Port of GPUI `pages/settings/appearance.rs`.

use rune::{Color, Cursor, El, FileDialogOptions, Tx, col, el, row};
use rune_kit::{Kit, Select, SelectAppearance};

use crate::design::look;
use crate::design::prefs::Wallpaper;
use crate::design::{Palette, rgb, themes};
use crate::icons;
use crate::pages::common::{glyph, group_header, label, rows_card, setting_row, switch};
use crate::state::State;

const CARD_HEIGHT: f32 = 148.;
const CARD_RADIUS: f32 = 6.;

/// A small painting of the app in palette `p`: sidebar, a selected row, a
/// message, some answer lines and the composer pill.
fn preview(p: Palette) -> El {
    let line = |w: f32, c: Color| el().h(4.).w(w).radius(2.).fill(c);
    row()
        .w_full()
        .h_full()
        .fill(p.surface)
        .child(
            col()
                .w(54.)
                .h_full()
                .padding(7.)
                .gap(6.)
                .child(
                    row().gap(3.).pb(2.).children(
                        [0xff5f57u32, 0xfebc2e, 0x28c840]
                            .map(|c| el().size(5., 5.).radius(2.5).fill(rgb(c))),
                    ),
                )
                .child(line(30., p.text_faint))
                .child(
                    row()
                        .padding(3.)
                        .radius(4.)
                        .fill(p.active)
                        .child(line(28., p.text)),
                )
                .child(line(24., p.text_faint))
                .child(line(32., p.text_faint)),
        )
        .child(
            col()
                .grow(1.0)
                .min_w(0.)
                .h_full()
                .my(5.)
                .mr(5.)
                .radius(4.)
                .fill(p.bg)
                .padding(8.)
                .gap(6.)
                .child(
                    row().w_full().justify_end().child(
                        row()
                            .px(6.)
                            .py(4.)
                            .radius(6.)
                            .fill(p.text.alpha(0.08))
                            .child(line(40., p.text_muted)),
                    ),
                )
                .child(line(90., p.text_muted))
                .child(line(70., p.text_muted))
                .child(line(46., p.accent))
                .child(el().grow(1.0))
                .child(
                    row()
                        .h(18.)
                        .px(6.)
                        .center_y()
                        .radius(8.)
                        .border(1., p.border_strong)
                        .child(line(36., p.text_faint))
                        .child(el().grow(1.0))
                        .child(el().size(10., 10.).radius(5.).fill(p.text)),
                ),
        )
}

/// Zeron's option card: the preview is the control, a quiet accent edge
/// marks the choice and the caption sits underneath.
fn option_card(
    p: &Palette,
    id: &'static str,
    icon: &'static rune::Icon,
    caption: &'static str,
    selected: bool,
    body: El,
    on_press: impl Fn(&mut State) + 'static,
) -> El {
    let ink = if selected { p.accent } else { p.text_muted };
    col()
        .key(id)
        .role("radio")
        .checked(selected)
        .label(caption)
        .focusable(true)
        .grow(1.0)
        .min_w(0.)
        .items_center()
        .gap(8.)
        .cursor(Cursor::Pointer)
        .on_press(on_press)
        .child(
            col()
                .key("card")
                .h(CARD_HEIGHT)
                .w_full()
                .radius(CARD_RADIUS)
                .clip_rounded(CARD_RADIUS)
                .border(1., if selected { p.accent } else { p.border })
                .child(body),
        )
        .child(
            row()
                .gap(6.)
                .center_y()
                .child(glyph(icon, 16., ink))
                .child(label(caption, 13., if selected { 500. } else { 400. }, ink)),
        )
}

/// Zeron's select: a button naming the choice, opening a menu of the
/// options under it. `id` is this select's slot in `appearance_menu`.
/// `options` are (value, label); `pick` gets the value.
fn select(
    s: &State,
    k: &Kit,
    layer: &mut Vec<El>,
    id: &'static str,
    current: &str,
    options: Vec<(String, String)>,
    pick: fn(&mut State, &str, &mut Tx),
) -> El {
    let labels: Vec<String> = options.iter().map(|(_, l)| l.clone()).collect();
    let current_label = options
        .iter()
        .find(|(v, _)| v == current)
        .map(|(_, l)| l.clone())
        .unwrap_or_else(|| current.to_string());
    let open = s.pages.settings.appearance_menu == Some(id);
    Select::new(format!("select-{id}"), &labels)
        .width(180.)
        .appearance(SelectAppearance::Field)
        .current(Some(&current_label))
        .label(id)
        .open(open)
        .on_toggle(move |s: &mut State| {
            let menu = &mut s.pages.settings.appearance_menu;
            *menu = if *menu == Some(id) { None } else { Some(id) };
        })
        .on_close(|s: &mut State| s.pages.settings.appearance_menu = None)
        .on_choose(move |s: &mut State, chosen: &str, tx: &mut Tx| {
            s.pages.settings.appearance_menu = None;
            if let Some((value, _)) = options.iter().find(|(_, l)| l == chosen) {
                pick(s, value, tx);
            }
        })
        .build(k, layer)
}

/// Switch preset: the light or dark slot, by the theme's own brightness.
fn set_theme(s: &mut State, id: &str, _: &mut Tx) {
    let id = id.to_string();
    let light = themes::find(&id).is_light();
    s.update_prefs(|p| {
        if light {
            p.light_theme = id;
        } else {
            p.theme = id;
        }
    });
    look::reload_wallpaper(s);
}

fn set_wallpaper(s: &mut State, choice: Wallpaper) {
    s.update_prefs(|p| p.wallpaper = choice);
    look::reload_wallpaper(s);
}

/// The accent row: the theme's own accent, then Zeron's seven presets.
fn accent_swatches(s: &State) -> El {
    let p = &s.palette;
    let prefs = &s.prefs;
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
            .map(|(id, name, d, l)| (*id, *name, rgb(if light { *l } else { *d }))),
    );
    row()
        .key("accents")
        .role("radiogroup")
        .label("Accent color")
        .gap(8.)
        .children(choices.map(|(id, name, color)| {
            let on = prefs.accent == id;
            let id_owned = id.to_string();
            el().key(format!(
                "accent-{}",
                if id.is_empty() { "theme" } else { id }
            ))
            .role("radio")
            .checked(on)
            .label(name)
            .focusable(true)
            .size(30., 30.)
            .radius(15.)
            .padding(3.)
            .border(
                2.,
                if on {
                    p.text.alpha(0.85)
                } else {
                    Color::TRANSPARENT
                },
            )
            .hover(p.text.alpha(0.06))
            .help(name)
            .cursor(Cursor::Pointer)
            .on_press(move |s: &mut State| {
                let id = id_owned.clone();
                s.update_prefs(|p| p.accent = id);
            })
            .child(el().w_full().h_full().radius(12.).fill(color))
        }))
}

/// Ask for a picture (or a folder of them) and use it as the wallpaper.
fn pick_picture(folder: bool, tx: &mut Tx) {
    let options = FileDialogOptions {
        title: if folder {
            "Use this folder"
        } else {
            "Use as wallpaper"
        }
        .into(),
        ..Default::default()
    };
    let done = move |s: &mut State, result: rune::FileDialogResult, _: &mut Tx| match result {
        Ok(Some(path)) => set_wallpaper(
            s,
            if folder {
                Wallpaper::Folder { path }
            } else {
                Wallpaper::Image { path }
            },
        ),
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "wallpaper picker failed"),
    };
    if folder {
        tx.open_folder(options, done);
    } else {
        tx.open_file(
            options.filter("Pictures", &["png", "jpg", "jpeg", "webp", "heic", "gif"]),
            done,
        );
    }
}

fn opts(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(v, l)| (v.to_string(), l.to_string()))
        .collect()
}

pub fn view(s: &State, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let prefs = &s.prefs;
    let dark_now = !p.light;
    let dark_palette = Palette::from_seed(themes::find_for(&prefs.theme, false));
    let light_palette = Palette::from_seed(themes::find_for(&prefs.light_theme, true));

    let scheme = |id: &'static str,
                  value: &'static str,
                  icon: &'static rune::Icon,
                  caption: &'static str,
                  body: El| {
        option_card(
            p,
            id,
            icon,
            caption,
            prefs.scheme == value,
            body,
            move |s: &mut State| s.update_prefs(|p| p.scheme = value.into()),
        )
    };
    let system_body = row()
        .w_full()
        .h_full()
        .child(
            col()
                .w_frac(0.5)
                .h_full()
                .clip_rounded(0.)
                .child(preview(light_palette)),
        )
        .child(
            col()
                .w_frac(0.5)
                .h_full()
                .clip_rounded(0.)
                .child(preview(dark_palette)),
        );
    let cards = row()
        .key("scheme-cards")
        .role("radiogroup")
        .label("Color scheme")
        .w_full()
        .items_start()
        .gap(16.)
        .child(scheme(
            "scheme-system",
            "system",
            &icons::SETTINGS_2,
            "System",
            system_body,
        ))
        .child(scheme(
            "scheme-light",
            "light",
            &icons::SUN,
            "Light",
            preview(light_palette),
        ))
        .child(scheme(
            "scheme-dark",
            "dark",
            &icons::MOON,
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
        s,
        k,
        layer,
        "light-theme",
        themes::find_for(&prefs.light_theme, true).id,
        themes_of(true),
        set_theme,
    );
    let dark_select = select(
        s,
        k,
        layer,
        "dark-theme",
        themes::find_for(&prefs.theme, false).id,
        themes_of(false),
        set_theme,
    );
    let color_rows = rows_card(
        p,
        [
            setting_row(
                p,
                "Light theme",
                Some("Used when the scheme is light."),
                light_select,
            ),
            setting_row(
                p,
                "Dark theme",
                Some("Used when the scheme is dark."),
                dark_select,
            ),
            setting_row(
                p,
                "Accent color",
                Some("Links, selection and the one loud button."),
                accent_swatches(s),
            ),
        ],
    );

    let wallpaper_value = match &prefs.wallpaper {
        Wallpaper::None => "none".to_string(),
        Wallpaper::Aurora => "auto".to_string(),
        Wallpaper::Scene { name } => name.clone(),
        Wallpaper::Image { .. } => "image".to_string(),
        Wallpaper::Folder { .. } => "folder".to_string(),
    };
    let mut wallpaper_options = vec![("auto".to_string(), "Painted sky".to_string())];
    wallpaper_options.extend(opts(&look::SCENES));
    wallpaper_options.extend(opts(&[
        ("image", "Picture…"),
        ("folder", "Folder…"),
        ("none", "None"),
    ]));
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
        s,
        k,
        layer,
        "wallpaper",
        &wallpaper_value,
        wallpaper_options,
        |s, v, tx| match v {
            "none" => set_wallpaper(s, Wallpaper::None),
            "image" => pick_picture(false, tx),
            "folder" => pick_picture(true, tx),
            name => set_wallpaper(s, Wallpaper::Scene { name: name.into() }),
        },
    );
    let effect_select = select(
        s,
        k,
        layer,
        "wallpaper-effect",
        &prefs.wallpaper_effect,
        opts(&look::EFFECTS),
        |s, v, _| {
            let v = v.to_string();
            s.update_prefs(|p| p.wallpaper_effect = v);
            look::reload_wallpaper(s);
        },
    );
    // shalldie's vscode-background settings: where, how strong, how it
    // fits, how often a folder moves on.
    let area_select = select(
        s,
        k,
        layer,
        "wallpaper-area",
        &prefs.wallpaper_area,
        opts(&[
            ("window", "Whole window"),
            ("conversation", "Conversation"),
            ("sidebar", "Sidebar"),
            ("pane", "Right pane"),
        ]),
        |s, v, _| {
            let v = v.to_string();
            s.update_prefs(|p| p.wallpaper_area = v);
        },
    );
    let opacity_select = select(
        s,
        k,
        layer,
        "wallpaper-opacity",
        &format!("{}", (prefs.wallpaper_opacity * 100.).round() as u32),
        [5, 10, 15, 20, 25, 30, 40, 50, 60]
            .into_iter()
            .map(|n| (n.to_string(), format!("{n} %")))
            .collect(),
        |s, v, _| {
            let n = v.parse::<f32>().unwrap_or(20.) / 100.;
            s.update_prefs(|p| p.wallpaper_opacity = n);
        },
    );
    let fit_select = select(
        s,
        k,
        layer,
        "wallpaper-fit",
        &prefs.wallpaper_fit,
        opts(&[("cover", "Fill"), ("contain", "Fit")]),
        |s, v, _| {
            let v = v.to_string();
            s.update_prefs(|p| p.wallpaper_fit = v);
            look::reload_wallpaper(s);
        },
    );
    let folder = matches!(prefs.wallpaper, Wallpaper::Folder { .. });
    let interval_select = folder.then(|| {
        select(
            s,
            k,
            layer,
            "wallpaper-interval",
            &prefs.wallpaper_interval.to_string(),
            opts(&[
                ("0", "New chat"),
                ("1", "1 min"),
                ("5", "5 min"),
                ("15", "15 min"),
                ("60", "1 hour"),
            ]),
            |s, v, _| {
                let n = v.parse().unwrap_or(0);
                s.update_prefs(|p| p.wallpaper_interval = n);
                look::reload_wallpaper(s);
            },
        )
    });
    let translucent_on = prefs.translucent;
    let translucent = switch(
        k,
        prefs.translucent,
        false,
        "Translucent window",
        move |s: &mut State| look::set_translucent(s, !translucent_on),
    );
    let material_rows = rows_card(
        p,
        [
            setting_row(p, "Wallpaper", Some(&wallpaper_detail), wallpaper_select),
            setting_row(
                p,
                "Effect",
                Some("Redraws the wallpaper as dots, print or a screen."),
                effect_select,
            ),
            setting_row(
                p,
                "Show on",
                Some("The part of the window the wallpaper lies over."),
                area_select,
            ),
            setting_row(
                p,
                "Opacity",
                Some("Lower keeps text sharper."),
                opacity_select,
            ),
            setting_row(
                p,
                "Size",
                Some("Fill crops the picture to the area, Fit shows all of it."),
                fit_select,
            ),
        ]
        .into_iter()
        .chain(interval_select.map(|select| {
            setting_row(
                p,
                "Next picture",
                Some("When the folder moves on to another picture."),
                select,
            )
        }))
        .chain([setting_row(
            p,
            "Translucent window",
            Some("The desktop shows through the sidebar, blurred by macOS."),
            translucent,
        )]),
    );

    let motion_select = select(
        s,
        k,
        layer,
        "reduce-motion",
        &prefs.reduce_motion,
        opts(&[("system", "System"), ("on", "On"), ("off", "Off")]),
        |s, v, _| {
            let v = v.to_string();
            s.update_prefs(|p| p.reduce_motion = v);
        },
    );
    let motion_rows = rows_card(
        p,
        [setting_row(
            p,
            "Reduce motion",
            Some(
                "On holds every animation still. System lets the battery saver pause motion when idle.",
            ),
            motion_select,
        )],
    );

    let size_now = if prefs.ui_font_size > 0. {
        prefs.ui_font_size
    } else {
        look::style_font_size(s)
    };
    let size_select = select(
        s,
        k,
        layer,
        "ui-size",
        &format!("{}", size_now as u32),
        [12, 13, 14, 15, 16, 17, 18, 20]
            .into_iter()
            .map(|n| (n.to_string(), format!("{n} px")))
            .collect(),
        |s, v, _| {
            let n: f32 = v.parse().unwrap_or(0.);
            s.update_prefs(|p| p.ui_font_size = n);
        },
    );
    let width_select = select(
        s,
        k,
        layer,
        "width",
        &format!("{}", prefs.conversation_width as u32),
        opts(&[
            ("680", "Narrow"),
            ("800", "Default"),
            ("920", "Wide"),
            ("1080", "Widest"),
        ]),
        |s, v, _| {
            let n: f32 = v.parse().unwrap_or(800.);
            s.update_prefs(|p| p.conversation_width = n);
        },
    );
    let layout_rows = rows_card(
        p,
        [
            setting_row(
                p,
                "Interface size",
                Some("Menus and conversations."),
                size_select,
            ),
            setting_row(
                p,
                "Conversation width",
                Some("How wide the reading column runs."),
                width_select,
            ),
        ],
    );

    col()
        .w_full()
        .child(group_header(
            p,
            "Color scheme",
            Some(if dark_now {
                "Showing the dark theme."
            } else {
                "Showing the light theme."
            }),
        ))
        .child(cards)
        .child(el().h(16.))
        .child(color_rows)
        .child(group_header(p, "Wallpaper and window", None))
        .child(material_rows)
        .child(group_header(p, "Motion", None))
        .child(motion_rows)
        .child(group_header(p, "Fonts and layout", None))
        .child(layout_rows)
}
