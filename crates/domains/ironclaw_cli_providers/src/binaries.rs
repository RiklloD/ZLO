//! Where the coding-agent CLIs are found. A backend started by the desktop
//! app inherits a GUI app's PATH (`/usr/bin:/bin:/usr/sbin:/sbin`), so the
//! usual install folders are searched too: the standalone installers'
//! `~/.local/bin`, Homebrew, the JavaScript version managers (nvm, volta,
//! bun, a user npm prefix, pnpm), and last the copies the ChatGPT and Claude
//! desktop apps bundle (`codex`, `claude`).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The first `name` executable: on `PATH`, then in the install folders.
pub(crate) fn find(home: &Path, name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH");
    search(
        home,
        name,
        path.as_deref(),
        &[PathBuf::from("/Applications"), home.join("Applications")],
    )
}

/// [`find`] over an explicit `PATH` and app folders.
pub(crate) fn search(
    home: &Path,
    name: &str,
    path: Option<&OsStr>,
    app_roots: &[PathBuf],
) -> Option<PathBuf> {
    let dirs = path
        .map(|p| std::env::split_paths(p).collect::<Vec<_>>())
        .unwrap_or_default() // silent-ok: no PATH searches the install folders only
        .into_iter()
        .chain(install_dirs(home, name));
    dirs.map(|dir| dir.join(name))
        .chain(bundled(home, name, app_roots))
        .find(|p| executable(p))
}

/// The install folders, most usual first.
fn install_dirs(home: &Path, name: &str) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".local/bin")];
    if name == "claude" {
        // The CLI's own `claude migrate-installer` location.
        dirs.push(home.join(".claude/local"));
    }
    dirs.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
    ]);
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

/// The copies the desktop apps ship, newest first.
fn bundled(home: &Path, name: &str, app_roots: &[PathBuf]) -> Vec<PathBuf> {
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
        // `claude-code/<version>/<build>/claude.app/Contents/MacOS/claude`.
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

/// `dir`'s subfolders, the highest version first (`v22.3.0` above
/// `v9.11.2`; names that are not versions last).
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
        .and_then(OsStr::to_str)
        .map(|n| n.trim_start_matches('v'))
        .map(|n| {
            n.split(['.', '-'])
                .map_while(|part| part.parse::<u64>().ok())
                .collect()
        })
        .unwrap_or_default() // silent-ok: a name that is not UTF-8 sorts last
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn tool(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_gui_path_still_finds_the_newest_nvm_install() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        tool(&h.join(".nvm/versions/node/v9.11.2/bin/codex"));
        tool(&h.join(".nvm/versions/node/v22.3.0/bin/codex"));
        let gui = OsStr::new("/usr/bin:/bin:/usr/sbin:/sbin");
        assert_eq!(
            search(h, "codex", Some(gui), &[]),
            Some(h.join(".nvm/versions/node/v22.3.0/bin/codex"))
        );
    }

    #[test]
    fn the_desktop_apps_bundled_clis_are_found_last() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let apps = h.join("Applications");
        tool(&apps.join("ChatGPT.app/Contents/Resources/codex-cli/bin/codex"));
        let claude = h.join(
            "Library/Application Support/Claude/claude-code/2.1.286/f2326db61802/claude.app/Contents/MacOS/claude",
        );
        tool(&claude);
        tool(&h.join(
            "Library/Application Support/Claude/claude-code/2.1.9/aaaa/claude.app/Contents/MacOS/claude",
        ));
        let roots = [apps.clone()];
        assert_eq!(
            search(h, "codex", None, &roots),
            Some(apps.join("ChatGPT.app/Contents/Resources/codex-cli/bin/codex"))
        );
        assert_eq!(search(h, "claude", None, &roots), Some(claude));
        // An install of its own wins over the bundled copy.
        tool(&h.join(".bun/bin/codex"));
        assert_eq!(
            search(h, "codex", None, &roots),
            Some(h.join(".bun/bin/codex"))
        );
    }

    #[test]
    fn path_comes_first_and_a_plain_file_is_not_a_cli() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let on_path = h.join("custom");
        tool(&on_path.join("claude"));
        tool(&h.join(".local/bin/claude"));
        assert_eq!(
            search(h, "claude", Some(on_path.as_os_str()), &[]),
            Some(on_path.join("claude"))
        );
        let other = h.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("muse"), "not executable").unwrap();
        assert_eq!(search(h, "muse", Some(other.as_os_str()), &[]), None);
        assert_eq!(
            search(h, "claude", None, &[]),
            Some(h.join(".local/bin/claude"))
        );
    }
}
