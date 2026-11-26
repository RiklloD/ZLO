//! The update's durable host files, under `<reborn_home>/update/` (0700):
//!
//! - `run.json` — the last (or current) job: state, times, message.
//! - `check.json` — what the last check found upstream.
//! - `update.log` — the job's output, truncated when a job starts.
//! - `update.lock` — held (an OS file lock) by the running job for its
//!   whole life, so it is released even when the job dies.
//! - `staged.json` + `staged/` — a downloaded build waiting to be
//!   installed (`zlo update` download/install jobs): the record names the
//!   commit; the folder holds the built `ZLO.app` or `zlo` binary.
//! - `zlo-updater` — the copy of the binary the detached job runs
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
    AvailableUpdate, MAX_UPDATE_LOG_TAIL_LINES, StagedUpdate, UpdateJob, UpdateRun, UpdateRunState,
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
        self.dir.join("zlo-updater")
    }

    fn staged_file(&self) -> PathBuf {
        self.dir.join("staged.json")
    }

    /// The folder a download job leaves its build in.
    pub(crate) fn staged_dir(&self) -> PathBuf {
        self.dir.join("staged")
    }
}

/// `run.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RunRecord {
    pub state: UpdateRunState,
    /// Absent from records older jobs wrote: those were full updates.
    #[serde(default)]
    pub job: UpdateJob,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub message: Option<String>,
}

impl RunRecord {
    pub(crate) fn running(message: &str) -> Self {
        Self {
            state: UpdateRunState::Running,
            job: UpdateJob::Update,
            started_at: Some(Utc::now()),
            finished_at: None,
            message: Some(message.to_string()),
        }
    }

    pub(crate) fn for_job(mut self, job: UpdateJob) -> Self {
        self.job = job;
        self
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

/// `staged.json`: the downloaded build, and the build it was downloaded
/// over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StagedRecord {
    /// The commit the staged build was built from.
    pub commit: String,
    pub staged_at: DateTime<Utc>,
    /// The commit of the build that was running when it was downloaded.
    /// The staged build is an update only over that build: once something
    /// else got installed (a newer `zlo update`, `bundle.sh`), installing
    /// it could go backwards.
    pub base: Option<String>,
}

impl StagedRecord {
    /// What the wire offers while `running` runs: the staged build, when
    /// it was downloaded over exactly this build and is not this build.
    pub(crate) fn offer(&self, running: Option<&str>) -> Option<StagedUpdate> {
        (self.base.as_deref() == running && running != Some(self.commit.as_str())).then(|| {
            StagedUpdate {
                commit: self.commit.clone(),
                staged_at: self.staged_at,
            }
        })
    }
}

/// The downloaded build, when its record and its folder are both there.
pub(crate) fn read_staged(paths: &UpdatePaths) -> anyhow::Result<Option<StagedRecord>> {
    let staged: Option<StagedRecord> = read_json(&paths.staged_file())?;
    Ok(staged.filter(|_| paths.staged_dir().is_dir()))
}

pub(crate) fn write_staged(paths: &UpdatePaths, staged: &StagedRecord) -> anyhow::Result<()> {
    write_json(paths, &paths.staged_file(), staged)
}

/// Forget the downloaded build: the record first (so a reader never sees a
/// record for a half-removed folder), then the folder.
pub(crate) fn clear_staged(paths: &UpdatePaths) -> anyhow::Result<()> {
    let file = paths.staged_file();
    match std::fs::remove_file(&file) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("remove {}", file.display())),
    }
    let dir = paths.staged_dir();
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", dir.display())),
    }
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

/// How long a starting job waits for the lock: a status probe
/// ([`is_locked`]) holds it for an instant, a running job for minutes.
const JOB_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

