//! Run a command in Terminal.app, shown first and started only on Enter.
//! Used where the person runs something on this Mac themselves: an agent's
//! install or sign-in (setup), `zlo triage` from the connect-failure screen.

/// The shell line Terminal runs: show the command, wait for Enter, run it.
pub fn script(command: &str) -> String {
    let shown = command.replace('\'', "'\\''");
    format!(
        "clear; printf '\\n  %s\\n\\n' '{shown}'; printf 'Press Enter to run it, or close this window to cancel. '; read _ && {command}"
    )
}

/// AppleScript string literal of `s`.
fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Open a Terminal window on [`script`]`(command)`. Blocking (osascript):
/// call it off the UI thread.
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
