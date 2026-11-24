//! The source cache: one folder per commit under `<ZLO_HOME>/source`, the
//! layout the playbook tells agents to use, so an in-app session finds and
//! reuses the backend's fetch instead of making another.
//!
//! ZLO has no release tags. The ref is the commit this binary was built from
//! (`build.rs` stamps `IRONCLAW_BUILD_COMMIT`); a build that records none
//! (a release tarball) tracks `main`, looked up with `git ls-remote` under a
//! short timeout so a dead network never stalls the session. A fetch runs in
//! the background into a staging folder beside the cache and appears in the
//! cache by rename only once complete, so a half-done fetch never looks like
//! a usable checkout (or like a stale entry the playbook would prune).
//! Sessions started while a commit is fetching share that fetch.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use crate::triage::prompt::{SourceClone, SourceRef, SourceRefKind, ZLO_REPOSITORY_URL};

const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(5);
const LOCAL_GIT_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_TIMEOUT: Duration = Duration::from_secs(600);

/// The source cache folder under a ZLO home.
pub(crate) fn source_cache_dir(home: &Path) -> PathBuf {
    home.join("source")
}

/// Picks the build's own commit, else `main` from `ls-remote` output.
pub(crate) fn pick_source_ref(ls_remote: &str, build_commit: Option<&str>) -> Option<SourceRef> {
    if let Some(commit) = build_commit {
        return Some(SourceRef {
            kind: SourceRefKind::BuildCommit,
            commit: commit.to_string(),
        });
    }
    ls_remote.lines().find_map(|line| {
        let (commit, name) = line.trim().split_once('\t')?;
        (name == "refs/heads/main" && !commit.is_empty()).then(|| SourceRef {
            kind: SourceRefKind::Main,
            commit: commit.to_string(),
        })
    })
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum GitError {
    #[error("`git {operation}` could not start: {reason}")]
    Spawn { operation: String, reason: String },
    #[error("`git {operation}` timed out after {seconds}s")]
    TimedOut { operation: String, seconds: u64 },
    #[error("`git {operation}` failed: {stderr}")]
    Failed { operation: String, stderr: String },
}

/// Runs `git`. A port so tests can answer each command; production runs the
/// real binary with [`git_env`].
#[async_trait]
pub(crate) trait GitRunner: Send + Sync {
    async fn run(&self, cwd: &Path, args: &[&str], timeout: Duration) -> Result<String, GitError>;
}

/// No tty: a credential prompt would hang until the timeout, so git must
/// fail instead. An empty `GIT_ASKPASS` also skips `core.askPass` and
/// `SSH_ASKPASS` helpers; `LANGUAGE=C` keeps messages English without
/// overriding `LC_CTYPE`, which a global hook inherits.
pub(crate) fn git_env() -> [(&'static str, &'static str); 3] {
    [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_ASKPASS", ""),
        ("LANGUAGE", "C"),
    ]
}

pub(crate) struct ProcessGit;

#[async_trait]
impl GitRunner for ProcessGit {
    async fn run(&self, cwd: &Path, args: &[&str], timeout: Duration) -> Result<String, GitError> {
        let operation = args.first().copied().unwrap_or_default().to_string();
        let mut command = tokio::process::Command::new("git");
        command
            .args(args)
            .current_dir(cwd)
            .envs(git_env())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = match tokio::time::timeout(timeout, command.output()).await {
            Err(_elapsed) => {
                return Err(GitError::TimedOut {
                    operation,
                    seconds: timeout.as_secs(),
                });
            }
            Ok(Err(error)) => {
                return Err(GitError::Spawn {
                    operation,
                    reason: error.to_string(),
                });
            }
            Ok(Ok(output)) => output,
        };
        if !output.status.success() {
            return Err(GitError::Failed {
                operation,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// The backend's source cache: resolves its build's ref and fetches it in
/// the background. One per process.
pub(crate) struct SourceCache {
    dir: PathBuf,
    /// Beside the cache, not in it: the playbook has agents prune the
    /// cache, and a fetch in progress must not look like a stale entry.
    /// Same filesystem, so the finished fetch moves in with a rename.
    staging: PathBuf,
    git: Arc<dyn GitRunner>,
    /// The build this backend runs, as the context file names it.
    build: String,
    build_commit: Option<String>,
    /// Commits with a fetch running, so sessions started meanwhile share it.
    cloning: Arc<Mutex<HashSet<String>>>,
}

impl SourceCache {
    pub(crate) fn new(
        home: &Path,
        git: Arc<dyn GitRunner>,
        build: String,
        build_commit: Option<String>,
    ) -> Self {
        let dir = source_cache_dir(home);
        let staging = home.join("source-staging");
        // Fetches a previous backend process left half done.
        if let Err(error) = std::fs::remove_dir_all(&staging)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(%error, staging = %staging.display(), "triage: stale source staging not removed");
        }
        Self {
            dir,
            staging,
            git,
            build,
            build_commit,
            cloning: Arc::default(),
        }
    }

    /// The source for this session: already fetched, fetching (started now
    /// or by an earlier session), or why not. Never fails the session.
    pub(crate) async fn start(&self) -> SourceClone {
        match self.try_start().await {
            Ok(clone) => clone,
            Err(reason) => SourceClone::Failed {
                build: self.build.clone(),
                reason,
            },
        }
    }

    async fn try_start(&self) -> Result<SourceClone, String> {
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|error| format!("could not create {}: {error}", self.dir.display()))?;
        let listed = match self.build_commit {
            // The build's own commit never moves: no lookup needed.
            Some(_) => String::new(),
            None => self
                .git
                .run(
                    &self.dir,
                    &["ls-remote", ZLO_REPOSITORY_URL, "refs/heads/main"],
                    LS_REMOTE_TIMEOUT,
                )
                .await
                .map_err(|error| error.to_string())?,
        };
        let Some(source) = pick_source_ref(&listed, self.build_commit.as_deref()) else {
            return Err("the repository listed no main branch.".into());
        };
        let directory = self.dir.join(&source.commit);
        let build = self.build.clone();
        // silent-ok: an unreadable cache entry is treated as absent and refetched
        if tokio::fs::try_exists(&directory).await.unwrap_or(false) {
            // Our fetches appear only when complete, but an agent may have
            // cloned here by hand and stopped partway. Without its own .git,
            // rev-parse would read a parent repository.
            let has_git = tokio::fs::try_exists(directory.join(".git"))
                .await
                .unwrap_or(false); // silent-ok: no readable .git means not a checkout
            let head = if has_git {
                self.git
                    .run(&directory, &["rev-parse", "HEAD"], LOCAL_GIT_TIMEOUT)
                    .await
                    .map(|out| out.trim().to_string())
                    .unwrap_or_default() // silent-ok: an unreadable HEAD reads as "not a complete checkout" below
            } else {
                String::new()
            };
            if head == source.commit {
                return Ok(SourceClone::Cloned {
                    directory,
                    source,
                    build,
                });
            }
            return Err(format!(
                "{} exists but is not a complete checkout of {}. Remove it first.",
                directory.display(),
                source.commit
            ));
        }
        let first = self
            .cloning
            .lock()
            .map_err(|_poisoned| "the source cache lock is poisoned".to_string())?
            .insert(source.commit.clone());
        if first {
            let fetch = Fetch {
                staging: self.staging.clone(),
                directory: directory.clone(),
                commit: source.commit.clone(),
                git: Arc::clone(&self.git),
            };
            let cloning = Arc::clone(&self.cloning);
            // Outlives the request that started it: a fresh fetch takes tens
            // of seconds, and the user is writing their description meanwhile.
            tokio::spawn(async move {
                if let Err(error) = fetch.run().await {
                    tracing::debug!(%error, commit = %fetch.commit, "triage: source fetch failed");
                }
                if let Ok(mut set) = cloning.lock() {
                    set.remove(&fetch.commit);
                }
            });
        }
        Ok(SourceClone::Cloning {
            directory,
            source,
            build,
        })
    }
}

/// One background fetch of one commit.
struct Fetch {
    staging: PathBuf,
    directory: PathBuf,
    commit: String,
    git: Arc<dyn GitRunner>,
}

impl Fetch {
    async fn run(&self) -> Result<(), String> {
        let work = self.staging.join(format!("clone-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&work)
            .await
            .map_err(|error| format!("create {}: {error}", work.display()))?;
        let result = self.fetch_into(&work).await;
        // Gone after a successful rename; otherwise a failed or losing fetch.
        if let Err(error) = tokio::fs::remove_dir_all(&work).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(%error, work = %work.display(), "triage: staging folder not removed");
        }
        result
    }

    /// Fetches the exact commit, so the folder a session names is the one
    /// that appears even when `main` moves meanwhile.
    async fn fetch_into(&self, work: &Path) -> Result<(), String> {
        let steps: [(&[&str], Duration); 4] = [
            (&["init", "--quiet"], LOCAL_GIT_TIMEOUT),
            (
                &["remote", "add", "origin", ZLO_REPOSITORY_URL],
                LOCAL_GIT_TIMEOUT,
            ),
            (
                &[
                    "fetch",
                    "--depth",
                    "1",
                    "--filter=blob:none",
                    "origin",
                    &self.commit,
                ],
                FETCH_TIMEOUT,
            ),
            (&["checkout", "--quiet", "FETCH_HEAD"], FETCH_TIMEOUT),
        ];
        for (args, timeout) in steps {
            self.git
                .run(work, args, timeout)
                .await
                .map_err(|error| error.to_string())?;
        }
        match tokio::fs::rename(work, &self.directory).await {
            Ok(()) => Ok(()),
            // A concurrent fetch of the same commit got there first; keep it.
            // silent-ok: the rename error is moot once the folder exists
            Err(_)
                if tokio::fs::try_exists(&self.directory)
                    .await
                    .unwrap_or(false) =>
            {
                Ok(())
            }
            Err(error) => Err(format!(
                "move the fetch into {}: {error}",
                self.directory.display()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: &str = "a1dc856ee61278d0199c36d4bcdef5fdea4d21fe";

    #[test]
    fn picks_the_build_commit_when_the_build_records_one() {
        let listed = format!("{MAIN}\trefs/heads/main\n");
        assert_eq!(
            pick_source_ref(&listed, Some("7027b1f")),
            Some(SourceRef {
                kind: SourceRefKind::BuildCommit,
                commit: "7027b1f".into(),
            })
        );
    }

    #[test]
    fn falls_back_to_main_and_to_nothing() {
        let listed = format!("1111111\trefs/heads/mainline\n{MAIN}\trefs/heads/main\n");
        assert_eq!(
            pick_source_ref(&listed, None),
            Some(SourceRef {
                kind: SourceRefKind::Main,
                commit: MAIN.into(),
            })
        );
        assert_eq!(pick_source_ref("", None), None);
        assert_eq!(pick_source_ref("garbage\n", None), None);
    }

    /// Answers each git command; `fetch` can be held until released.
    struct StubGit {
        commands: Mutex<Vec<String>>,
        head: String,
        hold_fetch: Option<tokio::sync::Semaphore>,
        fail_ls_remote: bool,
    }

    impl StubGit {
        fn new(head: &str) -> Self {
            Self {
                commands: Mutex::new(Vec::new()),
                head: head.into(),
                hold_fetch: None,
                fail_ls_remote: false,
            }
        }
        fn commands(&self) -> Vec<String> {
            self.commands.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl GitRunner for StubGit {
        async fn run(&self, _cwd: &Path, args: &[&str], _t: Duration) -> Result<String, GitError> {
            let command = args[0].to_string();
            self.commands.lock().expect("lock").push(command.clone());
            match command.as_str() {
                "ls-remote" if self.fail_ls_remote => Err(GitError::Failed {
                    operation: command,
                    stderr: "Could not resolve host: github.com".into(),
                }),
                "ls-remote" => Ok(format!("{MAIN}\trefs/heads/main\n")),
                "rev-parse" => Ok(format!("{}\n", self.head)),
                "fetch" => {
                    if let Some(gate) = &self.hold_fetch {
                        let _permit = gate.acquire().await;
                    }
                    Ok(String::new())
                }
                _ => Ok(String::new()),
            }
        }
    }

    fn cache(home: &Path, git: Arc<StubGit>, commit: Option<&str>) -> SourceCache {
        SourceCache::new(home, git, "1.2.0".into(), commit.map(str::to_string))
    }

    #[tokio::test]
    async fn sessions_started_during_a_fetch_share_it() {
        let home = tempfile::tempdir().expect("home");
        let mut stub = StubGit::new("");
        stub.hold_fetch = Some(tokio::sync::Semaphore::new(0));
        let git = Arc::new(stub);
        let cache = cache(home.path(), Arc::clone(&git), Some("7027b1f"));

        let first = cache.start().await;
        let second = cache.start().await;
        assert!(matches!(first, SourceClone::Cloning { .. }), "{first:?}");
        assert_eq!(first, second);
        // Let the spawned fetch reach `fetch`.
        for _ in 0..50 {
            if git.commands().iter().any(|c| c == "fetch") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let commands = git.commands();
        assert_eq!(commands.iter().filter(|c| *c == "init").count(), 1);
        assert!(
            !commands.iter().any(|c| c == "ls-remote"),
            "the build commit needs no lookup"
        );
    }

    #[tokio::test]
    async fn a_finished_fetch_appears_in_the_cache_by_rename() {
        let home = tempfile::tempdir().expect("home");
        let git = Arc::new(StubGit::new(""));
        let cache = cache(home.path(), Arc::clone(&git), None);
        let SourceClone::Cloning {
            directory, source, ..
        } = cache.start().await
        else {
            panic!("expected a fetch to start");
        };
        assert_eq!(source.kind, SourceRefKind::Main);
        assert_eq!(directory, home.path().join("source").join(MAIN));
        for _ in 0..100 {
            if directory.is_dir() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(directory.is_dir(), "the staged fetch moved in");
        let staged = std::fs::read_dir(home.path().join("source-staging"))
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(staged, 0, "staging is empty afterwards");
    }

    #[tokio::test]
    async fn reuses_a_finished_checkout_without_fetching() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join("source/7027b1f/.git")).expect("seed");
        let git = Arc::new(StubGit::new("7027b1f"));
        let cache = cache(home.path(), Arc::clone(&git), Some("7027b1f"));
        assert!(matches!(cache.start().await, SourceClone::Cloned { .. }));
        assert_eq!(git.commands(), vec!["rev-parse"]);
    }

    #[tokio::test]
    async fn does_not_trust_a_folder_without_its_own_git() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join("source/7027b1f")).expect("seed");
        // The stub would answer rev-parse with the commit, as a parent repo could.
        let git = Arc::new(StubGit::new("7027b1f"));
        let cache = cache(home.path(), Arc::clone(&git), Some("7027b1f"));
        let SourceClone::Failed { reason, .. } = cache.start().await else {
            panic!("expected a failure");
        };
        assert!(reason.contains("is not a complete checkout"), "{reason}");
        assert!(!git.commands().iter().any(|c| c == "rev-parse"));
    }

    #[tokio::test]
    async fn a_failed_lookup_still_answers_with_the_reason() {
        let home = tempfile::tempdir().expect("home");
        let mut stub = StubGit::new("");
        stub.fail_ls_remote = true;
        let cache = cache(home.path(), Arc::new(stub), None);
        let SourceClone::Failed { reason, build } = cache.start().await else {
            panic!("expected a failure");
        };
        assert_eq!(build, "1.2.0");
        assert!(reason.contains("Could not resolve host"), "{reason}");
    }

    #[tokio::test]
    async fn a_half_done_staging_folder_is_cleared_on_start() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join("source-staging/clone-old")).expect("seed");
        let _cache = cache(home.path(), Arc::new(StubGit::new("")), None);
        assert!(!home.path().join("source-staging").exists());
    }
}
