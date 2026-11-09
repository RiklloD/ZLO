//! The git half of a source update: which branch to follow, fetch it, see
//! how far behind the running build is, and fast-forward — never merge,
//! rebase or reset. A dirty or diverged checkout is refused with a message
//! the operator can act on.
//!
//! Every argument is either a fixed literal or a value validated by
//! [`ironclaw_config::is_safe_branch_name`] (branch and remote names) or a
//! full hex commit, so nothing here is an option or a revision expression
//! someone else chose. `GIT_TERMINAL_PROMPT=0` and a batch-mode SSH keep a
//! fetch from waiting on a prompt nobody sees.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use ironclaw_config::is_safe_branch_name;
use ironclaw_product_contracts::self_update::{MAX_UPDATE_SUBJECTS, bound_subject};

const FETCH_TIMEOUT: Duration = Duration::from_secs(120);
const LOCAL_TIMEOUT: Duration = Duration::from_secs(30);
/// Most bytes kept of one git command's output.
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Why the checkout cannot be checked or updated. `Display` is the
/// operator-facing message (the WebUI and ZLO show it as is).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum GitRefusal {
    #[error("The update checkout {0} does not exist. Fix `[update] repo` in config.toml.")]
    RepoMissing(PathBuf),
    #[error("{0} is not a git checkout. Fix `[update] repo` in config.toml.")]
    NotARepo(PathBuf),
    #[error(
        "The checkout is not on a branch (detached HEAD). Check out a branch, or set `[update] branch`."
    )]
    DetachedHead,
    #[error(
        "The checkout is on `{checked_out}`, but `[update] branch` is `{configured}`. Check out `{configured}` there, or change the setting."
    )]
    BranchMismatch {
        checked_out: String,
        configured: String,
    },
    #[error("Branch `{0}` has no usable upstream remote (set one with `git branch -u`).")]
    NoUpstream(String),
    #[error("The checkout has local changes ({0}). Commit or stash them, then update again.")]
    Dirty(String),
    #[error(
        "The checkout has {ahead} commit(s) that are not on `{upstream}`. Only fast-forward updates run: push or reset them first."
    )]
    Diverged { ahead: u32, upstream: String },
    #[error("Could not fetch `{upstream}`: {reason}")]
    FetchFailed { upstream: String, reason: String },
    #[error("git {command} failed: {reason}")]
    GitFailed { command: String, reason: String },
}

/// The branch an update follows and where its upstream lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Tracking {
    pub branch: String,
    pub remote: String,
}

impl Tracking {
    /// `refs/remotes/<remote>/<branch>`, the ref a fetch updates.
    pub(crate) fn upstream_ref(&self) -> String {
        format!("refs/remotes/{}/{}", self.remote, self.branch)
    }

    /// `<remote>/<branch>`, for messages.
    pub(crate) fn upstream_name(&self) -> String {
        format!("{}/{}", self.remote, self.branch)
    }
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckOutcome {
    pub tracking: Tracking,
    /// The checkout's HEAD.
    pub head: String,
    /// The upstream tip after the fetch.
    pub upstream: String,
    /// Commits from the running build (or HEAD, when the build is unknown
    /// to this checkout) to the upstream tip.
    pub behind: u32,
    /// Their subjects, newest first, bounded.
    pub subjects: Vec<String>,
}

/// One checkout, driven through the `git` on `PATH`.
pub(crate) struct Checkout {
    repo: PathBuf,
}

impl Checkout {
    /// Fails when `repo` is missing or not a git work tree.
    pub(crate) fn open(repo: &Path) -> Result<Self, GitRefusal> {
        if !repo.is_dir() {
            return Err(GitRefusal::RepoMissing(repo.to_path_buf()));
        }
        let checkout = Self {
            repo: repo.to_path_buf(),
        };
        match checkout.git(&["rev-parse", "--is-inside-work-tree"], LOCAL_TIMEOUT) {
            Ok(out) if out.trim() == "true" => Ok(checkout),
            _ => Err(GitRefusal::NotARepo(repo.to_path_buf())),
        }
    }

