//! The source cache: one folder per commit under `<ZLO_HOME>/source`, the
//! layout the playbook tells agents to use, so an in-app session finds and
//! reuses the backend's fetch instead of making another.
//!
//! ZLO has no release tags. The ref is the commit this binary was built from
//! (`build.rs` stamps `IRONCLAW_BUILD_COMMIT`); a build that records none
//! (a release tarball) tracks `main`, looked up with `git ls-remote` under a
//! short timeout so a dead network never stalls the session. Either commit
//! must be a full hash before it names a folder or reaches `git` as an
//! argument.
//!
//! A fetch runs in the background into a staging folder beside the cache
//! and appears in the cache by rename only once complete and verified, so a
//! half-done fetch never looks like a usable checkout (or like a stale entry
//! the playbook would prune). Fetches of one commit are coordinated across
//! backend processes by a lock file in the staging folder: a session started
//! while any process fetches that commit shares the fetch. A new process
//! reclaims only staging folders whose owner is gone or that are older than
//! any fetch can run.
//!
//! Git runs isolated ([`ProcessGit`]): no inherited repository routing, no
//! user or system config, no hooks, no credential helpers or prompts, https
//! only, and the whole process group is killed on a timeout or a drop. Its
//! stderr is only logged (debug): what reaches a session's context is a
//! typed [`SourceFailure`], because git's output can name credentials.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;

use crate::commands::update::is_full_hash;
use crate::triage::prompt::{SourceClone, SourceRef, SourceRefKind, ZLO_REPOSITORY_URL};

const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(5);
const LOCAL_GIT_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_TIMEOUT: Duration = Duration::from_secs(600);
/// A staging folder this old outlived any fetch (every step is bounded),
/// so its owner is gone even if its pid was reused.
const STAGING_ABANDONED_AFTER: Duration = Duration::from_secs(2 * 600 + 60);

/// The source cache folder under a ZLO home.
pub(crate) fn source_cache_dir(home: &Path) -> PathBuf {
    home.join("source")
}

/// Beside the cache, not in it: the playbook has agents prune the cache,
/// and a fetch in progress (or its lock) must not look like a stale entry.
/// Same filesystem, so the finished fetch moves in with a rename.
fn staging_dir(home: &Path) -> PathBuf {
    home.join("source-staging")
}

/// Why the backend has no source for a session. Rendered into the context
/// file as a fixed sentence: git's own output never is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SourceFailure {
    /// The cache or staging folder could not be created.
    CacheUnwritable {
        directory: PathBuf,
    },
    /// `git` could not be started.
    GitUnavailable,
    /// GitHub could not be reached.
    Network,
    TimedOut {
        operation: String,
        seconds: u64,
    },
    /// `git` ran and failed for another reason.
    GitFailed {
        operation: String,
    },
    /// The repository listed no `main`.
    NoMainBranch,
    /// A commit that is not a full lowercase hash.
    InvalidCommit,
    /// The cache folder for the commit holds something else.
    NotACheckout {
        directory: PathBuf,
        commit: String,
    },
}

impl SourceFailure {
    /// The context file's sentence.
    pub(crate) fn summary(&self) -> String {
        match self {
            Self::CacheUnwritable { directory } => {
                format!("the folder {} could not be created.", directory.display())
            }
            Self::GitUnavailable => "git could not be started (is it installed?).".into(),
            Self::Network => {
                "GitHub could not be reached (no network, or its name did not resolve).".into()
            }
            Self::TimedOut { operation, seconds } => {
                format!("`git {operation}` timed out after {seconds}s.")
            }
            Self::GitFailed { operation } => format!(
                "`git {operation}` failed. Its output is not copied here, since it can name \
                 credentials."
            ),
            Self::NoMainBranch => "the repository listed no main branch.".into(),
            Self::InvalidCommit => "the commit to fetch is not a full commit hash.".into(),
            Self::NotACheckout { directory, commit } => format!(
                "{} exists but is not a complete checkout of {commit}. Remove it first.",
                directory.display()
            ),
        }
    }
}

