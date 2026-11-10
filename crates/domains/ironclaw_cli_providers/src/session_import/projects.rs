//! The folders terminal sessions ran in, for first-run setup's project list
//! (t3code's "Import your projects"). Read-only: each session file is read
//! only up to the line that names its folder, newest files first, within a
//! byte and time budget; nothing is imported or written.
//!
//! Sessions this app started, subagent and app-folder sessions are left out
//! as the import leaves them out; empty sessions are still counted, so a
//! folder's count is an estimate. A folder that is a git checkout carries
//! its `origin` remote so clones of one repository group together.

use std::collections::BTreeMap;

use ironclaw_product_contracts::session_import::{
    SessionImportGit, SessionImportProject, SessionImportProjects,
};
use serde_json::Value;

use super::*;

/// Bytes read from one file's start while looking for its folder.
const HEAD_BYTES: u64 = 2 * 1024 * 1024;
/// Lines read from one file's start while looking for its folder.
const HEAD_LINES: usize = 40;
/// Longest line kept while looking (a Codex meta line carries the prompt).
const HEAD_LINE_MAX: usize = 1024 * 1024;
/// The whole scan's budget.
const SCAN_BYTES: u64 = 128 * 1024 * 1024;
const SCAN_TIME: Duration = Duration::from_secs(20);
/// A git config read for the remote.
const GIT_CONFIG_MAX: u64 = 256 * 1024;

#[derive(Default)]
struct Folder {
    sources: BTreeSet<usize>,
    threads: u32,
    last: Option<Millis>,
    imported: bool,
}

/// What the start of a session file says.
#[derive(Default)]
struct Head {
    cwd: Option<String>,
    /// Codex: this app wrote it, or a subagent did.
    own: bool,
}

