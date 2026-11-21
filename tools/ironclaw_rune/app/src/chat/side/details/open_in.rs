//! "Open in" for the thread details card (t3code's `OpenInPicker`): the
//! editors installed on this Mac, plus Finder and Terminal, and opening the
//! thread's folder in one with `open -a`.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// An app that can open a folder: what the menu shows and the app's name
/// for `open -a`.
pub struct Editor {
    pub label: &'static str,
    pub app: &'static str,
}

/// t3code's editor order, as macOS app names.
const KNOWN: [Editor; 9] = [
    Editor {
        label: "Cursor",
        app: "Cursor",
    },
    Editor {
        label: "VS Code",
        app: "Visual Studio Code",
    },
    Editor {
        label: "Zed",
        app: "Zed",
    },
    Editor {
        label: "Windsurf",
        app: "Windsurf",
    },
    Editor {
        label: "Sublime Text",
        app: "Sublime Text",
    },
    Editor {
        label: "Xcode",
        app: "Xcode",
    },
    Editor {
        label: "Ghostty",
        app: "Ghostty",
    },
    Editor {
        label: "Terminal",
        app: "Terminal",
    },
    Editor {
        label: "Finder",
        app: "Finder",
    },
];

fn installed(app: &str) -> bool {
    if matches!(app, "Finder" | "Terminal") {
        return true;
    }
    let bundle = format!("{app}.app");
    let home = std::env::var_os("HOME").map(PathBuf::from);
    [
        Some(PathBuf::from("/Applications")),
        home.map(|h| h.join("Applications")),
    ]
    .into_iter()
    .flatten()
    .any(|dir| dir.join(&bundle).exists())
}

/// The installed apps, looked up once.
pub fn editors() -> &'static [&'static Editor] {
    static FOUND: OnceLock<Vec<&'static Editor>> = OnceLock::new();
    FOUND.get_or_init(|| KNOWN.iter().filter(|e| installed(e.app)).collect())
}

/// The editor `pref` names (`Prefs::open_in_editor`), else the first one.
pub fn preferred(pref: &str) -> Option<&'static Editor> {
    let all = editors();
    all.iter()
        .find(|e| e.app == pref)
        .or_else(|| all.first())
        .copied()
}

/// Open `folder` in `editor`.
pub fn open(editor: &Editor, folder: &Path) {
    let mut cmd = std::process::Command::new("/usr/bin/open");
    if editor.app != "Finder" {
        cmd.args(["-a", editor.app]);
    }
    if let Err(e) = cmd.arg(folder).spawn() {
        tracing::warn!(error = %e, app = editor.app, "could not open the folder");
    }
}
