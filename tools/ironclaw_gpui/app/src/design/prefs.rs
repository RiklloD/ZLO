//! Appearance preferences, saved as a small JSON file next to the app's
//! other local state. Client-only: the gateway never sees them.

use std::path::PathBuf;

use gpui_kit::{App, AppContext as _, Global};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Wallpaper {
    None,
    /// Generated in code from the theme's colors.
    Aurora,
    /// A picture the user picked, blurred and dimmed once at load.
    Image {
        path: PathBuf,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub theme: String,
    pub wallpaper: Wallpaper,
    pub status_bar_hidden: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: super::themes::DEFAULT_THEME.into(),
            wallpaper: Wallpaper::Aurora,
            status_bar_hidden: false,
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

    pub fn load() -> Self {
        Self::file()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
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
