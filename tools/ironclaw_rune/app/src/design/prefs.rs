//! Appearance preferences, saved as a small JSON file next to the app's
//! other local state. Client-only: the gateway never sees them.

use std::path::PathBuf;

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
const VERSION: u32 = 5;

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
    /// Where the wallpaper lies over the app, as in shalldie's
    /// vscode-background: "window", "conversation", "sidebar" or "pane".
    pub wallpaper_area: String,
    /// How strongly the wallpaper shows over the app, 0..1.
    pub wallpaper_opacity: f32,
    /// "cover" crops the picture to fill its area, "contain" shows all of it.
    pub wallpaper_fit: String,
    /// A folder moves on to its next picture every this many minutes;
    /// 0 waits for a new chat.
    pub wallpaper_interval: u32,
    /// macOS blurs the desktop through the sidebar and panels.
    pub translucent: bool,
    pub status_bar_hidden: bool,
    /// The model picker's last picks, newest first: (provider, model).
    pub recent_models: Vec<(String, String)>,
    /// The model picker's provider order (provider ids); unlisted ones follow.
    pub provider_order: Vec<String>,
    /// Sidebar width in px, dragged at its edge; 0 keeps the default.
    pub sidebar_width: f32,
    /// The left sidebar is folded away; the panel's top strip reopens it.
    pub sidebar_hidden: bool,
    /// The chat's right pane: shown, its width (0 keeps the default) and
    /// the tab last picked.
    pub side_open: bool,
    pub side_width: f32,
    pub side_tab: String,
    /// The work panel between the conversation and the right pane
    /// (Changes, Files, Outputs): shown, width (0 keeps the default), tab.
    pub work_open: bool,
    pub work_width: f32,
    pub work_tab: String,
    /// Side chats by parent thread, newest last. They live under their
    /// parent (session card, right pane), not in the inbox list.
    pub side_chats: std::collections::HashMap<String, Vec<String>>,
    /// The project folder each thread's coding tabs read (thread id → path),
    /// and the last one picked, offered to threads without their own.
    pub project_folders: std::collections::HashMap<String, PathBuf>,
    pub last_project: Option<PathBuf>,
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
            conversation_width: 800.,
            wallpaper: Wallpaper::Scene {
                name: "auto".into(),
            },
            wallpaper_effect: "dither".into(),
            wallpaper_area: "window".into(),
            wallpaper_opacity: 0.2,
            wallpaper_fit: "cover".into(),
            wallpaper_interval: 0,
            translucent: false,
            status_bar_hidden: false,
            recent_models: Vec::new(),
            provider_order: Vec::new(),
            sidebar_width: 0.,
            sidebar_hidden: false,
            side_open: false,
            side_width: 0.,
            side_tab: String::new(),
            work_open: false,
            work_width: 0.,
            work_tab: String::new(),
            side_chats: Default::default(),
            project_folders: Default::default(),
            last_project: None,
        }
    }
}

impl Prefs {
    /// Prefs saved before the rename to ZLO move to the new folder once.
    fn file() -> Option<PathBuf> {
        let config = dirs::config_dir()?;
        let dir = config.join("ZLO");
        for old_name in ["Numoh", "IronClaw Desktop"] {
            let old = config.join(old_name);
            if !dir.exists() && old.exists() {
                let _ = std::fs::rename(&old, &dir);
            }
        }
        Some(dir.join("prefs.json"))
    }

    /// Read the saved file. A file from before the appearance redesign
    /// (version 1, four styles and fifty themes) keeps its layout choices
    /// and moves to the new look: IronClaw's themes and the painted scene.
    pub fn load() -> Self {
        let raw: Option<serde_json::Value> = Self::file()
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|b| serde_json::from_slice(&b).ok());
        // The baseline is the file as read, so keys a migration below
        // changes (the version among them) count as changed on first save.
        if let Some(serde_json::Value::Object(keys)) = &raw {
            *baseline().lock().unwrap_or_else(|e| e.into_inner()) = Some(keys.clone());
        }
        let mut prefs: Self = raw
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        if prefs.version < 3 {
            let fresh = Self::default();
            prefs.theme = fresh.theme;
            prefs.light_theme = fresh.light_theme;
            prefs.accent = fresh.accent;
            if !matches!(prefs.wallpaper, Wallpaper::Image { .. }) {
                prefs.wallpaper = fresh.wallpaper;
            }
        }
        if prefs.version < 5 && prefs.conversation_width == 720. {
            // Version 5 widens the default reading column.
            prefs.conversation_width = Self::default().conversation_width;
        }
        prefs.version = VERSION;
        prefs
    }

    /// Save to disk off the UI thread. Callers edit `State::prefs` in place,
    /// then call this; the view re-runs on its own after the handler.
    ///
    /// The file is shared with the GPUI app, so only the keys this app
    /// changed since it last loaded or saved are written over what is on
    /// disk (see `persist`); the rest keeps whatever the other app saved.
    pub fn save(&self) {
        let Some(file) = Self::file() else { return };
        let new = match serde_json::to_value(self) {
            Ok(serde_json::Value::Object(keys)) => keys,
            Ok(_) => return,
            Err(e) => {
                tracing::warn!("could not encode preferences: {e}");
                return;
            }
        };
        let changes = {
            let mut base = baseline().lock().unwrap_or_else(|e| e.into_inner());
            let changes = changed(base.as_ref(), &new);
            *base = Some(new.clone());
            changes
        };
        if !changes.is_empty() {
            crate::persist::global().patch(file, changes, serde_json::Value::Object(new));
        }
    }
}

type Keys = serde_json::Map<String, serde_json::Value>;

/// The preferences as this app last read or wrote them.
fn baseline() -> &'static std::sync::Mutex<Option<Keys>> {
    static BASE: std::sync::OnceLock<std::sync::Mutex<Option<Keys>>> = std::sync::OnceLock::new();
    BASE.get_or_init(Default::default)
}

/// The top-level keys of `new` that differ from `base` (all without one).
fn changed(base: Option<&Keys>, new: &Keys) -> Keys {
    new.iter()
        .filter(|(k, v)| base.is_none_or(|b| b.get(*k) != Some(*v)))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_changed_keys_are_patched() {
        let mut prefs = Prefs::default();
        let serde_json::Value::Object(base) = serde_json::to_value(&prefs).unwrap() else {
            panic!()
        };
        assert_eq!(changed(None, &base).len(), base.len());
        prefs.conversation_width += 10.;
        let serde_json::Value::Object(new) = serde_json::to_value(&prefs).unwrap() else {
            panic!()
        };
        let keys: Vec<_> = changed(Some(&base), &new).keys().cloned().collect();
        assert_eq!(keys, vec!["conversation_width".to_string()]);
    }
}
