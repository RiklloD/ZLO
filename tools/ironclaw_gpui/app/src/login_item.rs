//! "Open at login" via a per-user LaunchAgent that runs `open -a <bundle>`.
//!
//! launchd reads `~/Library/LaunchAgents` at login, so writing or removing
//! the plist is the whole switch. Only offered when the app runs from its
//! `.app` bundle: a bare cargo binary has no stable path worth registering.

use std::path::{Path, PathBuf};

const LABEL: &str = "com.ironclaw.desktop";

/// The `.app` this process runs from, if any.
pub fn bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // <bundle>.app/Contents/MacOS/<exe>
    let bundle = exe.parent()?.parent()?.parent()?;
    (bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

fn plist_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        Path::new(&home)
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")),
    )
}

pub fn is_enabled() -> bool {
    plist_path().is_some_and(|p| p.exists())
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let path = plist_path().ok_or("HOME is not set")?;
    if !enabled {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("cannot remove {}: {e}", path.display())),
        };
    }
    let bundle = bundle_path().ok_or("IronClaw is not running from its .app bundle")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(&path, plist(&bundle))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn plist(bundle: &Path) -> String {
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

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::plist;
    use std::path::Path;

    #[test]
    fn plist_opens_the_bundle_and_escapes_its_path() {
        let text = plist(Path::new("/Users/a&b/Applications/IronClaw.app"));
        assert!(text.contains("<string>/Users/a&amp;b/Applications/IronClaw.app</string>"));
        assert!(text.contains("<key>RunAtLoad</key>\n    <true/>"));
    }
}
