//! Paths inside agy's profile that the driver reads or removes. Every
//! component below the profile is checked on its own: a plain folder for
//! each parent, the expected kind for the last one, never a link and never
//! a component that could leave the profile (`..`, a separator). A link in
//! place of `brain` or `conversations` would otherwise send a read, or the
//! probe cleanup's removal, outside the profile.

use std::path::{Path, PathBuf};

/// What the last component must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    File,
    Dir,
}

/// A checked path under the profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Managed {
    /// It exists, of the expected kind, reached through plain folders only.
    Found(PathBuf),
    /// It (or a parent folder) does not exist yet; every part that does is
    /// a plain folder.
    Missing(PathBuf),
}

impl Managed {
    pub(super) fn found(self) -> Option<PathBuf> {
        match self {
            Self::Found(path) => Some(path),
            Self::Missing(_) => None,
        }
    }
}

/// One path component the driver may join: no separator, no `.`/`..`.
pub(super) fn plain_component(part: &str) -> bool {
    !part.is_empty() && part != "." && part != ".." && !part.contains(['/', '\\', '\0'])
}

/// `profile/parts...`, checked part by part; `None` when any part is a
/// link, the wrong kind, or not a plain component. The profile folder
/// itself may be reached through a link (a shared profile), so it is
/// resolved first and everything below it is held to the rules.
pub(super) async fn managed(profile: &Path, parts: &[&str], kind: Kind) -> Option<Managed> {
    if parts.is_empty() || !parts.iter().all(|p| plain_component(p)) {
        return None;
    }
    let mut path = match tokio::fs::canonicalize(profile).await {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(Managed::Missing(parts.iter().fold(
                profile.to_path_buf(),
                |mut p, part| {
                    p.push(part);
                    p
                },
            )));
        }
        Err(error) => {
            tracing::debug!(%error, "antigravity: profile folder unreadable");
            return None;
        }
    };
    let last = parts.len() - 1;
    for (n, part) in parts.iter().enumerate() {
        path.push(part);
        let meta = match tokio::fs::symlink_metadata(&path).await {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                for rest in &parts[n + 1..] {
                    path.push(rest);
                }
                return Some(Managed::Missing(path));
            }
            Err(error) => {
                tracing::debug!(%error, "antigravity: profile path unreadable");
                return None;
            }
        };
        let file_type = meta.file_type();
        let fits = if n < last {
            file_type.is_dir()
        } else {
            match kind {
                Kind::File => file_type.is_file(),
                Kind::Dir => file_type.is_dir(),
            }
        };
        // `is_dir`/`is_file` on `symlink_metadata` are false for a link.
        if !fits {
            return None;
        }
    }
    Some(Managed::Found(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_component_is_checked_and_links_are_refused() {
        let profile = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let acp = profile.path().join("antigravity-acp");
        std::fs::create_dir_all(acp.join("conversations")).unwrap();
        std::fs::write(acp.join("conversations/s.db"), "x").unwrap();
        let found = managed(
            profile.path(),
            &["antigravity-acp", "conversations", "s.db"],
            Kind::File,
        )
        .await;
        assert!(matches!(found, Some(Managed::Found(_))), "{found:?}");
        assert!(matches!(
            managed(
                profile.path(),
                &["antigravity-acp", "conversations", "t.db"],
                Kind::File
            )
            .await,
            Some(Managed::Missing(_))
        ));
        // Wrong kind, escapes and separators are refused.
        assert_eq!(
            managed(
                profile.path(),
                &["antigravity-acp", "conversations"],
                Kind::File
            )
            .await,
            None
        );
        for bad in ["..", ".", "a/b", ""] {
            assert_eq!(
                managed(profile.path(), &["antigravity-acp", bad], Kind::Dir).await,
                None,
                "{bad}"
            );
        }
        // A linked parent sends nothing outside the profile.
        std::fs::create_dir_all(elsewhere.path().join("s")).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), acp.join("brain")).unwrap();
        assert_eq!(
            managed(
                profile.path(),
                &["antigravity-acp", "brain", "s"],
                Kind::Dir
            )
            .await,
            None
        );
    }
}