/// Picks the build's own commit, else `main` from `ls-remote` output. Only
/// a full hash is ever picked: it becomes a folder name and a git argument.
pub(crate) fn pick_source_ref(
    ls_remote: &str,
    build_commit: Option<&str>,
) -> Result<SourceRef, SourceFailure> {
    if let Some(commit) = build_commit {
        if !is_full_hash(commit) {
            return Err(SourceFailure::InvalidCommit);
        }
        return Ok(SourceRef {
            kind: SourceRefKind::BuildCommit,
            commit: commit.to_string(),
        });
    }
    let main = ls_remote.lines().find_map(|line| {
        let (commit, name) = line.trim().split_once('\t')?;
        (name == "refs/heads/main").then_some(commit)
    });
    match main {
        None => Err(SourceFailure::NoMainBranch),
        Some(commit) if !is_full_hash(commit) => Err(SourceFailure::InvalidCommit),
        Some(commit) => Ok(SourceRef {
            kind: SourceRefKind::Main,
            commit: commit.to_string(),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum GitError {
    #[error("`git {operation}` could not start: {reason}")]
    Spawn { operation: String, reason: String },
    #[error("`git {operation}` timed out after {seconds}s")]
    TimedOut { operation: String, seconds: u64 },
    /// `stderr` is for the debug log only (see [`SourceFailure`]).
    #[error("`git {operation}` failed: {stderr}")]
    Failed { operation: String, stderr: String },
}

impl GitError {
    /// The typed failure a session's context may show.
    fn failure(&self) -> SourceFailure {
        match self {
            Self::Spawn { .. } => SourceFailure::GitUnavailable,
            Self::TimedOut { operation, seconds } => SourceFailure::TimedOut {
                operation: operation.clone(),
                seconds: *seconds,
            },
            Self::Failed { stderr, .. } if is_network_error(stderr) => SourceFailure::Network,
            Self::Failed { operation, .. } => SourceFailure::GitFailed {
                operation: operation.clone(),
            },
        }
    }
}

fn is_network_error(stderr: &str) -> bool {
    [
        "Could not resolve host",
        "Failed to connect",
        "Connection refused",
        "Network is unreachable",
        "Connection timed out",
        "Operation timed out",
    ]
    .iter()
    .any(|needle| stderr.contains(needle))
}

/// Logs the full error (debug) and answers its typed failure.
fn logged(error: GitError) -> SourceFailure {
    tracing::debug!(%error, "triage: source git step failed");
    error.failure()
}

/// Runs `git`. A port so tests can answer each command; production runs the
/// real binary isolated ([`ProcessGit`]).
#[async_trait]
pub(crate) trait GitRunner: Send + Sync {
    async fn run(&self, cwd: &Path, args: &[&str], timeout: Duration) -> Result<String, GitError>;
}

/// `/dev/null`, or Windows' `NUL`.
const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// Config every git run gets ahead of its own arguments: hooks off (a
/// template or global `core.hooksPath` could name programs), no credential
/// helpers or askpass, no fsmonitor, and only https as a transport.
pub(crate) fn git_config_args() -> Vec<String> {
    [
        format!("core.hooksPath={NULL_DEVICE}"),
        "credential.helper=".into(),
        "core.askPass=".into(),
        "core.fsmonitor=false".into(),
        "protocol.allow=never".into(),
        "protocol.https.allow=always".into(),
    ]
    .into_iter()
    .flat_map(|setting| ["-c".to_string(), setting])
    .collect()
}

/// Inherited variables git must not see: everything `GIT_*` (repository
/// routing such as `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`/
/// `GIT_OBJECT_DIRECTORY`, injected config `GIT_CONFIG*`, `GIT_SSH*`,
/// `GIT_EXEC_PATH`, …) and the askpass helpers.
fn is_scrubbed(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with("GIT_") || name == "SSH_ASKPASS" || name == "SSH_ASKPASS_REQUIRE"
}

/// The whole environment a git run gets: `inherited` without
/// [`is_scrubbed`] names, plus isolation of our own. No system or global
/// config; no prompt (a credential prompt would hang until the timeout, so
/// git must fail instead); an empty `GIT_ASKPASS`/`SSH_ASKPASS` skips
/// helpers; https only; no repository discovery above `cwd`, so a folder
/// without its own `.git` never reads a parent repository. `LANGUAGE=C`
/// keeps messages English for [`is_network_error`].
pub(crate) fn git_env(
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
    cwd: &Path,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = inherited
        .into_iter()
        .filter(|(name, _)| !is_scrubbed(name))
        .collect();
    let ceiling = cwd.parent().unwrap_or(cwd).as_os_str().to_os_string();
    let ours: [(&str, OsString); 10] = [
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ("GIT_CONFIG_GLOBAL", NULL_DEVICE.into()),
        ("GIT_TERMINAL_PROMPT", "0".into()),
        ("GIT_ASKPASS", OsString::new()),
        ("SSH_ASKPASS", OsString::new()),
        ("GIT_ALLOW_PROTOCOL", "https".into()),
        ("GIT_PROTOCOL_FROM_USER", "0".into()),
        ("GIT_CEILING_DIRECTORIES", ceiling),
        ("GIT_NO_REPLACE_OBJECTS", "1".into()),
        ("LANGUAGE", "C".into()),
    ];
    env.retain(|(name, _)| !ours.iter().any(|(own, _)| name == own));
    env.extend(ours.into_iter().map(|(name, value)| (name.into(), value)));
    env
}

/// The real `git`, run isolated.
pub(crate) struct ProcessGit {
    program: OsString,
    /// The environment git starts from (before [`git_env`]); `None` reads
    /// this process's at each run.
    inherited: Option<Vec<(OsString, OsString)>>,
}

impl ProcessGit {
    /// `git` from `PATH`, from this process's environment.
    pub(crate) fn system() -> Self {
        Self {
            program: "git".into(),
            inherited: None,
        }
    }
}

/// Kills the process group a git run leads when dropped while armed: when it
/// times out or the request awaiting it goes away. Stray children
/// (`git-remote-https`, a helper) die with it. Disarmed once git has exited
/// and been reaped, so a reused pid is never signalled.
struct ProcessGroup(Option<u32>);

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pgid) = self.0.and_then(|id| libc::pid_t::try_from(id).ok()) {
            // SAFETY: `kill` takes plain integers; a negative pid names the
            // process group this git leads (`process_group(0)` at spawn), so
            // nothing outside that group is signalled.
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
}

#[async_trait]
impl GitRunner for ProcessGit {
    async fn run(&self, cwd: &Path, args: &[&str], timeout: Duration) -> Result<String, GitError> {
        let operation = args.first().copied().unwrap_or_default().to_string();
        let inherited = match &self.inherited {
            Some(env) => env.clone(),
            None => std::env::vars_os().collect(),
        };
        let mut command = tokio::process::Command::new(&self.program);
        command
            .env_clear()
            .envs(git_env(inherited, cwd))
            .args(git_config_args())
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command.spawn().map_err(|error| GitError::Spawn {
            operation: operation.clone(),
            reason: error.to_string(),
        })?;
        let mut group = ProcessGroup(child.id());
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Err(_elapsed) => {
                return Err(GitError::TimedOut {
                    operation,
                    seconds: timeout.as_secs(),
                });
            }
            Ok(Err(error)) => {
                group.0 = None;
                return Err(GitError::Spawn {
                    operation,
                    reason: error.to_string(),
                });
            }
            Ok(Ok(output)) => {
                // Exited and reaped (its pipes closed, so its children too).
                group.0 = None;
                output
            }
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

/// Whether `pid` still runs (a staging folder's owner).
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: signal 0 only checks the pid exists and may be signalled.
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        // Unknown: only age reclaims a folder here.
        let _ = pid;
        true
    }
}

/// `clone-<pid>-<uuid>`: the staging folder of one fetch by process `pid`.
fn staging_name(pid: u32) -> String {
    format!("clone-{pid}-{}", uuid::Uuid::new_v4().simple())
}

fn staging_owner(name: &str) -> Option<u32> {
    name.strip_prefix("clone-")?.split('-').next()?.parse().ok()
}

/// Remove staging folders no live fetch owns: their process is gone, or
/// they are older than any fetch can run. Lock files stay (a live fetch
/// holds its lock; an unheld one is reused).
pub(crate) fn reclaim_abandoned_staging(
    staging: &Path,
    now: SystemTime,
    alive: impl Fn(u32) -> bool,
) {
    let Ok(entries) = std::fs::read_dir(staging) else {
        return; // silent-ok: no staging folder means nothing to reclaim
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".lock") {
            continue;
        }
        let age = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or_default(); // silent-ok: an unreadable age falls back to the owner check
        let owner_gone = staging_owner(&name).is_none_or(|pid| !alive(pid));
        if (owner_gone || age > STAGING_ABANDONED_AFTER)
            && let Err(error) = std::fs::remove_dir_all(entry.path())
        {
            tracing::debug!(%error, name, "triage: abandoned source staging not removed");
        }
    }
}

/// A fetch lock for one commit, held while that commit is fetched.
struct FetchLock {
    _file: std::fs::File,
}

/// `Ok(None)`: another fetch (this process or another) holds it.
fn try_lock_fetch(staging: &Path, commit: &str) -> std::io::Result<Option<FetchLock>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(staging.join(format!("{commit}.lock")))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(FetchLock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

/// Whether `directory` is a complete checkout of `commit`: its own `.git`
/// folder (not a symlink, not a parent's) and `HEAD` at the commit.
async fn is_checkout_of(git: &dyn GitRunner, directory: &Path, commit: &str) -> bool {
    let own_git = tokio::fs::symlink_metadata(directory.join(".git"))
        .await
        .is_ok_and(|meta| meta.is_dir()); // silent-ok: no readable .git means not a checkout
    if !own_git {
        return false;
    }
    match git
        .run(directory, &["rev-parse", "HEAD"], LOCAL_GIT_TIMEOUT)
        .await
    {
        Ok(head) => head.trim() == commit,
        Err(error) => {
            tracing::debug!(%error, "triage: cached checkout unreadable");
            false
        }
    }
}

/// The backend's source cache: resolves its build's ref and fetches it in
/// the background.
pub(crate) struct SourceCache {
    dir: PathBuf,
    staging: PathBuf,
    git: Arc<dyn GitRunner>,
    /// The build this backend runs, as the context file names it.
    build: String,
    build_commit: Option<String>,
}

impl SourceCache {
    pub(crate) fn new(
        home: &Path,
        git: Arc<dyn GitRunner>,
        build: String,
        build_commit: Option<String>,
    ) -> Self {
        let staging = staging_dir(home);
        reclaim_abandoned_staging(&staging, SystemTime::now(), pid_alive);
        Self {
            dir: source_cache_dir(home),
            staging,
            git,
            build,
            build_commit,
        }
    }

    /// The source for this session: already fetched, fetching (started now
    /// or by an earlier session, in any process), or why not. Never fails
    /// the session.
    pub(crate) async fn start(&self) -> SourceClone {
        match self.try_start().await {
            Ok(clone) => clone,
            Err(reason) => SourceClone::Failed {
                build: self.build.clone(),
                reason,
            },
        }
    }

    /// An existing cache folder: used only when it is a complete checkout.
    async fn existing(
        &self,
        directory: PathBuf,
        source: SourceRef,
    ) -> Result<SourceClone, SourceFailure> {
        if is_checkout_of(self.git.as_ref(), &directory, &source.commit).await {
            Ok(SourceClone::Cloned {
                directory,
                source,
                build: self.build.clone(),
            })
        } else {
            // Our fetches appear only when complete, but an agent may have
            // cloned here by hand and stopped partway.
            Err(SourceFailure::NotACheckout {
                directory,
                commit: source.commit,
            })
        }
    }

    async fn try_start(&self) -> Result<SourceClone, SourceFailure> {
        for dir in [&self.dir, &self.staging] {
            tokio::fs::create_dir_all(dir).await.map_err(|error| {
                tracing::debug!(%error, dir = %dir.display(), "triage: source folder not created");
                SourceFailure::CacheUnwritable {
                    directory: dir.clone(),
                }
            })?;
        }
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
                .map_err(logged)?,
        };
        let source = pick_source_ref(&listed, self.build_commit.as_deref())?;
        let directory = self.dir.join(&source.commit);
        let build = self.build.clone();
        if path_exists(&directory).await {
            return self.existing(directory, source).await;
        }
        let lock = try_lock_fetch(&self.staging, &source.commit).map_err(|error| {
            tracing::debug!(%error, "triage: source fetch lock unavailable");
            SourceFailure::CacheUnwritable {
                directory: self.staging.clone(),
            }
        })?;
        let Some(lock) = lock else {
            // Another session's fetch (any process) is running: share it.
            return Ok(SourceClone::Cloning {
                directory,
                source,
                build,
            });
        };
        // A fetch that finished between the check above and the lock.
        if path_exists(&directory).await {
            drop(lock);
            return self.existing(directory, source).await;
        }
        let fetch = Fetch {
            staging: self.staging.clone(),
            directory: directory.clone(),
            commit: source.commit.clone(),
            git: Arc::clone(&self.git),
        };
        // Outlives the request that started it: a fresh fetch takes tens of
        // seconds, and the user is writing their description meanwhile. The
        // lock goes with it and is released when it ends.
        tokio::spawn(async move {
            if let Err(failure) = fetch.run().await {
                tracing::debug!(?failure, commit = %fetch.commit, "triage: source fetch failed");
            }
            drop(lock);
        });
        Ok(SourceClone::Cloning {
            directory,
            source,
            build,
        })
    }
}

async fn path_exists(path: &Path) -> bool {
    // silent-ok: an unreadable cache entry is treated as absent; the
    // fetch's rename then fails and is reported.
    tokio::fs::symlink_metadata(path).await.is_ok()
}

/// One background fetch of one commit, holding that commit's lock.
struct Fetch {
    staging: PathBuf,
    directory: PathBuf,
    commit: String,
    git: Arc<dyn GitRunner>,
}

impl Fetch {
    async fn run(&self) -> Result<(), SourceFailure> {
        let work = self.staging.join(staging_name(std::process::id()));
        tokio::fs::create_dir_all(&work).await.map_err(|error| {
            tracing::debug!(%error, work = %work.display(), "triage: staging folder not created");
            SourceFailure::CacheUnwritable {
                directory: work.clone(),
            }
        })?;
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
    async fn fetch_into(&self, work: &Path) -> Result<(), SourceFailure> {
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
            self.git.run(work, args, timeout).await.map_err(logged)?;
        }
        if !is_checkout_of(self.git.as_ref(), work, &self.commit).await {
            return Err(SourceFailure::GitFailed {
                operation: "checkout".into(),
            });
        }
        let not_a_checkout = || SourceFailure::NotACheckout {
            directory: self.directory.clone(),
            commit: self.commit.clone(),
        };
        match tokio::fs::rename(work, &self.directory).await {
            Ok(()) => Ok(()),
            // Something got there first (a hand clone; fetches are locked):
            // keep it only when it is the same complete checkout.
            Err(error) if path_exists(&self.directory).await => {
                tracing::debug!(%error, "triage: the cache folder appeared during the fetch");
                if is_checkout_of(self.git.as_ref(), &self.directory, &self.commit).await {
                    Ok(())
                } else {
                    Err(not_a_checkout())
                }
            }
            Err(error) => {
                tracing::debug!(%error, "triage: the fetch could not move into the cache");
                Err(SourceFailure::CacheUnwritable {
                    directory: self.directory.clone(),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests;
