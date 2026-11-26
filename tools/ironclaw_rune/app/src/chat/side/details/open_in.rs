//! "Open in" for the thread details card (t3code's `OpenInPicker`): the
//! editors installed on this computer, plus the file manager and a
//! terminal, and opening the thread's folder in one: `open -a` on macOS,
//! the editor's own command on Linux.

use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::sync::OnceLock;

/// An app that can open a folder: what the menu shows, the app's name for
/// `open -a` (also the saved preference, `Prefs::open_in_editor`), and its
/// command on Linux (`None`: macOS only).
pub struct Editor {
    pub label: &'static str,
    pub app: &'static str,
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    cli: Option<&'static str>,
}

const fn editor(label: &'static str, app: &'static str, cli: Option<&'static str>) -> Editor {
    Editor { label, app, cli }
}

/// t3code's editor order, as macOS app names.
const KNOWN: [Editor; 9] = [
    editor("Cursor", "Cursor", Some("cursor")),
    editor("VS Code", "Visual Studio Code", Some("code")),
    editor("Zed", "Zed", Some("zed")),
    editor("Windsurf", "Windsurf", Some("windsurf")),
    editor("Sublime Text", "Sublime Text", Some("subl")),
    editor("Xcode", "Xcode", None),
    editor("Ghostty", "Ghostty", Some("ghostty")),
    // On Linux: the first terminal emulator found (`crate::terminal`).
    editor("Terminal", "Terminal", Some("")),
    // On Linux: the desktop's file manager, through `xdg-open`.
    editor(FILES_LABEL, "Finder", Some("")),
];

const FILES_LABEL: &str = if cfg!(target_os = "macos") {
    "Finder"
} else {
    "Files"
};

#[cfg(target_os = "macos")]
fn installed(e: &Editor) -> bool {
    let app = e.app;
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

#[cfg(not(target_os = "macos"))]
fn installed(e: &Editor) -> bool {
    match e.cli {
        None => false,
        Some("") => true,
        Some(cli) => crate::system::find_on_path(cli, &crate::system::search_path()).is_some(),
    }
}

/// The installed apps, looked up once.
pub fn editors() -> &'static [&'static Editor] {
    static FOUND: OnceLock<Vec<&'static Editor>> = OnceLock::new();
    FOUND.get_or_init(|| KNOWN.iter().filter(|e| installed(e)).collect())
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
#[cfg(target_os = "macos")]
pub fn open(editor: &Editor, folder: &Path) {
    let mut cmd = std::process::Command::new("/usr/bin/open");
    if editor.app != "Finder" {
        cmd.args(["-a", editor.app]);
    }
    let app = editor.app;
    let folder = folder.to_path_buf();
    // `open` returns at once; waiting on it on a thread reaps the child.
    std::thread::spawn(move || {
        if let Err(e) = cmd.arg(folder).status() {
            tracing::warn!(error = %e, app, "could not open the folder");
        }
    });
}

/// Open `folder` in `editor`: the editor's command with the folder, a
/// terminal started in it, or the file manager on it.
#[cfg(not(target_os = "macos"))]
pub fn open(editor: &Editor, folder: &Path) {
    let app = editor.app;
    let opened = match (app, editor.cli) {
        ("Finder", _) => crate::system::open(folder),
        ("Terminal", _) => crate::terminal::open_in(folder),
        ("Ghostty", Some(cli)) => spawn_reaped(
            std::process::Command::new(cli)
                .arg(format!("--working-directory={}", folder.display())),
        ),
        (_, Some(cli)) => spawn_reaped(std::process::Command::new(cli).arg(folder)),
        (_, None) => Ok(()),
    };
    if let Err(e) = opened {
        tracing::warn!(error = %e, app, "could not open the folder");
    }
}

#[cfg(not(target_os = "macos"))]
fn spawn_reaped(cmd: &mut std::process::Command) -> std::io::Result<()> {
    let mut child = cmd
        .env("PATH", crate::login_path::path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}
