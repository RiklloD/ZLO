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
        let shell = crate::system::login_shell();
        let own = std::env::var("PATH").unwrap_or_default(); // silent-ok: no PATH merges the shell's alone
        backend_path(&shell, &own, SHELL_TIMEOUT)
    })
}

/// `shell`'s login PATH ahead of `own`; `own` alone when the shell gives
/// none in `timeout`.
fn backend_path(shell: &str, own: &str, timeout: Duration) -> String {
    let login = login_shell_path(shell, timeout);
    if login.is_none() {
        tracing::debug!("login shell did not answer; backend keeps the app's PATH");
    }
    merge(login.as_deref().unwrap_or_default(), own)
}

/// Shell command that prints `PATH` colon-separated between two [`MARK`]s.
/// POSIX shells already store `$PATH` colon-separated; `fish` and `nu` store
/// it as a list and would otherwise print spaces or structured text.
fn path_script(shell: &str) -> String {
    let name = std::path::Path::new(shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    match name {
        "fish" => format!("printf '%s%s%s' '{MARK}' (string join : -- $PATH) '{MARK}'"),
        "nu" => format!("print --no-newline $\"{MARK}($env.PATH | str join ':'){MARK}\""),
        _ => format!("printf '%s%s%s' '{MARK}' \"$PATH\" '{MARK}'"),
    }
}

#[cfg(unix)]
fn kill_process_group(pgid: u32) {
    // silent-ok: the shell may already have exited.
    let _ = Command::new("/bin/kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// `shell -ilc` printing PATH between marks. The whole probe has `timeout`:
/// stdout is read on its own thread (a chatty `.zshrc` cannot fill the
/// pipe), the answer is taken as soon as both marks are in (a background
/// job keeping stdout open does not hold it up), and the shell's whole
/// process group is killed when it is not done in time.
fn login_shell_path(shell: &str, timeout: Duration) -> Option<String> {
    let script = path_script(shell);
    let mut cmd = Command::new(shell);
    cmd.args(["-ilc", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| tracing::debug!(error = %e, "login shell did not start"))
        .ok()?;
    let pid = child.id();
    let stdout = child.stdout.take()?;
    let answer = read_marked(stdout, Instant::now() + timeout);
    // Done with it either way; kill the whole process group so any
    // descendant started by a slow profile cannot leak or hold stdout open.
    #[cfg(unix)]
    kill_process_group(pid);
    let _ = child.kill();
    let _ = child.wait();
    answer
}

/// Output read while waiting for the marks; past it the probe gives up.
const OUTPUT_MAX: usize = 1024 * 1024;

/// The marked PATH read from `from` on a thread, by `deadline`. The thread
/// parses as it reads and sends one answer; it is left behind when a
/// descendant keeps the pipe open, and ends with the pipe.
fn read_marked(mut from: impl Read + Send + 'static, deadline: Instant) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Option<String>>(1);
    std::thread::spawn(move || {
        let _ = tx.send(scan_marked(&mut from));
    });
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
        .flatten()
}

/// Read until the text between the first two marks is in (each byte looked
/// at about once), the stream ends, or [`OUTPUT_MAX`].
fn scan_marked(from: &mut impl Read) -> Option<String> {
    let mark = MARK.as_bytes();
    let overlap = mark.len() - 1;
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    // Where the next search starts, and where the PATH starts once the
    // first mark is found.
    let mut seek = 0;
    let mut start: Option<usize> = None;
    loop {
        let n = from.read(&mut buf).ok()?;
        out.extend_from_slice(buf.get(..n)?);
        if n == 0 || out.len() > OUTPUT_MAX {
            return None;
        }
        if start.is_none() {
            match find(out.get(seek..)?, mark) {
                Some(at) => {
                    start = Some(seek + at + mark.len());
                    seek = seek + at + mark.len();
                }
                None => {
                    seek = out.len().saturating_sub(overlap);
                    continue;
                }
            }
        }
        let from_ix = start?;
        match find(out.get(seek..)?, mark) {
            Some(at) => {
                let path = String::from_utf8_lossy(out.get(from_ix..seek + at)?);
                let path = path.trim();
                return (!path.is_empty()).then(|| path.to_string());
            }
            None => seek = out.len().saturating_sub(overlap).max(from_ix),
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The PATH between the marks (an interactive shell may print around it).
#[cfg(test)]
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
    fn a_cli_folder_only_the_login_shell_knows_reaches_the_backend() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // A stand-in login shell whose profile adds a folder of its own.
        let shell = dir.path().join("sh");
        std::fs::write(
            &shell,
            "#!/bin/sh\necho 'Welcome back'\nPATH=/only/from/login:$PATH\neval \"$2\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = backend_path(
            shell.to_str().unwrap(),
            "/usr/bin:/bin",
            Duration::from_secs(5),
        );
        assert!(path.starts_with("/only/from/login:"), "{path}");
        let dirs: Vec<&str> = path.split(':').collect();
        assert!(
            dirs.contains(&"/usr/bin") && dirs.contains(&"/bin"),
            "{path}"
        );
        // No shell: the app's own PATH.
        assert_eq!(
            backend_path("/nonexistent/shell", "/usr/bin", Duration::from_secs(1)),
            "/usr/bin"
        );
    }

    #[test]
    fn a_mark_split_across_reads_is_still_found() {
        struct Trickle(Vec<u8>, usize);
        impl Read for Trickle {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let Some(&b) = self.0.get(self.1) else {
                    return Ok(0);
                };
                buf[0] = b;
                self.1 += 1;
                Ok(1)
            }
        }
        let bytes = format!("noise{MARK}/a/bin{MARK}tail").into_bytes();
        assert_eq!(
            scan_marked(&mut Trickle(bytes, 0)).as_deref(),
            Some("/a/bin")
        );
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

    #[test]
    fn fish_shell_uses_colon_joined_path_script() {
        let fish = path_script("/opt/homebrew/bin/fish");
        assert!(fish.contains("string join : -- $PATH"), "{fish}");
        assert!(fish.contains(MARK), "{fish}");
        let zsh = path_script("/bin/zsh");
        assert!(zsh.contains("\"$PATH\""), "{zsh}");
    }

    #[cfg(unix)]
    #[test]
    fn a_timed_out_shell_kills_its_descendants() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let pgid_file = dir.path().join("pgid");
        let shell = dir.path().join("slow-sh");
        std::fs::write(
            &shell,
            format!(
                "#!/bin/sh\necho $$ > {}\nsleep 30 &\nwait\n",
                pgid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = Instant::now();
        let path = login_shell_path(shell.to_str().unwrap(), Duration::from_millis(250));
        assert_eq!(path, None);
        assert!(started.elapsed() < Duration::from_secs(2));

        if let Ok(raw) = std::fs::read_to_string(&pgid_file)
            && let Ok(pgid) = raw.trim().parse::<u32>()
        {
            // Give the kernel a moment to reap the killed process group.
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut alive = true;
            while Instant::now() < deadline {
                alive = Command::new("/bin/kill")
                    .args(["-0", "--", &format!("-{pgid}")])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                if !alive {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert!(!alive, "descendant process group {pgid} leaked");
        }
    }
}
