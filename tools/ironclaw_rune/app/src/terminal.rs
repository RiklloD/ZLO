//! Run a command in a terminal window, shown first and started only on
//! Enter: Terminal.app on macOS, the first terminal emulator found on Linux.
//! Used where the person runs something on this computer themselves: an
//! agent's install or sign-in (setup), `zlo triage` from the connect-failure
//! screen.

/// The shell line Terminal runs: show the command, wait for Enter, run it.
pub fn script(command: &str) -> String {
    let shown = command.replace('\'', "'\\''");
    format!(
        "clear; printf '\\n  %s\\n\\n' '{shown}'; printf 'Press Enter to run it, or close this window to cancel. '; read _ && {command}"
    )
}

/// AppleScript string literal of `s`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Open a Terminal window on [`script`]`(command)`. Blocking (osascript):
/// call it off the UI thread.
#[cfg(target_os = "macos")]
pub fn run(command: &str) -> Result<(), String> {
    let script = format!(
        "tell application \"Terminal\"\nactivate\ndo script {}\nend tell",
        applescript_string(&script(command))
    );
    let out = std::process::Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|e| format!("Could not open Terminal: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err("Could not open Terminal. Copy the command and run it yourself.".into())
    }
}

/// Terminal emulators tried in order, each with the arguments that make it
/// run one command line: Debian's alternatives link first, then the common
/// desktops' own terminals and the popular standalone ones.
#[cfg(not(target_os = "macos"))]
const TERMINALS: [(&str, &[&str]); 9] = [
    ("x-terminal-emulator", &["-e"]),
    ("gnome-terminal", &["--"]),
    ("konsole", &["-e"]),
    ("xfce4-terminal", &["-x"]),
    ("kitty", &[]),
    ("alacritty", &["-e"]),
    ("ghostty", &["-e"]),
    ("wezterm", &["start", "--"]),
    ("xterm", &["-e"]),
];

/// Open a terminal window on [`script`]`(command)`: the first installed of
/// [`TERMINALS`], on the login PATH. Off the UI thread, like macOS's.
#[cfg(not(target_os = "macos"))]
pub fn run(command: &str) -> Result<(), String> {
    let path = &crate::system::search_path();
    let shell = crate::system::script_shell().program;
    let line = script(command);
    for (name, args) in TERMINALS {
        let Some(program) = crate::system::find_on_path(name, path) else {
            continue;
        };
        let spawned = std::process::Command::new(program)
            .args(args)
            .args([shell, "-lc", &line])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        match spawned {
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
                return Ok(());
            }
            Err(e) => tracing::debug!(error = %e, terminal = name, "terminal did not start"),
        }
    }
    Err("No terminal app found. Copy the command and run it yourself.".into())
}

/// Open a terminal window with its shell in `folder` (Linux's "Open in →
/// Terminal").
#[cfg(not(target_os = "macos"))]
pub fn open_in(folder: &std::path::Path) -> std::io::Result<()> {
    let path = &crate::system::search_path();
    for (name, _) in TERMINALS {
        let Some(program) = crate::system::find_on_path(name, path) else {
            continue;
        };
        match std::process::Command::new(program)
            .current_dir(folder)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                std::thread::spawn(move || child.wait());
                return Ok(());
            }
            Err(e) => tracing::debug!(error = %e, terminal = name, "terminal did not start"),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no terminal app found",
    ))
}

/// Single-quote a path for a POSIX shell when it needs it.
pub fn shell_quote(path: &str) -> String {
    if path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-~".contains(c))
    {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_shows_the_command_and_waits_for_enter() {
        let line = script("claude auth login");
        assert!(line.contains("'claude auth login'"));
        assert!(line.ends_with("read _ && claude auth login"));
        assert_eq!(applescript_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(shell_quote("/Users/a/zlo"), "/Users/a/zlo");
        assert_eq!(shell_quote("/Users/a b/zlo"), "'/Users/a b/zlo'");
        assert_eq!(shell_quote("/x/it's"), "'/x/it'\\''s'");
    }
}
