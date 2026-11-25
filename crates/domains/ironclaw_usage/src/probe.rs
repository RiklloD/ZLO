//! Asking a provider's own CLI for its limits over JSON lines, as t3code does
//! (`claude -p` control requests, `codex app-server` JSON-RPC). The CLI uses
//! and refreshes its own login, so no token is read here.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(25);

/// What the caller wants after one line from the CLI.
pub(crate) enum Step {
    /// Keep reading; write these frames first.
    Send(Vec<Value>),
    Done(Value),
}

/// Spawn `binary args`, write `frames`, and feed every JSON line to `want`
/// until it returns [`Step::Done`]. `None` on spawn failure, EOF or timeout;
/// the child is killed either way.
pub(crate) async fn run(
    binary: &Path,
    args: &[&str],
    env: &[(&str, Option<&Path>)],
    frames: Vec<Value>,
    mut want: impl FnMut(&Value) -> Step,
) -> Option<Value> {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.args(args)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (key, value) in env {
        match value {
            Some(value) => cmd.env(key, value),
            None => cmd.env_remove(key),
        };
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::debug!(%error, binary = %binary.display(), "usage probe did not start");
            return None;
        }
    };
    let mut stdin = child.stdin.take()?;
    let mut lines = BufReader::new(child.stdout.take()?).lines();
    let exchange = async {
        write(&mut stdin, &frames).await?;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match want(&message) {
                Step::Send(more) => write(&mut stdin, &more).await?,
                Step::Done(value) => return Some(value),
            }
        }
        None
    };
    let result = tokio::time::timeout(PROBE_TIMEOUT, exchange)
        .await
        .ok()
        .flatten();
    if result.is_none() {
        tracing::debug!(binary = %binary.display(), "usage probe gave no answer");
    }
    #[expect(
        clippy::let_underscore_must_use,
        reason = "the child may have exited already"
    )]
    let _ = child.start_kill();
    result
}

async fn write(stdin: &mut tokio::process::ChildStdin, frames: &[Value]) -> Option<()> {
    for frame in frames {
        let mut line = frame.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).await.ok()?;
    }
    stdin.flush().await.ok()
}

/// `name` on `PATH`, then the usual install folders and desktop-app bundled
/// copies, matching `ironclaw_cli_providers::binaries::find`.
pub(crate) fn find_binary(home: &Path, name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH");
    search_binary(
        home,
        name,
        path.as_deref(),
        &[Path::new("/opt/homebrew/bin"), Path::new("/usr/local/bin")],
        &[PathBuf::from("/Applications"), home.join("Applications")],
    )
}

fn search_binary(
    home: &Path,
    name: &str,
    path: Option<&std::ffi::OsStr>,
    system_dirs: &[&Path],
    app_roots: &[PathBuf],
) -> Option<PathBuf> {
    let dirs = path
        .map(|p| std::env::split_paths(p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain(install_dirs(home, name, system_dirs));
    dirs.map(|dir| dir.join(name))
        .chain(bundled_binaries(home, name, app_roots))
        .find(|p| is_executable(p))
}

fn install_dirs(home: &Path, name: &str, system_dirs: &[&Path]) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".local/bin")];
    if name == "claude" {
        dirs.push(home.join(".claude/local"));
    }
    dirs.extend(system_dirs.iter().map(|p| p.to_path_buf()));
    dirs.extend(newest_children(&home.join(".nvm/versions/node")).map(|v| v.join("bin")));
    dirs.extend([
        home.join(".volta/bin"),
        home.join(".bun/bin"),
        home.join(".npm-global/bin"),
        home.join(".npm/bin"),
        home.join("Library/pnpm"),
    ]);
    dirs
}

fn bundled_binaries(home: &Path, name: &str, app_roots: &[PathBuf]) -> Vec<PathBuf> {
    match name {
        "codex" => app_roots
            .iter()
            .flat_map(|root| {
                [
                    root.join("ChatGPT.app/Contents/Resources/codex-cli/bin/codex"),
                    root.join("Codex.app/Contents/Resources/codex-cli/bin/codex"),
                    root.join("Codex.app/Contents/Resources/codex"),
                ]
            })
            .collect(),
        "claude" => newest_children(&home.join("Library/Application Support/Claude/claude-code"))
            .flat_map(|version| {
                std::fs::read_dir(&version)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|build| build.path().join("claude.app/Contents/MacOS/claude"))
                    .collect::<Vec<_>>()
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn newest_children(dir: &Path) -> impl Iterator<Item = PathBuf> {
    let mut children: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    children.sort_by_key(|p| std::cmp::Reverse(version_key(p)));
    children.into_iter()
}

fn version_key(path: &Path) -> Vec<u64> {
    path.file_name()
        .and_then(std::ffi::OsStr::to_str)
        .map(|n| n.trim_start_matches('v'))
        .map(|n| {
            n.split(['.', '-'])
                .map_while(|part| part.parse::<u64>().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// Unix seconds or millis to millis.
pub(crate) fn epoch_ms(raw: &Value) -> Option<i64> {
    let n = raw.as_f64()?;
    (n > 0.0).then_some(if n < 1e12 { n * 1000.0 } else { n } as i64)
}

/// "max" → "Max".
pub(crate) fn plan_name(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let mut chars = raw.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bin(path: &Path, executable: bool) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = if executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
    }

    #[test]
    fn search_binary_finds_claude_local_and_ignores_non_executable() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let non_exec = home.join(".local/bin/claude");
        make_bin(&non_exec, false);
        let claude_local = home.join(".claude/local/claude");
        make_bin(&claude_local, true);

        let found = search_binary(home, "claude", None, &[], &[]);
        assert_eq!(found.as_deref(), Some(claude_local.as_path()));
    }

    #[test]
    fn search_binary_picks_newest_nvm_and_bundled_app() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let old_nvm = home.join(".nvm/versions/node/v18.0.0/bin/codex");
        let new_nvm = home.join(".nvm/versions/node/v22.4.0/bin/codex");
        make_bin(&old_nvm, true);
        make_bin(&new_nvm, true);
        assert_eq!(
            search_binary(home, "codex", None, &[], &[]).as_deref(),
            Some(new_nvm.as_path())
        );

        let dir2 = tempfile::tempdir().unwrap();
        let home2 = dir2.path();
        let apps = home2.join("Applications");
        let bundled = apps.join("ChatGPT.app/Contents/Resources/codex-cli/bin/codex");
        make_bin(&bundled, true);
        assert_eq!(
            search_binary(home2, "codex", None, &[], &[apps]).as_deref(),
            Some(bundled.as_path())
        );
    }
}