impl SessionImporter {
    /// The caller's session folders, newest first.
    pub async fn projects(
        &self,
        engine: &Engine,
        owner: &ProfileOwner,
    ) -> Result<SessionImportProjects, OrchestratorError> {
        let store = engine.store();
        let state = store.session_import_state(owner).await?;
        let mut budget = Budget {
            bytes: SCAN_BYTES,
            deadline: Instant::now() + SCAN_TIME,
        };
        let mut folders: BTreeMap<String, Folder> = BTreeMap::new();
        let mut truncated = false;
        for (ix, spec) in SOURCES.iter().enumerate() {
            let runtime = RuntimeId::new(spec.runtime)?;
            if !state.settings.enabled(spec.id) || !engine.has_runtime(&runtime) {
                continue;
            }
            let mut own = store
                .native_sessions(std::slice::from_ref(&runtime))
                .await?;
            if spec.id == CODEX {
                own.extend(self.codex_thread_ids().await);
            }
            let ledger = store.import_ledger(owner, spec.id).await?;
            let mut candidates = match spec.id {
                CLAUDE_CODE => self.claude_candidates().await,
                _ => self.codex_candidates().await,
            };
            if candidates.len() >= self.limits.files {
                truncated = true;
            }
            candidates.sort_by_key(|c| std::cmp::Reverse(c.file.mtime_ms));
            for c in candidates {
                // Imported threads carry their session's id too; only the
                // app's own sessions are left out.
                if own.contains(&c.native_id) && !ledger.contains_key(&c.native_id) {
                    continue;
                }
                if budget.spent() {
                    truncated = true;
                    break;
                }
                let head = match self.head(&c, spec.id, &mut budget).await {
                    Ok(head) => head,
                    Err(error) => {
                        tracing::debug!(%error, source = spec.id, "session folder unreadable");
                        continue;
                    }
                };
                let Some(cwd) = head.cwd else { continue };
                if head.own || self.own_folder(Some(Path::new(&cwd))) {
                    continue;
                }
                let folder = folders.entry(cwd).or_default();
                folder.sources.insert(ix);
                folder.threads += 1;
                folder.last = Some(
                    folder
                        .last
                        .map_or(c.file.mtime_ms, |l| l.max(c.file.mtime_ms)),
                );
                folder.imported |= ledger.contains_key(&c.native_id);
                tokio::task::yield_now().await;
            }
        }
        let mut projects = Vec::with_capacity(folders.len());
        for (path, folder) in folders {
            let git = git_checkout(Path::new(&path)).await;
            projects.push(SessionImportProject {
                title: title_of(&path),
                sources: folder
                    .sources
                    .iter()
                    .map(|ix| SOURCES[*ix].id.to_string())
                    .collect(),
                thread_count: folder.threads,
                last_active_at: folder.last,
                git,
                imported: folder.imported,
                path,
            });
        }
        projects.sort_by(|a, b| {
            b.last_active_at
                .cmp(&a.last_active_at)
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(SessionImportProjects {
            projects,
            truncated,
        })
    }

    /// The folder a session ran in, from its first lines.
    async fn head(
        &self,
        c: &Candidate,
        source: &str,
        budget: &mut Budget,
    ) -> Result<Head, ImportError> {
        let file = self.open(c).await?;
        let limit = c.file.size.min(HEAD_BYTES);
        let mut lines = lines::Lines::from_file(file, 0, limit, HEAD_LINE_MAX).await?;
        let mut head = Head::default();
        for _ in 0..HEAD_LINES {
            let Some(line) = lines.next_line().await? else {
                break;
            };
            let Ok(v) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            if source == CLAUDE_CODE {
                // As the import reads it: the first non-empty folder.
                head.cwd = v
                    .get("cwd")
                    .and_then(Value::as_str)
                    .filter(|c| !c.is_empty())
                    .map(str::to_string);
            } else {
                head.own |= codex_own(&v);
                head.cwd = codex_cwd(&v);
            }
            if head.cwd.is_some() {
                break;
            }
        }
        budget.take(lines.end().max(1));
        Ok(head)
    }
}

/// The folder a Codex line names (`session_meta` or `turn_context`).
fn codex_cwd(v: &Value) -> Option<String> {
    match v.get("type").and_then(Value::as_str) {
        Some("session_meta") | Some("turn_context") => v["payload"]
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .map(str::to_string),
        // Older layout: a bare meta line first.
        _ if v.get("payload").is_none() => v.get("cwd").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// A Codex meta line written by this app, or by a subagent.
fn codex_own(v: &Value) -> bool {
    if v.get("type").and_then(Value::as_str) != Some("session_meta") {
        return false;
    }
    let meta = &v["payload"];
    let originator = meta
        .get("originator")
        .and_then(Value::as_str)
        .is_some_and(|o| o.eq_ignore_ascii_case("ironclaw"));
    let subagent = meta
        .get("source")
        .is_some_and(|s| s.get("subagent").is_some());
    originator || subagent
}

fn title_of(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| path.to_string())
}

/// The folder's git checkout and its `origin` remote; `None` when it is not
/// a checkout (or is gone). Worktrees read their main checkout's config.
async fn git_checkout(dir: &Path) -> Option<SessionImportGit> {
    let dot_git = dir.join(".git");
    let meta = tokio::fs::symlink_metadata(&dot_git).await.ok()?; // silent-ok: no .git means not a checkout
    let git_dir = if meta.is_dir() {
        dot_git
    } else if meta.is_file() {
        let text = read_small(&dot_git).await?;
        let target = text.trim().strip_prefix("gitdir:")?.trim();
        let worktree = dir.join(target);
        match read_small(&worktree.join("commondir")).await {
            Some(common) => worktree.join(common.trim()),
            None => worktree,
        }
    } else {
        return None;
    };
    let url = read_small(&git_dir.join("config"))
        .await
        .and_then(|config| origin_url(&config));
    let remote_key = url.as_deref().and_then(remote_key);
    let repository = remote_key.as_deref().and_then(repository_of);
    Some(SessionImportGit {
        repository,
        remote_key,
    })
}

async fn read_small(path: &Path) -> Option<String> {
    let meta = tokio::fs::metadata(path).await.ok()?; // silent-ok: optional git file
    if !meta.is_file() || meta.len() > GIT_CONFIG_MAX {
        return None;
    }
    tokio::fs::read_to_string(path).await.ok() // silent-ok: optional git file
}

/// The `origin` remote's url, else the first remote's.
fn origin_url(config: &str) -> Option<String> {
    let mut section: Option<String> = None;
    let mut first: Option<String> = None;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line
                .strip_prefix("[remote \"")
                .and_then(|r| r.strip_suffix("\"]"))
                .map(str::to_string);
            continue;
        }
        let Some(name) = &section else { continue };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "url" {
            continue;
        }
        let value = value.trim().to_string();
        if name == "origin" {
            return Some(value);
        }
        first.get_or_insert(value);
    }
    first
}

/// `https://user@example.com/o/r.git`, `git@example.com:o/r.git` and
/// `ssh://git@example.com/o/r` all become `example.com/o/r`.
fn remote_key(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest.to_string(),
        // scp-like `user@host:path`.
        None => {
            let (host, path) = url.split_once(':')?;
            format!("{host}/{path}")
        }
    };
    let rest = rest.rsplit_once('@').map_or(rest.as_str(), |(_, r)| r);
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    // A port on an ssh url (`host:22/o/r`) is not part of the identity.
    let (host, path) = rest.split_once('/')?;
    let host = host.split(':').next().unwrap_or(host).to_lowercase();
    let path = path.trim_matches('/');
    (!host.is_empty() && !path.is_empty()).then(|| format!("{host}/{path}"))
}