/// The lock for a job that is starting: [`try_lock`], retried briefly so a
/// status probe that happens to hold it at that instant does not look like
/// another running job.
pub(crate) fn lock_for_job(paths: &UpdatePaths) -> anyhow::Result<Option<UpdateLock>> {
    let deadline = std::time::Instant::now() + JOB_LOCK_WAIT;
    loop {
        if let Some(lock) = try_lock(paths)? {
            return Ok(Some(lock));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// What [`reserve`] found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reservation {
    /// `run.json` now says `running` ("Starting the update"); start the job.
    Reserved,
    /// A job holds the lock, or another starter reserved a moment ago.
    AlreadyRunning,
}

/// Claim the next job before starting it: under the lock, refuse when a
/// job runs or a fresh reservation stands, else write a `running` record.
/// The lock is released on return so the started job can take it; the
/// record keeps a second starter out until then ([`RunRecord::just_started`]).
/// Every start path (the CLI's hand-off, the app's button) goes through here.
pub(crate) fn reserve(paths: &UpdatePaths, job: UpdateJob) -> anyhow::Result<Reservation> {
    let Some(_lock) = lock_for_job(paths)? else {
        return Ok(Reservation::AlreadyRunning);
    };
    if read_run(paths)?.is_some_and(|run| run.just_started(Utc::now())) {
        return Ok(Reservation::AlreadyRunning);
    }
    write_run(paths, &RunRecord::running(STARTING).for_job(job))?;
    Ok(Reservation::Reserved)
}

/// The message of a reserved, not yet started job.
pub(crate) const STARTING: &str = "Starting the update";

/// Mark a reservation whose job could not be started as failed.
pub(crate) fn reservation_failed(paths: &UpdatePaths, message: &str) -> anyhow::Result<()> {
    let job = read_run(paths)?.map(|run| run.job).unwrap_or_default();
    write_run(
        paths,
        &RunRecord::running(STARTING)
            .for_job(job)
            .finish(UpdateRunState::Failed, message.to_string()),
    )
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
        job: record.job,
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
    fn a_staged_build_needs_its_record_and_its_folder_and_clears_whole() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = UpdatePaths::new(home.path());
        assert_eq!(read_staged(&paths).expect("read"), None);
        let staged = StagedRecord {
            commit: "b".repeat(40),
            staged_at: Utc::now(),
            base: Some("a".repeat(40)),
        };
        write_staged(&paths, &staged).expect("write");
        assert_eq!(
            read_staged(&paths).expect("read"),
            None,
            "a record without its build is nothing to install"
        );
        std::fs::create_dir_all(paths.staged_dir().join("ZLO.app")).expect("build");
        assert_eq!(read_staged(&paths).expect("read"), Some(staged.clone()));
        // Offered only over the build it was downloaded over.
        let (a, b, c) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        assert_eq!(staged.offer(Some(&a)).map(|o| o.commit), Some(b.clone()));
        assert_eq!(staged.offer(Some(&b)), None, "already running it");
        assert_eq!(
            staged.offer(Some(&c)),
            None,
            "something else was installed since: installing it could go backwards"
        );
        assert_eq!(staged.offer(None), None);
        clear_staged(&paths).expect("clear");
        assert_eq!(read_staged(&paths).expect("read"), None);
        assert!(!paths.staged_dir().exists());
        clear_staged(&paths).expect("clearing nothing is fine");

        // The job kind rides the reservation and a failed start; a record
        // from before the field reads as a full update.
        assert_eq!(
            reserve(&paths, UpdateJob::Download).expect("reserve"),
            Reservation::Reserved
        );
        reservation_failed(&paths, "did not start").expect("fail");
        let run = last_run(&paths, false, Utc::now()).expect("view");
        assert_eq!(
            (run.job, run.state),
            (UpdateJob::Download, UpdateRunState::Failed)
        );
        let old: RunRecord = serde_json::from_str(
            r#"{"state":"succeeded","started_at":null,"finished_at":null,"message":"Updated"}"#,
        )
        .expect("old record");
        assert_eq!(old.job, UpdateJob::Update);
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
    fn one_reservation_at_a_time_and_none_while_a_job_runs() {
        let home = tempfile::tempdir().expect("tempdir");
        let paths = UpdatePaths::new(home.path());
        assert_eq!(
            reserve(&paths, UpdateJob::Update).expect("reserve"),
            Reservation::Reserved
        );
        let run = read_run(&paths).expect("read").expect("record");
        assert_eq!(
            run.state,
            UpdateRunState::Running,
            "published before returning"
        );
        assert!(!is_locked(&paths).expect("probe"), "released for the job");
        assert_eq!(
            reserve(&paths, UpdateJob::Update).expect("reserve"),
            Reservation::AlreadyRunning,
            "a fresh reservation keeps a second starter out"
        );
        reservation_failed(&paths, "did not start").expect("fail");
        let _job = try_lock(&paths).expect("lock").expect("free");
        assert_eq!(
            reserve(&paths, UpdateJob::Update).expect("reserve"),
            Reservation::AlreadyRunning,
            "a running job keeps starters out"
        );
        drop(_job);
        assert_eq!(
            reserve(&paths, UpdateJob::Update).expect("reserve"),
            Reservation::Reserved
        );
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
