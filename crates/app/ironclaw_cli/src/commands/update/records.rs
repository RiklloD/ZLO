//! The update's durable host files, under `<reborn_home>/update/` (0700):
//!
//! - `run.json` — the last (or current) job: state, times, message.
//! - `check.json` — what the last check found upstream.
//! - `update.log` — the job's output, truncated when a job starts.
//! - `update.lock` — held (an OS file lock) by the running job for its
//!   whole life, so it is released even when the job dies.
//! - `ironclaw-updater` — the copy of the binary the detached job runs
//!   from, so replacing the installed binary or app never touches it.
//!
//! Plain files beside the other host files the binary keeps in the Reborn
//! home (the WebUI token, the onboarding marker): they must be readable by
//! a backend that just restarted, before any runtime exists. Writes are
//! atomic (temp file + rename) so a reader never sees half a record.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use ironclaw_product_contracts::self_update::{
    AvailableUpdate, MAX_UPDATE_LOG_TAIL_LINES, UpdateRun, UpdateRunState,
};
use serde::{Deserialize, Serialize};

/// A job that reports `running` but holds no lock this long after it
/// started died without writing its outcome.
const START_GRACE_SECONDS: i64 = 30;
const LOG_TAIL_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct UpdatePaths {
    dir: PathBuf,
}

impl UpdatePaths {
    pub(crate) fn new(reborn_home: &Path) -> Self {
        Self {
            dir: reborn_home.join("update"),
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn ensure_dir(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("create {}", self.dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("set permissions on {}", self.dir.display()))?;
        }
        Ok(())
    }

    fn run_file(&self) -> PathBuf {
        self.dir.join("run.json")
    }

    fn check_file(&self) -> PathBuf {
        self.dir.join("check.json")
    }

    pub(crate) fn log_file(&self) -> PathBuf {
        self.dir.join("update.log")
    }

    fn lock_file(&self) -> PathBuf {
        self.dir.join("update.lock")
    }

    pub(crate) fn updater_binary(&self) -> PathBuf {
        self.dir.join("ironclaw-updater")
    }
}

/// `run.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RunRecord {
    pub state: UpdateRunState,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub message: Option<String>,
}

impl RunRecord {
    pub(crate) fn running(message: &str) -> Self {
        Self {
            state: UpdateRunState::Running,
            started_at: Some(Utc::now()),
            finished_at: None,
            message: Some(message.to_string()),
        }
    }

    pub(crate) fn finish(mut self, state: UpdateRunState, message: String) -> Self {
        self.state = state;
        self.finished_at = Some(Utc::now());
        self.message = Some(message);
        self
    }

    /// Within the start grace: the job may not hold the lock yet.
    pub(crate) fn just_started(&self, now: DateTime<Utc>) -> bool {
        self.state == UpdateRunState::Running
            && self
                .started_at
                .is_some_and(|t| (now - t).num_seconds() < START_GRACE_SECONDS)
    }
}

/// `check.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CheckRecord {
    pub upstream: String,
    pub behind: u32,
    pub subjects: Vec<String>,
    pub checked_at: DateTime<Utc>,
}

impl CheckRecord {
    /// What the wire calls `available`: `None` when nothing is newer, or
    /// when the running build already is the upstream tip.
    pub(crate) fn available(&self, running: Option<&str>) -> Option<AvailableUpdate> {
        if self.behind == 0 || running == Some(self.upstream.as_str()) {
            return None;
        }
        Some(AvailableUpdate {
            commit: self.upstream.clone(),
            behind: self.behind,
            subjects: self.subjects.clone(),
            checked_at: self.checked_at,
        })
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("parse {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn write_json<T: Serialize>(paths: &UpdatePaths, path: &Path, value: &T) -> anyhow::Result<()> {
    paths.ensure_dir()?;
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut tmp = tempfile::NamedTempFile::new_in(paths.dir())
        .with_context(|| format!("create temp file in {}", paths.dir().display()))?;
    tmp.write_all(&bytes)
        .and_then(|()| tmp.flush())
        .with_context(|| format!("write {}", tmp.path().display()))?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

pub(crate) fn read_run(paths: &UpdatePaths) -> anyhow::Result<Option<RunRecord>> {
    read_json(&paths.run_file())
}

pub(crate) fn write_run(paths: &UpdatePaths, run: &RunRecord) -> anyhow::Result<()> {
    write_json(paths, &paths.run_file(), run)
}

pub(crate) fn read_check(paths: &UpdatePaths) -> anyhow::Result<Option<CheckRecord>> {
    read_json(&paths.check_file())
}

pub(crate) fn write_check(paths: &UpdatePaths, check: &CheckRecord) -> anyhow::Result<()> {
    write_json(paths, &paths.check_file(), check)
}

/// The OS lock a job holds for its whole life. Dropped (or the process
/// gone): released.
pub(crate) struct UpdateLock {
    _file: File,
}

/// Take the lock, `Ok(None)` when another job holds it.
pub(crate) fn try_lock(paths: &UpdatePaths) -> anyhow::Result<Option<UpdateLock>> {
    paths.ensure_dir()?;
    let path = paths.lock_file();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(UpdateLock { _file: file })),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("lock {}", path.display()))
        }
    }
}

