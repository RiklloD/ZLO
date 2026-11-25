//! The PATH a terminal would have. An app opened from the Finder or the Dock
//! inherits launchd's `/usr/bin:/bin:/usr/sbin:/sbin`, so the backend it
//! starts would not find the coding-agent CLIs installed through npm, nvm,
//! bun or Homebrew, nor import their sessions. The login shell's PATH is
//! asked once (t3code does the same), merged ahead of this process's own,
//! and handed to the backend.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long the login shell may take (a slow `.zshrc` must not hold up the
/// backend's start).
const SHELL_TIMEOUT: Duration = Duration::from_secs(3);
const MARK: &str = "__ZLO_LOGIN_PATH__";

/// The PATH to start the backend with: the login shell's entries first,
/// then this process's, each once. Asked once per app run.
pub fn path() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        let own = std::env::var("PATH").unwrap_or_default(); // silent-ok: no PATH merges the shell's alone
        let shell = login_shell_output()
            .as_deref()
            .and_then(extract)
            .unwrap_or_default(); // silent-ok: no answer keeps this process's PATH
        merge(&shell, &own)
    })
}

/// `$SHELL -ilc` printing PATH between marks, killed after
/// [`SHELL_TIMEOUT`]. `None` when there is no shell or it did not answer.
fn login_shell_output() -> Option<String> {
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| s.starts_with('/'))
        .unwrap_or_else(|| "/bin/zsh".to_string());
    let script = format!("printf '%s%s%s' '{MARK}' \"$PATH\" '{MARK}'");
    let mut child = Command::new(shell)
        .args(["-ilc", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| tracing::debug!(error = %e, "login shell did not start"))
        .ok()?;
    let deadline = Instant::now() + SHELL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) | Err(_) => {
                tracing::debug!("login shell did not answer; backend keeps the app's PATH");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    Some(out)
}

/// The PATH between the marks (an interactive shell may print around it).
fn extract(output: &str) -> Option<String> {
    let start = output.find(MARK)? + MARK.len();
    let len = output[start..].find(MARK)?;
    let path = output[start..start + len].trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// `first`'s entries, then `second`'s, empty and repeated ones dropped.
fn merge(first: &str, second: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    first
        .split(':')
        .chain(second.split(':'))
        .filter(|dir| !dir.is_empty() && seen.insert(*dir))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_is_read_between_the_marks() {
        let noisy = format!("Last login: today\n{MARK}/a/bin:/usr/bin{MARK}\nbye");
        assert_eq!(extract(&noisy).as_deref(), Some("/a/bin:/usr/bin"));
        assert_eq!(extract("no marks"), None);
        assert_eq!(extract(&format!("{MARK}{MARK}")), None);
    }

    #[test]
    fn the_shell_path_comes_first_and_nothing_repeats() {
        assert_eq!(
            merge(
                "/Users/me/.nvm/versions/node/v22/bin:/usr/bin",
                "/usr/bin:/bin::/usr/sbin"
            ),
            "/Users/me/.nvm/versions/node/v22/bin:/usr/bin:/bin:/usr/sbin"
        );
        assert_eq!(merge("", "/usr/bin:/bin"), "/usr/bin:/bin");
    }
}
