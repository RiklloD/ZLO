//! The thread's folder as it travels with a remote delegation: where it is
//! on GitHub (its pushed base) plus what is only here (unpushed commits,
//! uncommitted and untracked files) as one binary patch. Best effort: any
//! failure leaves the field out, never the delegation.

use std::path::Path;

use base64::Engine as _;
use serde_json::{Value, json};

use crate::engine::Engine;
use crate::model::ThreadRecord;
use crate::worktree::git::Git;

/// A patch past this many bytes is left out (`omitted: "too_large"`).
pub(crate) const PATCH_MAX: usize = 8 * 1024 * 1024;
const SMALL: usize = 64 * 1024;

/// Untracked paths that look like secrets never travel.
pub(crate) const SECRET_EXCLUDES: [&str; 12] = [
    ":(exclude,glob)**/.env",
    ":(exclude,glob)**/.env.*",
    ":(exclude,glob)**/*.pem",
    ":(exclude,glob)**/*.key",
    ":(exclude,glob)**/*.p12",
    ":(exclude,glob)**/id_rsa*",
    ":(exclude,glob)**/id_ed25519*",
    ":(exclude,glob)**/.netrc",
    ":(exclude,glob)**/credentials.json",
    ":(exclude,glob)**/.npmrc",
    ":(exclude,glob)**/.pypirc",
    ":(exclude,glob)**/*.keystore",
];

/// The base: HEAD when every commit of it is on a remote-tracking ref, else
/// the parent of the oldest unpushed commit; `None` when nothing is pushed.
pub(crate) fn pushed_base(
    head: &str,
    unpushed_topo: &str,
    oldest_parent: Option<&str>,
) -> Option<String> {
    if unpushed_topo.split_whitespace().next().is_none() {
        return Some(head.to_string());
    }
    oldest_parent.map(str::to_string).filter(|p| !p.is_empty())
}

/// Files a `git diff` patch touches.
pub(crate) fn patch_files(patch: &[u8]) -> usize {
    patch
        .split(|b| *b == b'\n')
        .filter(|l| l.starts_with(b"diff --git "))
        .count()
}

fn text(out: &crate::worktree::git::Captured) -> Option<String> {
    (out.exit_code == Some(0)).then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

impl Engine {
    /// `launch.workspace` for a remote delegation from `parent`, or `None`
    /// when its folder is not a git repository (or git is unavailable).
    pub(crate) async fn peer_workspace(&self, parent: &ThreadRecord) -> Option<Value> {
        let host = self.worktree_host.get()?;
        let dir = self.thread_cwd(parent).await.ok()??;
        let fut = workspace_of(Git::new(host.runner.as_ref()), &dir);
        match tokio::time::timeout(std::time::Duration::from_secs(20), fut).await {
            Ok(v) => v,
            Err(_) => {
                tracing::debug!("peer workspace: git took too long");
                None
            }
        }
    }
}

pub(crate) async fn workspace_of(git: Git<'_>, dir: &Path) -> Option<Value> {
    let root = text(
        &git.capture(dir, &["rev-parse", "--show-toplevel"], SMALL)
            .await
            .ok()?,
    )?;
    let root = Path::new(&root).to_path_buf();
    let head = text(
        &git.capture(&root, &["rev-parse", "HEAD"], SMALL)
            .await
            .ok()?,
    );
    let branch = git
        .capture(
            &root,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
            SMALL,
        )
        .await
        .ok()
        .and_then(|o| text(&o))
        .filter(|b| !b.is_empty());
    let remote = match &branch {
        Some(b) => git
            .capture(
                &root,
                &["config", "--get", &format!("branch.{b}.remote")],
                SMALL,
            )
            .await
            .ok()
            .and_then(|o| text(&o))
            .filter(|r| !r.is_empty() && r != "."),
        None => None,
    }
    .unwrap_or_else(|| "origin".into());
    let origin = git
        .capture(&root, &["remote", "get-url", &remote], SMALL)
        .await
        .ok()
        .and_then(|o| text(&o))
        .filter(|u| !u.is_empty());
    let base = match &head {
        Some(head) => {
            let unpushed = git
                .capture(
                    &root,
                    &["rev-list", "--topo-order", "HEAD", "--not", "--remotes"],
                    PATCH_MAX,
                )
                .await
                .ok()
                .and_then(|o| text(&o))
                .unwrap_or_default();
            let oldest = unpushed.split_whitespace().last().map(str::to_string);
            let parent = match &oldest {
                Some(c) => git
                    .capture(
                        &root,
                        &["rev-parse", "--verify", "--quiet", &format!("{c}^")],
                        SMALL,
                    )
                    .await
                    .ok()
                    .and_then(|o| text(&o)),
                None => None,
            };
            pushed_base(head, &unpushed, parent.as_deref())
        }
        None => None, // no commit yet
    };
    let mut view = json!({
        "path": root.to_string_lossy(),
        "origin": origin,
        "branch": branch,
        "base_commit": base,
        "patch": Value::Null,
        "patch_bytes": 0,
        "files": 0,
        "omitted": Value::Null,
    });
    let Some(base) = base else {
        view["omitted"] = json!("no_pushed_base");
        return Some(view);
    };
    // A scratch index: the person's own index is never touched.
    let index = std::env::temp_dir().join(format!("zlo-peer-index-{}", uuid::Uuid::new_v4()));
    let patch = async {
        let ok = |o: crate::worktree::git::Captured| (o.exit_code == Some(0)).then_some(o);
        ok(git
            .capture_with_index(&root, Some(&index), &["read-tree", "HEAD"], SMALL)
            .await
            .ok()?)?;
        let mut add = vec!["add", "-A", "--", "."];
        add.extend(SECRET_EXCLUDES);
        ok(git
            .capture_with_index(&root, Some(&index), &add, SMALL)
            .await
            .ok()?)?;
        git.capture_with_index(
            &root,
            Some(&index),
            &["diff", "--cached", "--binary", &base],
            PATCH_MAX + 1,
        )
        .await
        .ok()
    }
    .await;
    let _removed = std::fs::remove_file(&index);
    let patch = patch?;
    if patch.truncated || patch.stdout.len() > PATCH_MAX {
        view["omitted"] = json!("too_large");
        return Some(view);
    }
    if patch.exit_code != Some(0) {
        return Some(view);
    }
    view["patch_bytes"] = json!(patch.stdout.len());
    view["files"] = json!(patch_files(&patch.stdout));
    if !patch.stdout.is_empty() {
        view["patch"] = json!(base64::engine::general_purpose::STANDARD.encode(&patch.stdout));
    }
    Some(view)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_is_head_when_everything_is_pushed() {
        assert_eq!(pushed_base("abc", "", None), Some("abc".into()));
        assert_eq!(pushed_base("abc", "  \n", None), Some("abc".into()));
    }

    #[test]
    fn base_is_parent_of_oldest_unpushed() {
        assert_eq!(pushed_base("c3", "c3\nc2\n", Some("c1")), Some("c1".into()));
        assert_eq!(pushed_base("c3", "c3\nc2\n", None), None);
    }

    #[test]
    fn counts_patched_files() {
        let p = b"diff --git a/x b/x\n+1\ndiff --git a/y b/y\nGIT binary patch\n";
        assert_eq!(patch_files(p), 2);
        assert_eq!(patch_files(b""), 0);
    }
}