/// Whether a job holds the lock right now (probes and releases at once).
/// Creates nothing: no lock file means no job ever ran here.
pub(crate) fn is_locked(paths: &UpdatePaths) -> anyhow::Result<bool> {
    if !paths.lock_file().exists() {
        return Ok(false);
    }
    Ok(try_lock(paths)?.is_none())
}

/// Truncate the log for a new job and open it for appending.
pub(crate) fn fresh_log(paths: &UpdatePaths) -> anyhow::Result<File> {
    paths.ensure_dir()?;
    let path = paths.log_file();
    File::create(&path).with_context(|| format!("truncate {}", path.display()))?;
    append_log(paths)
}

pub(crate) fn append_log(paths: &UpdatePaths) -> anyhow::Result<File> {
    let path = paths.log_file();
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open {}", path.display()))
}

/// The last lines of the log, without control characters other than tab.
pub(crate) fn log_tail(paths: &UpdatePaths) -> anyhow::Result<String> {
    let path = paths.log_file();
    let mut file = match File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(LOG_TAIL_BYTES)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<String> = text
        .lines()
        .map(|l| {
            // Progress bars redraw with `\r`; keep the last frame.
            let l = l.rsplit('\r').next().unwrap_or(l);
            l.chars()
                .filter(|c| *c == '\t' || !c.is_control())
                .collect()
        })
        .collect();
    let start = lines.len().saturating_sub(MAX_UPDATE_LOG_TAIL_LINES);
    Ok(lines[start..].join("\n"))
}

/// The wire view of the last job. A `running` record whose job holds no
/// lock past the start grace died without an outcome: shown as failed
/// (the file is not rewritten; the next job replaces it).
pub(crate) fn last_run(
    paths: &UpdatePaths,
    locked: bool,
    now: DateTime<Utc>,
) -> anyhow::Result<UpdateRun> {
    let Some(record) = read_run(paths)? else {
        return Ok(UpdateRun::default());
    };
    let mut run = UpdateRun {
        state: record.state,
        started_at: record.started_at,
        finished_at: record.finished_at,
        message: record.message.clone(),
        log_tail: log_tail(paths)?,
    };
    if record.state == UpdateRunState::Running && !locked && !record.just_started(now) {
        run.state = UpdateRunState::Failed;
        run.message = Some(
            "The update stopped before it finished (the process ended). See the log, then try again."
                .into(),
        );
    }
    Ok(run)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_start_empty() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = UpdatePaths::new(home.path());
        assert_eq!(read_run(&paths).expect("read"), None);
        assert_eq!(
            last_run(&paths, false, Utc::now()).expect("view"),
            UpdateRun::default()
        );
        let run = RunRecord::running("Fetching");
        write_run(&paths, &run).expect("write");
        assert_eq!(read_run(&paths).expect("read"), Some(run));
    }

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = UpdatePaths::new(home.path());
        let held = try_lock(&paths).expect("lock").expect("free");
        assert!(is_locked(&paths).expect("probe"));
        assert!(try_lock(&paths).expect("lock").is_none());
        drop(held);
        assert!(!is_locked(&paths).expect("probe"));
    }

    #[test]
    fn a_running_record_without_its_job_reads_as_failed_after_the_grace() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = UpdatePaths::new(home.path());
        let started = Utc::now();
        let mut run = RunRecord::running("Building");
        run.started_at = Some(started);
        write_run(&paths, &run).expect("write");
        let view = last_run(&paths, false, started).expect("view");
        assert_eq!(view.state, UpdateRunState::Running, "still starting");
        let later = started + chrono::Duration::seconds(START_GRACE_SECONDS + 1);
        assert_eq!(
            last_run(&paths, true, later).expect("view").state,
            UpdateRunState::Running,
            "a job holding the lock is running"
        );
        let dead = last_run(&paths, false, later).expect("view");
        assert_eq!(dead.state, UpdateRunState::Failed);
        assert!(dead.message.expect("message").contains("stopped"));
    }

    #[test]
    fn the_log_tail_is_bounded_and_clean() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = UpdatePaths::new(home.path());
        let mut log = fresh_log(&paths).expect("log");
        for i in 0..100 {
            writeln!(log, "line {i}\x1b[0m").expect("write");
        }
        writeln!(log, "Compiling 1/9\rCompiling 9/9").expect("write");
        let tail = log_tail(&paths).expect("tail");
        let lines: Vec<&str> = tail.lines().collect();
        assert_eq!(lines.len(), MAX_UPDATE_LOG_TAIL_LINES);
        assert_eq!(lines.last(), Some(&"Compiling 9/9"));
        assert!(!tail.contains('\x1b'));
        drop(fresh_log(&paths).expect("new job"));
        assert_eq!(
            log_tail(&paths).expect("tail"),
            "",
            "a new job starts a new log"
        );
    }

    #[test]
    fn available_hides_an_up_to_date_build() {
        let check = CheckRecord {
            upstream: "b".repeat(40),
            behind: 2,
            subjects: vec!["x".into()],
            checked_at: Utc::now(),
        };
        assert!(check.available(Some(&"a".repeat(40))).is_some());
        assert!(check.available(Some(&"b".repeat(40))).is_none());
        assert!(CheckRecord { behind: 0, ..check }.available(None).is_none());
    }
}
