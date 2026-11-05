//! `git`, run so that a repository cannot make it execute anything.
//!
//! A mentioned folder may be any checkout, including one whose `.git/config`
//! or `.gitattributes` came from someone else. Several git settings name
//! commands git then runs: `core.fsmonitor`, `diff.<driver>.textconv`,
//! `diff.external`/`diff.<driver>.command`, `filter.<driver>.clean` and
//! `.process` (which `git diff` runs on worktree files), hooks and pagers.
//! Every call here:
//!
//! - starts from an empty environment (only `PATH` is kept), with system
//!   and global config switched off, so no host `GIT_*` variable or user
//!   config applies;
//! - overrides each executing setting on the command line, which wins over
//!   repository config, and empties every filter driver the repository
//!   config declares (read with `git config`, which runs nothing);
//! - passes `--no-textconv --no-ext-diff` to diffs and ignores submodules;
//! - runs in its own process group, killed whole on timeout or when the
//!   output cap is reached.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;

/// A git call that takes longer than this is abandoned.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
/// Filter drivers neutralized per call; a config declaring more is refused.
const MAX_FILTERS: usize = 32;
/// Settings that would otherwise run a command, forced off.
const SAFE_CONFIG: &[&str] = &[
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "core.pager=cat",
    "core.untrackedCache=false",
    "core.sshCommand=false",
    "diff.external=",
    "submodule.recurse=false",
    "protocol.allow=never",
];

/// stdout of `git <args>` in `dir`, at most `cap` bytes, or `None` when git
/// fails or times out.
pub(super) async fn run(dir: &Path, args: &[&str], cap: usize) -> Option<Vec<u8>> {
    let filters = filter_overrides(dir).await?;
    let mut config: Vec<String> = SAFE_CONFIG.iter().map(|s| s.to_string()).collect();
    config.extend(filters);
    raw(dir, &config, args, cap).await
}

/// `-c filter.<driver>.<key>=` for every filter driver the repository's
/// config declares, so `git diff` runs none of them.
async fn filter_overrides(dir: &Path) -> Option<Vec<String>> {
    let config: Vec<String> = SAFE_CONFIG.iter().map(|s| s.to_string()).collect();
    let listed = raw(
        dir,
        &config,
        &[
            "config",
            "--null",
            "--name-only",
            "--get-regexp",
            "^filter\\.",
        ],
        64 * 1024,
    )
    .await
    .unwrap_or_default(); // silent-ok: `git config` exits 1 when no filter is declared
    let mut drivers: Vec<String> = String::from_utf8_lossy(&listed)
        .split('\0')
        .filter_map(|name| {
            let rest = name.strip_prefix("filter.")?;
            let (driver, _) = rest.rsplit_once('.')?;
            Some(driver.to_string())
        })
        .collect();
    drivers.sort();
    drivers.dedup();
    if drivers.len() > MAX_FILTERS {
        tracing::debug!("mention: repository declares too many filter drivers");
        return None;
    }
    Some(
        drivers
            .iter()
            .flat_map(|d| {
                ["clean=", "smudge=", "process=", "required=false"]
                    .map(|key| format!("filter.{d}.{key}"))
            })
            .collect(),
    )
}

async fn raw(dir: &Path, config: &[String], args: &[&str], cap: usize) -> Option<Vec<u8>> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .arg("--no-pager")
        .arg("-C")
        .arg(dir);
    for setting in config {
        cmd.arg("-c").arg(setting);
    }
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|error| tracing::debug!(%error, "mention: git did not start"))
        .ok()?;
    let group = child
        .id()
        .and_then(|id| rustix::process::Pid::from_raw(id as i32));
    let kill_group = || {
        if let Some(group) = group {
            // silent-ok: the group may already be gone
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    };
    let mut stdout = child.stdout.take()?;
    let read = async {
        let mut out = Vec::new();
        (&mut stdout)
            .take(cap as u64)
            .read_to_end(&mut out)
            .await
            .map(|_| out)
    };
    let out = match tokio::time::timeout(GIT_TIMEOUT, read).await {
        Ok(Ok(out)) => out,
        _ => {
            kill_group();
            return None;
        }
    };
    drop(stdout);
    if out.len() >= cap {
        // Capped: the rest is not wanted.
        kill_group();
        return Some(out);
    }
    match tokio::time::timeout(GIT_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) if status.success() => Some(out),
        Ok(_) => None,
        Err(_) => {
            kill_group();
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A checkout whose config would run a marker script from every hook
    /// point `git diff` and `git ls-files` reach.
    #[tokio::test]
    async fn hostile_repository_config_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .unwrap()
        };
        if !git(&["init", "-q"]).status.success() {
            return; // no git on this machine
        }
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        let marker = dir.path().join("PWNED");
        let script = dir.path().join("evil.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let s = script.to_string_lossy().to_string();
        for (key, value) in [
            ("core.fsmonitor", s.as_str()),
            ("filter.evil.clean", s.as_str()),
            ("filter.evil.process", s.as_str()),
            ("filter.evil.required", "true"),
            ("diff.evil.textconv", s.as_str()),
            ("diff.evil.command", s.as_str()),
            ("diff.external", s.as_str()),
            ("core.pager", s.as_str()),
        ] {
            git(&["config", key, value]);
        }
        std::fs::write(repo.join(".gitattributes"), "*.txt filter=evil diff=evil\n").unwrap();
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();

        let diff = run(
            repo,
            &["diff", "HEAD", "--no-textconv", "--no-ext-diff"],
            1 << 20,
        )
        .await
        .expect("diff runs");
        let listing = run(
            repo,
            &["ls-files", "--others", "--exclude-standard"],
            1 << 20,
        )
        .await
        .expect("ls-files runs");
        assert!(String::from_utf8_lossy(&diff).contains("+two"));
        assert!(String::from_utf8_lossy(&listing).contains(".gitattributes"));
        assert!(!marker.exists(), "a repository-configured command ran");
    }
}