    /// The branch to follow: the configured one, which must be the one
    /// checked out, else the checked-out branch. Its remote comes from
    /// `branch.<name>.remote`, default `origin`.
    pub(crate) fn tracking(&self, configured: Option<&str>) -> Result<Tracking, GitRefusal> {
        let checked_out = self
            .git(
                &["symbolic-ref", "--quiet", "--short", "HEAD"],
                LOCAL_TIMEOUT,
            )
            .map(|s| s.trim().to_string())
            .map_err(|_| GitRefusal::DetachedHead)?;
        if !is_safe_branch_name(&checked_out) {
            return Err(GitRefusal::NoUpstream(checked_out));
        }
        if let Some(configured) = configured
            && configured != checked_out
        {
            return Err(GitRefusal::BranchMismatch {
                checked_out,
                configured: configured.to_string(),
            });
        }
        let key = format!("branch.{checked_out}.remote");
        let remote = self
            .git(&["config", "--get", &key], LOCAL_TIMEOUT)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "origin".to_string()); // silent-ok: an unset branch remote means git's own default, `origin`.
        if !is_safe_branch_name(&remote) {
            return Err(GitRefusal::NoUpstream(checked_out));
        }
        Ok(Tracking {
            branch: checked_out,
            remote,
        })
    }

    /// Fetch the branch into its remote-tracking ref (and nothing else).
    pub(crate) fn fetch(&self, tracking: &Tracking) -> Result<String, GitRefusal> {
        let refspec = format!(
            "+refs/heads/{}:{}",
            tracking.branch,
            tracking.upstream_ref()
        );
        self.git(
            &["fetch", "--quiet", "--no-tags", &tracking.remote, &refspec],
            FETCH_TIMEOUT,
        )
        .map_err(|e| GitRefusal::FetchFailed {
            upstream: tracking.upstream_name(),
            reason: e.reason(),
        })?;
        self.rev(&tracking.upstream_ref())
    }

    pub(crate) fn head(&self) -> Result<String, GitRefusal> {
        self.rev("HEAD")
    }

    fn rev(&self, name: &str) -> Result<String, GitRefusal> {
        let spec = format!("{name}^{{commit}}");
        self.git(&["rev-parse", "--verify", "--quiet", &spec], LOCAL_TIMEOUT)
            .map(|s| s.trim().to_string())
            .map_err(|e| e.refusal("rev-parse"))
    }

    /// Whether `commit` (a full hex hash) is known to this checkout.
    fn knows(&self, commit: &str) -> bool {
        is_full_hash(commit)
            && self
                .git(
                    &["cat-file", "-e", &format!("{commit}^{{commit}}")],
                    LOCAL_TIMEOUT,
                )
                .is_ok()
    }

    /// Tracked files with local changes, `None` when clean. Untracked files
    /// do not count: a fast-forward refuses by itself to overwrite one.
    pub(crate) fn dirty(&self) -> Result<Option<String>, GitRefusal> {
        let out = self
            .git(
                &["status", "--porcelain", "--untracked-files=no"],
                LOCAL_TIMEOUT,
            )
            .map_err(|e| e.refusal("status"))?;
        let files: Vec<&str> = out
            .lines()
            .filter_map(|l| l.get(3..))
            .filter(|f| !f.is_empty())
            .collect();
        if files.is_empty() {
            return Ok(None);
        }
        let mut sample = files.iter().take(3).copied().collect::<Vec<_>>().join(", ");
        if files.len() > 3 {
            sample.push_str(&format!(" and {} more", files.len() - 3));
        }
        Ok(Some(sample))
    }

    fn count(&self, range: &str) -> Result<u32, GitRefusal> {
        let out = self
            .git(&["rev-list", "--count", range], LOCAL_TIMEOUT)
            .map_err(|e| e.refusal("rev-list"))?;
        out.trim().parse().map_err(|_| GitRefusal::GitFailed {
            command: "rev-list".into(),
            reason: format!("unexpected count {:?}", out.trim()),
        })
    }

    fn subjects(&self, range: &str) -> Result<Vec<String>, GitRefusal> {
        let limit = format!("--max-count={MAX_UPDATE_SUBJECTS}");
        let out = self
            .git(&["log", &limit, "--format=%s", range], LOCAL_TIMEOUT)
            .map_err(|e| e.refusal("log"))?;
        Ok(out.lines().map(bound_subject).collect())
    }

