//! "Open at login": on macOS a per-user LaunchAgent that runs
//! `open -a <bundle>`; on Linux an XDG autostart entry
//! (`~/.config/autostart/zlo.desktop`) that runs the installed app.
//!
//! launchd and the desktop session read those folders at login, so writing
//! or removing the file is the whole switch. Only offered when the app runs
//! from its install (the `.app` bundle, or the folder `install-linux.sh`
//! makes): a bare cargo binary has no stable path worth registering.

use std::path::{Path, PathBuf};

/// The app's bundle id (see `bundle.sh`), unchanged since the first release
/// so an existing login item keeps working.
#[cfg(target_os = "macos")]
const LABEL: &str = "com.numoh.desktop";

/// The `.app` this process runs from, if any.
#[cfg(target_os = "macos")]
pub fn bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // <bundle>.app/Contents/MacOS/<exe>
    let bundle = exe.parent()?.parent()?.parent()?;
    (bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

/// The installed app this process runs as, if any: the binary in a folder
/// that also holds its `zlo-backend`, or the `zlo-light` mark of a light
/// install (what `install-linux.sh` lays out).
#[cfg(not(target_os = "macos"))]
pub fn bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    (dir.join("zlo-backend").is_file() || dir.join("zlo-light").is_file()).then_some(exe)
}

#[cfg(not(target_os = "macos"))]
fn login_file_path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| Some(Path::new(&std::env::var_os("HOME")?).join(".config")))?;
    Some(config.join("autostart").join("zlo.desktop"))
}

#[cfg(target_os = "macos")]
fn login_file_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        Path::new(&home)
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")),
    )
}

pub fn is_enabled() -> bool {
    login_file_path().is_some_and(|p| p.exists())
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let path = login_file_path().ok_or("HOME is not set")?;
    if !enabled {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
        };
    }
    let bundle = bundle_path().ok_or(NOT_INSTALLED)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(&path, login_file(&bundle))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

const NOT_INSTALLED: &str = if cfg!(target_os = "macos") {
    "ZLO is not running from its .app bundle"
} else {
    "ZLO is not running from its install (install-linux.sh)"
};

/// The autostart entry running the installed app.
#[cfg(not(target_os = "macos"))]
fn login_file(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName=ZLO\nExec={}\nX-GNOME-Autostart-enabled=true\n",
        exec_value(&exe.display().to_string())
    )
}

/// `path` as a Desktop Entry `Exec` value running it, in the spec's two
/// layers: a double-quoted argument (`"`, `` ` ``, `$` and `\` escaped with
/// `\`, `%` doubled so it is no field code), then the string value's own
/// escapes (`\` doubled, control characters as `\n`, `\t`, `\r`).
/// `install-linux.sh` writes the same encoding.
#[cfg(not(target_os = "macos"))]
fn exec_value(path: &str) -> String {
    let mut arg = String::from("\"");
    for c in path.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                arg.push('\\');
                arg.push(c);
            }
            '%' => arg.push_str("%%"),
            _ => arg.push(c),
        }
    }
    arg.push('"');
    let mut value = String::new();
    for c in arg.chars() {
        match c {
            '\\' => value.push_str("\\\\"),
            '\n' => value.push_str("\\n"),
            '\t' => value.push_str("\\t"),
            '\r' => value.push_str("\\r"),
            _ => value.push(c),
        }
    }
    value
}

#[cfg(target_os = "macos")]
fn login_file(bundle: &Path) -> String {
    let bundle = xml_escape(&bundle.display().to_string());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/bin/open</string>
        <string>-a</string>
        <string>{bundle}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#
    )
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::login_file;
    use std::path::Path;

    #[cfg(target_os = "macos")]
    #[test]
    fn plist_opens_the_bundle_and_escapes_its_path() {
        let text = login_file(Path::new("/Users/a&b/Applications/ZLO.app"));
        assert!(text.contains("<string>/Users/a&amp;b/Applications/ZLO.app</string>"));
        assert!(text.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(text.contains("<string>com.numoh.desktop</string>"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn autostart_runs_the_installed_app_and_quotes_its_path() {
        let text = login_file(Path::new("/home/a b/.local/share/zlo/app/zlo"));
        assert!(text.starts_with("[Desktop Entry]\n"), "{text}");
        assert!(
            text.contains("Exec=\"/home/a b/.local/share/zlo/app/zlo\"\n"),
            "{text}"
        );
    }

    /// The spec's two layers: `z"l$o%\` is quoted for the command line
    /// (`\"`, `\$`, `%%`, `\\`), then every backslash doubled for the
    /// string value.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn exec_values_escape_both_layers() {
        assert_eq!(super::exec_value("/a b/zlo"), "\"/a b/zlo\"");
        assert_eq!(
            super::exec_value("/x/z\"l$o%\\"),
            "\"/x/z\\\\\"l\\\\$o%%\\\\\\\\\""
        );
    }
}