/// `owner/name`: the remote key's last two segments.
fn repository_of(key: &str) -> Option<String> {
    let mut parts = key.rsplit('/');
    let name = parts.next()?;
    let owner = parts.next()?;
    parts.next()?;
    Some(format!("{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remotes_of_one_repository_share_a_key() {
        for url in [
            "https://example.com/acme/app.git",
            "https://user:pw@Example.com/acme/app",
            "git@example.com:acme/app.git",
            "ssh://git@example.com:22/acme/app/",
        ] {
            assert_eq!(
                remote_key(url).as_deref(),
                Some("example.com/acme/app"),
                "{url}"
            );
        }
        assert_eq!(
            repository_of("example.com/acme/app").as_deref(),
            Some("acme/app")
        );
        assert_eq!(
            repository_of("gitlab.com/g/sub/app").as_deref(),
            Some("sub/app")
        );
        assert_eq!(remote_key("not a url"), None);
    }

    #[test]
    fn origin_wins_over_other_remotes() {
        let config = "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = git@example.com:up/app.git\n[remote \"origin\"]\n\turl = https://example.com/me/app.git\n";
        assert_eq!(
            origin_url(config).as_deref(),
            Some("https://example.com/me/app.git")
        );
        let only = "[remote \"fork\"]\n\turl = git@example.com:me/x.git\n";
        assert_eq!(origin_url(only).as_deref(), Some("git@example.com:me/x.git"));
        assert_eq!(origin_url("[core]\n"), None);
    }

    #[test]
    fn codex_lines_name_their_folder_and_origin() {
        let meta: Value = serde_json::from_str(
            r#"{"type":"session_meta","payload":{"cwd":"/work/app","originator":"codex_cli_rs"}}"#,
        )
        .unwrap();
        assert_eq!(codex_cwd(&meta).as_deref(), Some("/work/app"));
        assert!(!codex_own(&meta));
        let own: Value = serde_json::from_str(
            r#"{"type":"session_meta","payload":{"cwd":"/w","originator":"IronClaw"}}"#,
        )
        .unwrap();
        assert!(codex_own(&own));
        let sub: Value = serde_json::from_str(
            r#"{"type":"session_meta","payload":{"cwd":"/w","source":{"subagent":"review"}}}"#,
        )
        .unwrap();
        assert!(codex_own(&sub));
        assert_eq!(title_of("/work/app"), "app");
        assert_eq!(title_of("/"), "/");
    }
}
