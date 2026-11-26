//! What differs between macOS and Linux when ZLO hands something to the
//! system: opening a file or link in its app, revealing it, the shell that
//! runs a command line, the computer's name, and finding a system tool.
//! Callers stay platform-neutral; each answer here has one macOS and one
//! Linux (any other Unix) branch.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// What runs a command line on this computer: a login shell that knows
/// `pipefail` (zsh on macOS, as Terminal does; bash elsewhere, whose
/// availability on Linux is near-universal, else `sh`).
pub struct ScriptShell {
    pub program: &'static str,
    /// Prefix making a pipe fail when any of its commands does.
    pub pipefail: &'static str,
}

pub fn script_shell() -> ScriptShell {
    if cfg!(target_os = "macos") {
        return ScriptShell {
            program: "/bin/zsh",
            pipefail: "setopt pipefail; ",
        };
    }
    if Path::new("/bin/bash").is_file() {
        ScriptShell {
            program: "/bin/bash",
            pipefail: "set -o pipefail; ",
        }
    } else {
        ScriptShell {
            program: "/bin/sh",
            pipefail: "",
        }
    }
}

/// The person's login shell (`$SHELL`), else the system's default one.
pub fn login_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| s.starts_with('/'))
        .unwrap_or_else(|| script_shell().program.to_string())
}

/// The system's opener: `open(1)` on macOS, `xdg-open` elsewhere.
fn opener() -> Command {
    if cfg!(target_os = "macos") {
        Command::new("/usr/bin/open")
    } else {
        Command::new("xdg-open")
    }
}

fn quiet(mut cmd: Command) -> Command {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Open a file, folder or link in the app the system picks for it. The
/// opener returns at once; waiting on it on a thread reaps the child.
pub fn open(target: impl AsRef<OsStr>) -> io::Result<()> {
    let mut cmd = quiet(opener());
    cmd.arg(target.as_ref());
    let mut child = cmd.spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Show `path` in the file manager: selected in Finder on macOS; on Linux,
/// the folder holding it (file managers have no common "select" call).
pub fn reveal(path: &Path) -> io::Result<()> {
    if cfg!(target_os = "macos") {
        let mut cmd = quiet(opener());
        cmd.arg("-R").arg(path);
        let mut child = cmd.spawn()?;
        std::thread::spawn(move || child.wait());
        return Ok(());
    }
    let folder = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    open(folder)
}

/// This computer's name as people know it: the Sharing name on macOS, the
/// host name elsewhere.
pub fn computer_name() -> Option<String> {
    let out = if cfg!(target_os = "macos") {
        Command::new("/usr/sbin/scutil")
            .args(["--get", "ComputerName"])
            .output()
    } else {
        return std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .or_else(|| std::env::var("HOSTNAME").ok().filter(|n| !n.is_empty()));
    };
    out.ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|n| !n.is_empty())
}

/// What this computer is called when nothing better is known.
pub const FALLBACK_NAME: &str = if cfg!(target_os = "macos") {
    "Mac"
} else {
    "Linux"
};

/// The operating system's name, for pairing records.
pub const OS_NAME: &str = if cfg!(target_os = "macos") {
    "macOS"
} else {
    "Linux"
};

/// A system tool by name: the first of the usual system folders that has
/// it (`/usr/sbin/lsof` on macOS, `/usr/bin/lsof` on most Linux), else the
/// bare name for `PATH` to resolve.
pub fn system_tool(name: &str) -> PathBuf {
    ["/usr/bin", "/usr/sbin", "/bin", "/sbin"]
        .into_iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Control held as a modifier of its own, besides the shortcut key: ⌃ on
/// macOS. On Linux Ctrl *is* the shortcut key (Rune sets `cmd` with it), so
/// it never counts as an extra there.
pub fn extra_ctrl(m: rune::Mods) -> bool {
    cfg!(target_os = "macos") && m.ctrl
}

/// The shortcut key (⌘ on macOS, Ctrl on Linux) with no other modifier.
pub fn primary_alone(m: rune::Mods) -> bool {
    m.cmd && !extra_ctrl(m) && !m.alt && !m.shift
}

/// Where to look for programs: the login shell's PATH (`login_path`), which
/// a desktop-launched app does not inherit. Tests use this process's own
/// PATH and never start a login shell.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn search_path() -> String {
    if cfg!(test) {
        return std::env::var("PATH").unwrap_or_default(); // silent-ok: no PATH finds nothing
    }
    crate::login_path::path().to_string()
}

/// A program on `path` (colon-separated, the login PATH), if any.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn find_on_path(name: &str, path: &str) -> Option<PathBuf> {
    path.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_shell_has_pipefail_where_it_can() {
        let shell = script_shell();
        assert!(Path::new(shell.program).is_file(), "{}", shell.program);
        if shell.program != "/bin/sh" {
            let ok = Command::new(shell.program)
                .args(["-fc", &format!("{}false | cat", shell.pipefail)])
                .status()
                .unwrap();
            assert!(!ok.success(), "pipefail makes the pipe fail");
        }
    }

    #[test]
    fn programs_are_found_on_a_given_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tool"), "").unwrap();
        let path = format!("/nonexistent:{}", dir.path().display());
        assert_eq!(find_on_path("tool", &path), Some(dir.path().join("tool")));
        assert_eq!(find_on_path("missing", &path), None);
    }
}
