//! Appearance preferences, saved as a small JSON file next to the app's
//! other local state. Client-only: the gateway never sees them.

use std::path::PathBuf;

use gpui_kit::{App, AppContext as _, Global};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Wallpaper {
    None,
    /// The old generated colour wash; read as the default scene.
    Aurora,
    /// A landscape painted in code (`wallpaper::SCENES`), "auto" picks one
    /// by the color scheme.
    Scene {
        name: String,
    },
    /// A picture the user picked.
    Image {
        path: PathBuf,
    },
    /// A random picture from this folder for every new chat.
    Folder {
        path: PathBuf,
    },
}

/// Preference file layout version; see [`Prefs::load`].
const VERSION: u32 = 3;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// 0 when the file predates versioning (field-level default).
    #[serde(default)]
    pub version: u32,
    /// The dark theme (the field predates light/dark pairs).
    pub theme: String,
    pub light_theme: String,
    /// "system", "light" or "dark", as Zeron's color scheme cards.
    pub scheme: String,
    /// A Zeron accent preset id, or empty for the theme's own.
    pub accent: String,
    /// "system" (the battery saver decides), "on" or "off".
    pub reduce_motion: String,
    /// Interface text size in px; 0 keeps the style's own.
    pub ui_font_size: f32,
    /// Reading column width in px.
    pub conversation_width: f32,
    pub wallpaper: Wallpaper,
    /// How the wallpaper is redrawn: "none", "dither", "halftone" or
    /// "scanlines" (Zeron's background effects).
    pub wallpaper_effect: String,
    /// A faint copy of the wallpaper behind open threads too.
    pub wallpaper_in_threads: bool,
    /// macOS blurs the desktop through the sidebar and panels.
    pub translucent: bool,
    pub status_bar_hidden: bool,
    /// The model picker's last picks, newest first: (provider, model).
    pub recent_models: Vec<(String, String)>,
    /// The model picker's provider order (provider ids); unlisted ones follow.
    pub provider_order: Vec<String>,
    /// Sidebar width in px, dragged at its edge; 0 keeps the default.
    pub sidebar_width: f32,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            version: VERSION,
            theme: super::themes::DEFAULT_THEME.into(),
            light_theme: super::themes::DEFAULT_LIGHT_THEME.into(),
            scheme: "dark".into(),
            accent: String::new(),
            reduce_motion: "system".into(),
            ui_font_size: 0.,
            conversation_width: 720.,
            wallpaper: Wallpaper::Scene {
                name: "auto".into(),
            },
            wallpaper_effect: "dither".into(),
            wallpaper_in_threads: false,
            translucent: false,
            status_bar_hidden: false,
            recent_models: Vec::new(),
            provider_order: Vec::new(),
            sidebar_width: 0.,
        }
    }
}

impl Global for Prefs {}

impl Prefs {
    fn file() -> Option<PathBuf> {
        Some(
            dirs::config_dir()?
                .join("IronClaw Desktop")
                .join("prefs.json"),
        )
    }

    /// Read the saved file. A file from before the appearance redesign
    /// (version 1, four styles and fifty themes) keeps its layout choices
    /// and moves to the new look: IronClaw's themes and the painted scene.
    pub fn load() -> Self {
        let mut prefs: Self = Self::file()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if prefs.version < VERSION {
            let fresh = Self::default();
            prefs.theme = fresh.theme;
            prefs.light_theme = fresh.light_theme;
            prefs.accent = fresh.accent;
            if !matches!(prefs.wallpaper, Wallpaper::Image { .. }) {
                prefs.wallpaper = fresh.wallpaper;
            }
            // Behind threads the wallpaper read as smudges; new chat only.
            prefs.wallpaper_in_threads = false;
            prefs.version = VERSION;
        }
        prefs
    }

    pub fn of(cx: &App) -> &Self {
        cx.global::<Self>()
    }

    /// Edit, save to disk off the UI thread, and redraw.
    pub fn update(cx: &mut App, edit: impl FnOnce(&mut Self)) {
        let mut prefs = cx.global::<Self>().clone();
        edit(&mut prefs);
        let snapshot = prefs.clone();
        cx.set_global(prefs);
        cx.background_spawn(async move {
            let Some(file) = Self::file() else { return };
            let saved = file
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| {
                    let json =
                        serde_json::to_vec_pretty(&snapshot).map_err(std::io::Error::other)?;
                    std::fs::write(&file, json)
                });
            if let Err(e) = saved {
                tracing::warn!("could not save preferences: {e}");
            }
        })
        .detach();
        cx.refresh_windows();
    }
}
