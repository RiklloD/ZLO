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

/// `$SHELL -ilc` printing PATH between marks. The whole probe has
/// [`SHELL_TIMEOUT`]: stdout is drained on its own thread (a chatty
/// `.zshrc` cannot fill the pipe), the answer is taken as soon as both
/// marks are in (a background job keeping stdout open does not hold it up),
/// and the shell is killed when it is not done in time. `None` when there
/// is no shell or no answer in time.
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
    let stdout = child.stdout.take()?;
    let answer = read_marked(stdout, Instant::now() + SHELL_TIMEOUT);
    // Done with it either way; an exited shell makes these no-ops.
    let _ = child.kill();
    let _ = child.wait();
    if answer.is_none() {
        tracing::debug!("login shell did not answer; backend keeps the app's PATH");
    }
    answer
}

/// Output kept while waiting for the marks.
const OUTPUT_MAX: usize = 1024 * 1024;

/// Read `from` on a thread until the marked PATH is in, it ends, or
/// `deadline`. The thread is left behind when a descendant keeps the pipe
/// open; it ends with the pipe.
fn read_marked(mut from: impl Read + Send + 'static, deadline: Instant) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let chunk = buf[..n].to_vec(); // safety: bytes, not a string; n <= buf.len()
                    if tx.send(chunk).is_err() {
                        return;
                    }
                }
            }
        }
    });
    let mut out = Vec::new();
    loop {
        let left = deadline.checked_duration_since(Instant::now())?;
        match rx.recv_timeout(left) {
            Ok(chunk) => {
                if out.len() < OUTPUT_MAX {
                    out.extend_from_slice(&chunk);
                }
                if let Some(path) = extract(&String::from_utf8_lossy(&out)) {
                    return Some(path);
                }
            }
            // Ended without the marks, or out of time.
            Err(_) => return None,
        }
    }
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

    /// A reader that answers, then never ends (a background job holding
    /// the shell's stdout open).
    struct ThenHang(Option<Vec<u8>>);

    impl Read for ThenHang {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.take() {
                Some(bytes) => {
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    Ok(bytes.len())
                }
                None => {
                    std::thread::sleep(Duration::from_secs(60));
                    Ok(0)
                }
            }
        }
    }

    #[test]
    fn the_answer_is_taken_without_waiting_for_the_pipe_to_close() {
        let answer = format!("{MARK}/a/bin{MARK}").into_bytes();
        let started = Instant::now();
        let path = read_marked(
            ThenHang(Some(answer)),
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(path.as_deref(), Some("/a/bin"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_silent_shell_gives_up_at_the_deadline() {
        let started = Instant::now();
        let path = read_marked(ThenHang(None), Instant::now() + Duration::from_millis(200));
        assert_eq!(path, None);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn chatty_output_ahead_of_the_answer_is_read_through() {
        // More than a pipe buffer of noise before the marks.
        let mut out = "x".repeat(200 * 1024).into_bytes();
        out.extend(format!("{MARK}/a/bin:/usr/bin{MARK}").into_bytes());
        let path = read_marked(
            std::io::Cursor::new(out),
            Instant::now() + Duration::from_secs(5),
        );
        assert_eq!(path.as_deref(), Some("/a/bin:/usr/bin"));
    }

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