    /// Fetch and compare. `running` is the commit the running build came
    /// from, when known; else the checkout's HEAD is the base.
    pub(crate) fn check(
        &self,
        configured: Option<&str>,
        running: Option<&str>,
    ) -> Result<CheckOutcome, GitRefusal> {
        let tracking = self.tracking(configured)?;
        let upstream = self.fetch(&tracking)?;
        let head = self.head()?;
        let base = match running {
            Some(commit) if self.knows(commit) => commit.to_string(),
            _ => head.clone(),
        };
        let range = format!("{base}..{upstream}");
        Ok(CheckOutcome {
            behind: self.count(&range)?,
            subjects: self.subjects(&range)?,
            tracking,
            head,
            upstream,
        })
    }

    /// Fast-forward the checkout to the fetched upstream. Refuses a dirty
    /// checkout and one with commits the upstream lacks. Returns the new
    /// HEAD.
    pub(crate) fn fast_forward(&self, tracking: &Tracking) -> Result<String, GitRefusal> {
        if let Some(files) = self.dirty()? {
            return Err(GitRefusal::Dirty(files));
        }
        let upstream = tracking.upstream_ref();
        let ahead = self.count(&format!("{upstream}..HEAD"))?;
        if ahead > 0 {
            return Err(GitRefusal::Diverged {
                ahead,
                upstream: tracking.upstream_name(),
            });
        }
        self.git(&["merge", "--ff-only", "--quiet", &upstream], LOCAL_TIMEOUT)
            .map_err(|e| e.refusal("merge --ff-only"))?;
        self.head()
    }

    fn git(&self, args: &[&str], timeout: Duration) -> Result<String, GitRun> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&self.repo)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if std::env::var_os("GIT_SSH_COMMAND").is_none() {
            command.env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
        }
        run_bounded(command, timeout)
    }
}

/// A full lowercase 40- or 64-character hex hash.
pub(crate) fn is_full_hash(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// One failed git run.
#[derive(Debug)]
struct GitRun {
    stderr: String,
}

impl GitRun {
    fn reason(&self) -> String {
        let line = self
            .stderr
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("no output");
        bound_subject(line)
    }

    fn refusal(&self, command: &str) -> GitRefusal {
        GitRefusal::GitFailed {
            command: command.to_string(),
            reason: self.reason(),
        }
    }
}

/// How long output readers may take to finish once git has exited or been
/// killed; a helper git started could still hold the pipes open.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Run `command` to completion within `timeout`. Git runs in its own process
/// group, and the whole group is killed on timeout, so a helper it started
/// (ssh, a remote helper) cannot outlive it. Output is read on threads so a
/// full pipe cannot stall the child, and reading stops after
/// [`DRAIN_GRACE`] even if a pipe is never closed.
fn run_bounded(mut command: Command, timeout: Duration) -> Result<String, GitRun> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|e| GitRun {
        stderr: format!("could not run git: {e}"),
    })?;
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                kill_group(&mut child);
                break Err(format!("timed out after {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                kill_group(&mut child);
                break Err(format!("wait failed: {e}"));
            }
        }
    };
    let drain_deadline = Instant::now() + DRAIN_GRACE;
    let out = stdout
        .map(|r| collect(r, drain_deadline))
        .unwrap_or_default();
    let err = stderr
        .map(|r| collect(r, drain_deadline))
        .unwrap_or_default();
    match status {
        Ok(status) if status.success() => Ok(out),
        Ok(_) => Err(GitRun { stderr: err }),
        Err(reason) => Err(GitRun { stderr: reason }),
    }
}

/// Kill git's whole process group, then reap git itself.
fn kill_group(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        if let Ok(pgid) = libc::pid_t::try_from(child.id()) {
            // SAFETY: `kill` takes plain integers; a negative pid names the
            // process group git leads (`process_group(0)` above).
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        }
    }
    // silent-ok: best-effort kill and reap of a timed-out git; the timeout is reported by the caller.
    let _ = child.kill();
    let _ = child.wait();
}

fn drain<R: Read + Send + 'static>(mut pipe: R) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match pipe.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = MAX_OUTPUT_BYTES.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        }
        // silent-ok: the receiver gave up after the drain grace; the output is no longer wanted.
        let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
    });
    rx
}

/// The reader's output, or nothing once `deadline` passes.
fn collect(reader: std::sync::mpsc::Receiver<String>, deadline: Instant) -> String {
    let wait = deadline.saturating_duration_since(Instant::now());
    reader.recv_timeout(wait).unwrap_or_default() // silent-ok: a pipe still held open after the grace yields no output; the exit status still decides
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Run git in `dir` for test setup, failing the test on error.
    pub(crate) fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub(crate) fn commit(dir: &Path, file: &str, text: &str, subject: &str) -> String {
        std::fs::write(dir.join(file), text).expect("write");
        sh(dir, &["add", file]);
        sh(dir, &["commit", "-q", "-m", subject]);
        sh(dir, &["rev-parse", "HEAD"])
    }

    /// A bare `origin`, a checkout of it on `main` with one commit, and a
    /// second clone ("upstream work") to push new commits from. No network.
    pub(crate) struct Repos {
        pub _root: tempfile::TempDir,
        pub checkout: PathBuf,
        pub other: PathBuf,
    }

    pub(crate) fn repos() -> Repos {
        let root = tempfile::tempdir().expect("tempdir");
        let origin = root.path().join("origin.git");
        let seed = root.path().join("seed");
        std::fs::create_dir_all(&seed).expect("seed dir");
        sh(
            root.path(),
            &["init", "-q", "--bare", "-b", "main", "origin.git"],
        );
        sh(&seed, &["init", "-q", "-b", "main"]);
        commit(&seed, "README", "one\n", "First");
        let origin_str = origin.to_str().expect("utf8 path");
        sh(&seed, &["remote", "add", "origin", origin_str]);
        sh(&seed, &["push", "-q", "origin", "main"]);
        let checkout = root.path().join("checkout");
        let other = root.path().join("other");
        for dir in [&checkout, &other] {
            sh(
                root.path(),
                &["clone", "-q", origin_str, dir.to_str().expect("utf8")],
            );
        }
        Repos {
            _root: root,
            checkout,
            other,
        }
    }

    pub(crate) fn push_upstream(r: &Repos, subjects: &[&str]) -> String {
        let mut last = String::new();
        for (i, s) in subjects.iter().enumerate() {
            last = commit(&r.other, &format!("f{i}-{s}"), s, s);
        }
        sh(&r.other, &["push", "-q", "origin", "main"]);
        last
    }

    #[test]
    fn check_reports_commits_behind_with_subjects_newest_first() {
        let r = repos();
        let checkout = Checkout::open(&r.checkout).expect("opens");
        let head = checkout.head().expect("head");
        let before = checkout.check(None, Some(&head)).expect("check");
        assert_eq!(before.behind, 0);
        assert_eq!(before.upstream, head);
        assert_eq!(before.tracking.branch, "main");
        assert_eq!(before.tracking.remote, "origin");

        let tip = push_upstream(&r, &["Second", "Third"]);
        let after = checkout.check(None, Some(&head)).expect("check");
        assert_eq!(after.behind, 2);
        assert_eq!(after.upstream, tip);
        assert_eq!(after.subjects, vec!["Third", "Second"]);
        assert_eq!(after.head, head, "a check never moves the checkout");
    }

    #[test]
    fn check_counts_from_the_running_build_not_just_head() {
        let r = repos();
        let checkout = Checkout::open(&r.checkout).expect("opens");
        let built = checkout.head().expect("head");
        push_upstream(&r, &["Second"]);
        let tracking = checkout.tracking(None).expect("tracking");
        checkout.fetch(&tracking).expect("fetch");
        checkout
            .fast_forward(&tracking)
            .expect("pulled, not rebuilt");
        let outcome = checkout.check(None, Some(&built)).expect("check");
        assert_eq!(outcome.behind, 1, "the running build is still behind");
        let unknown = "f".repeat(40);
        let outcome = checkout.check(None, Some(&unknown)).expect("check");
        assert_eq!(outcome.behind, 0, "an unknown build falls back to HEAD");
    }

    #[test]
    fn subjects_are_bounded() {
        let r = repos();
        let checkout = Checkout::open(&r.checkout).expect("opens");
        let names: Vec<String> = (0..MAX_UPDATE_SUBJECTS + 5)
            .map(|i| format!("c{i}"))
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        push_upstream(&r, &refs);
        let outcome = checkout.check(None, None).expect("check");
        assert_eq!(outcome.behind as usize, MAX_UPDATE_SUBJECTS + 5);
        assert_eq!(outcome.subjects.len(), MAX_UPDATE_SUBJECTS);
    }

    #[test]
    fn fast_forward_moves_a_clean_checkout() {
        let r = repos();
        let tip = push_upstream(&r, &["Second"]);
        let checkout = Checkout::open(&r.checkout).expect("opens");
        let tracking = checkout.tracking(Some("main")).expect("tracking");
        checkout.fetch(&tracking).expect("fetch");
        assert_eq!(checkout.fast_forward(&tracking).expect("ff"), tip);
    }

    #[test]
    fn a_dirty_checkout_is_refused_and_left_alone() {
        let r = repos();
        push_upstream(&r, &["Second"]);
        std::fs::write(r.checkout.join("README"), "edited\n").expect("edit");
        let checkout = Checkout::open(&r.checkout).expect("opens");
        let head = checkout.head().expect("head");
        let tracking = checkout.tracking(None).expect("tracking");
        checkout.fetch(&tracking).expect("fetch");
        let err = checkout.fast_forward(&tracking).expect_err("refused");
        assert_eq!(err, GitRefusal::Dirty("README".into()));
        assert!(err.to_string().contains("Commit or stash"), "{err}");
        assert_eq!(checkout.head().expect("head"), head);
        assert_eq!(
            std::fs::read_to_string(r.checkout.join("README")).expect("read"),
            "edited\n"
        );
    }

    #[test]
    fn untracked_files_do_not_make_a_checkout_dirty() {
        let r = repos();
        std::fs::write(r.checkout.join("scratch.txt"), "x").expect("write");
        let checkout = Checkout::open(&r.checkout).expect("opens");
        assert_eq!(checkout.dirty().expect("status"), None);
    }

    #[test]
    fn a_diverged_checkout_is_refused() {
        let r = repos();
        push_upstream(&r, &["Upstream"]);
        let local = commit(&r.checkout, "local.txt", "mine", "Local work");
        let checkout = Checkout::open(&r.checkout).expect("opens");
        let tracking = checkout.tracking(None).expect("tracking");
        checkout.fetch(&tracking).expect("fetch");
        let err = checkout.fast_forward(&tracking).expect_err("refused");
        assert_eq!(
            err,
            GitRefusal::Diverged {
                ahead: 1,
                upstream: "origin/main".into()
            }
        );
        assert_eq!(checkout.head().expect("head"), local, "nothing merged");
    }

    #[test]
    fn branch_rules_are_enforced() {
        let r = repos();
        let checkout = Checkout::open(&r.checkout).expect("opens");
        assert_eq!(
            checkout.tracking(Some("release")).expect_err("mismatch"),
            GitRefusal::BranchMismatch {
                checked_out: "main".into(),
                configured: "release".into()
            }
        );
        let head = checkout.head().expect("head");
        sh(&r.checkout, &["checkout", "-q", "--detach", &head]);
        assert_eq!(
            checkout.tracking(None).expect_err("detached"),
            GitRefusal::DetachedHead
        );
    }

    #[test]
    fn missing_and_non_git_directories_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope");
        assert!(matches!(
            Checkout::open(&missing),
            Err(GitRefusal::RepoMissing(_))
        ));
        assert!(matches!(
            Checkout::open(dir.path()),
            Err(GitRefusal::NotARepo(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_kills_helpers_that_hold_the_pipes() {
        // Like git leaving an ssh helper behind: the child exits only when
        // killed, and its own child keeps stdout and stderr open.
        let marker = tempfile::tempdir().expect("tempdir");
        let pid_file = marker.path().join("helper.pid");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("sleep 60 & echo $! > {}; wait", pid_file.display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let err = run_bounded(command, Duration::from_millis(500)).unwrap_err();
        assert!(err.stderr.contains("timed out"), "{}", err.stderr);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "returned after {:?}",
            started.elapsed()
        );
        let helper = std::fs::read_to_string(&pid_file).expect("helper pid");
        let alive = Command::new("kill")
            .args(["-0", helper.trim()])
            .status()
            .expect("kill -0");
        assert!(!alive.success(), "the helper outlived the timeout");
    }

    #[test]
    fn an_unreachable_remote_is_a_fetch_failure() {
        let r = repos();
        let gone = r.checkout.join("no-such-origin.git");
        sh(
            &r.checkout,
            &["remote", "set-url", "origin", gone.to_str().expect("utf8")],
        );
        let checkout = Checkout::open(&r.checkout).expect("opens");
        assert!(matches!(
            checkout.check(None, None),
            Err(GitRefusal::FetchFailed { .. })
        ));
    }
}
