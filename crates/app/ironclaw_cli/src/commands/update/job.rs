//! One update job, and the outcome it records in `run.json`. Three kinds
//! ([`UpdateJob`]):
//!
//! - `update`: lock, fetch, fast-forward, build into the staging folder,
//!   install from it, restart — all in one go (`zlo update`, older clients).
//! - `download`: the same up to the build, then stop: the new version
//!   waits in `<reborn_home>/update/staged/` (`staged.json` names its
//!   commit) while the running one keeps going.
//! - `install`: install the staged build and restart. No fetch, no build:
//!   this is the quick "restart to install" step.
//!
//! The job outlives the backend that starts it. The backend copies its own
//! binary to `<reborn_home>/update/zlo-updater` and runs
//! `zlo-updater update --run-job --install-path <its own path>` in a
//! new process group (on Linux under systemd, in its own transient
//! `systemd-run --user` unit), with stdin closed and output appended to
//! `update.log`. So:
//!
//! - quitting ZLO (which `bundle.sh` does, and which stops its backend), a
//!   launchd stop or a `systemctl restart` of the backend's unit does not
//!   reach the job;
//! - `bundle.sh`'s "wait until nothing under ZLO.app runs" never waits on
//!   the job, which runs from outside the bundle;
//! - replacing the installed binary or app does not touch the running job.
//!
//! The backend that comes back reads `run.json`, so a remote caller sees
//! the outcome after the restart.
//!
//! Building, installing and restarting sit behind [`UpdateHost`]; tests
//! never run `bundle.sh` or replace a binary.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context as _;
use ironclaw_config::{REBORN_HOME_ENV, REBORN_PROFILE_ENV, SourceUpdateSettings};
use ironclaw_product_contracts::self_update::{UpdateJob, UpdateRunState};

use super::git::{Checkout, Scope};
use super::records::{self, RunRecord, StagedRecord, UpdatePaths};

/// Where `cargo build --release -p ironclaw --bin zlo` leaves the binary.
const BUILT_BINARY: &str = "target/release/zlo";
/// Where `bundle.sh --no-install` leaves the app.
const BUILT_APP: &str = "tools/ironclaw_rune/target/ZLO.app";
/// The scripts that swap a built ZLO.app in (quit, replace, reopen), staged
/// with the app so the install never depends on the checkout's state then.
const INSTALL_SCRIPTS: [&str; 2] = [
    "tools/ironclaw_rune/install_app.sh",
    "tools/ironclaw_rune/running.sh",
];
/// Tells `running.sh` the pid of the detached updater, where its walk up
/// the process tree stops: the ZLO above it is what the install quits.
const UPDATER_PID_ENV: &str = "ZLO_UPDATER_PID";

/// How this host runs IronClaw, read from the backend binary's path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstallKind {
    /// `…/ZLO.app/Contents/MacOS/zlo-backend`: rebuilt by the repo's
    /// `tools/ironclaw_rune/bundle.sh`, which installs over `app` (wherever
    /// that bundle lives, `ZLO_INSTALL_DEST`) and reopens it; ZLO then
    /// starts its new backend.
    Zlo {
        bundle_script: PathBuf,
        app: PathBuf,
    },
    /// A plain `zlo` binary (or a pre-ZLO `ironclaw` one): `cargo build
    /// --release -p ironclaw` (the package keeps that name; its binary is
    /// `zlo`), then the file at `target` is replaced atomically.
    Binary { target: PathBuf },
}

impl InstallKind {
    /// What the binary at `install_path` is part of: ZLO.app, or a plain
    /// backend.
    pub(crate) fn scope_of(install_path: &Path) -> Scope {
        let in_app_bundle = install_path
            .parent()
            .filter(|dir| dir.ends_with("Contents/MacOS"))
            .and_then(|dir| dir.parent())
            .and_then(Path::parent)
            .is_some_and(|app| app.extension().is_some_and(|ext| ext == "app"));
        if in_app_bundle && install_path.file_name().is_some_and(|n| n == "zlo-backend") {
            Scope::App
        } else {
            Scope::Backend
        }
    }

    pub(crate) fn scope(&self) -> Scope {
        match self {
            Self::Zlo { .. } => Scope::App,
            Self::Binary { .. } => Scope::Backend,
        }
    }

    pub(crate) fn detect(install_path: &Path, repo: &Path) -> Self {
        // `scope_of` checked the shape `<app>/Contents/MacOS/zlo-backend`.
        let app = install_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .filter(|_| Self::scope_of(install_path) == Scope::App);
        if let Some(app) = app {
            Self::Zlo {
                bundle_script: repo.join("tools/ironclaw_rune/bundle.sh"),
                app: app.to_path_buf(),
            }
        } else {
            Self::Binary {
                target: install_path.to_path_buf(),
            }
        }
    }
}

/// After the build: did something restart the backend?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Restarted {
    /// `bundle.sh` reopened ZLO, which starts its new backend.
    ByZlo,
    /// The installed `zlo service` was restarted.
    Service,
    /// Nothing manages the backend here; someone restarts `zlo serve`.
    No,
}

/// The side-effecting half of a job. The production impl runs `cargo` /
/// `bundle.sh`, replaces the binary or app and restarts the service; tests
/// record.
pub(crate) trait UpdateHost {
    /// Build from `repo` into `staging` (an empty folder), writing tool
    /// output to `log`. Installs nothing: the running version keeps going.
    fn build_staged(
        &self,
        kind: &InstallKind,
        repo: &Path,
        staging: &Path,
        log: &std::fs::File,
    ) -> anyhow::Result<()>;

    /// Install the build [`Self::build_staged`] left in `staging` over
    /// `kind`.
    fn install_staged(
        &self,
        kind: &InstallKind,
        repo: &Path,
        staging: &Path,
        log: &std::fs::File,
    ) -> anyhow::Result<()>;

    /// Restart what runs the backend, after a successful install.
    fn restart(&self, kind: &InstallKind) -> anyhow::Result<Restarted>;
}

/// Everything one job needs, all from local config and the process itself.
pub(crate) struct JobInput<'a> {
    pub job: UpdateJob,
    pub paths: &'a UpdatePaths,
    pub settings: &'a SourceUpdateSettings,
    /// The commit the running build came from, when known.
    pub running_commit: Option<&'a str>,
    pub install: InstallKind,
}

/// Why a job did not run at all (nothing recorded).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum JobRefused {
    AlreadyRunning,
}

/// Run one job to its end and return the record it left in `run.json`.
pub(crate) fn run_job(
    input: JobInput<'_>,
    host: &dyn UpdateHost,
    progress: &mut dyn FnMut(&str),
) -> anyhow::Result<Result<RunRecord, JobRefused>> {
    let Some(lock) = records::lock_for_job(input.paths)? else {
        return Ok(Err(JobRefused::AlreadyRunning));
    };
    run_locked(&lock, input, host, progress).map(Ok)
}

/// [`run_job`] for a caller that already holds the lock (the detached job,
/// which reads `[update]` only once it has it).
pub(crate) fn run_locked(
    _lock: &records::UpdateLock,
    input: JobInput<'_>,
    host: &dyn UpdateHost,
    progress: &mut dyn FnMut(&str),
) -> anyhow::Result<RunRecord> {
    let mut log = records::fresh_log(input.paths)?;
    let first = match input.job {
        UpdateJob::Install => "Installing the downloaded update",
        UpdateJob::Update | UpdateJob::Download => "Fetching the source",
    };
    let mut record = RunRecord::running(first).for_job(input.job);
    let mut step = |record: &mut RunRecord, message: &str, log: &mut std::fs::File| {
        record.message = Some(message.to_string());
        if let Err(error) = writeln!(log, "== {message}") {
            tracing::debug!(%error, "update: could not append to the run log");
        }
        progress(message);
        records::write_run(input.paths, record)
    };
    step(&mut record, first, &mut log)?;
    let outcome = steps(&input, host, &mut record, &mut log, &mut step);
    let finished = match outcome {
        Ok(message) => record.finish(UpdateRunState::Succeeded, message),
        Err(error) => {
            if let Err(error) = writeln!(log, "!! {error:#}") {
                tracing::debug!(%error, "update: could not append to the run log");
            }
            record.finish(UpdateRunState::Failed, format!("{error}"))
        }
    };
    records::write_run(input.paths, &finished)?;
    progress(finished.message.as_deref().unwrap_or_default());
    Ok(finished)
}

type Step<'s> = dyn FnMut(&mut RunRecord, &str, &mut std::fs::File) -> anyhow::Result<()> + 's;

fn steps(
    input: &JobInput<'_>,
    host: &dyn UpdateHost,
    record: &mut RunRecord,
    log: &mut std::fs::File,
    step: &mut Step<'_>,
) -> anyhow::Result<String> {
    match input.job {
        UpdateJob::Install => return install(input, host, record, log, step),
        UpdateJob::Update | UpdateJob::Download => {}
    }
    let head = match fetch(input)? {
        Fetched::Nothing(message) => {
            // The running build is as new as the source: a staged build
            // has nothing left to give.
            records::clear_staged(input.paths)?;
            return Ok(message);
        }
        Fetched::Build(head) => head,
    };
    let short = short(&head);
    if input.job == UpdateJob::Download
        && records::read_staged(input.paths)?
            .and_then(|staged| staged.offer(input.running_commit))
            .is_some_and(|staged| staged.commit == head)
    {
        return Ok(format!("{short} is downloaded. Restart to install it."));
    }
    step(
        record,
        &format!("Building {short} (this takes a few minutes)"),
        log,
    )?;
    stage(input, host, &head, log)
        .map_err(|e| anyhow::anyhow!("The build failed: {e:#}. See the log."))?;
    if input.job == UpdateJob::Download {
        return Ok(format!("Downloaded {short}. Restart to install it."));
    }
    install(input, host, record, log, step)
}

/// What the fetch found.
enum Fetched {
    /// Nothing to build; the outcome line.
    Nothing(String),
    /// The fast-forwarded head, which this computer needs built.
    Build(String),
}

fn fetch(input: &JobInput<'_>) -> anyhow::Result<Fetched> {
    let checkout = Checkout::open(&input.settings.repo).map_err(anyhow::Error::new)?;
    let tracking = checkout
        .tracking(input.settings.branch.as_deref())
        .map_err(anyhow::Error::new)?;
    checkout.fetch(&tracking).map_err(anyhow::Error::new)?;
    let head = checkout
        .fast_forward(&tracking)
        .map_err(anyhow::Error::new)?;
    let short = short(&head);
    if input.running_commit == Some(head.as_str()) {
        return Ok(Fetched::Nothing(format!("Already up to date ({short}).")));
    }
    // New commits that change nothing this computer runs (the app's sources
    // on a plain backend, docs anywhere): no rebuild, no restart.
    if let Some(running) = input.running_commit
        && !checkout
            .changes_install(input.install.scope(), running, &head)
            .map_err(anyhow::Error::new)?
    {
        return Ok(Fetched::Nothing(format!(
            "Up to date: nothing new for this computer (source is at {short})."
        )));
    }
    Ok(Fetched::Build(head))
}

/// Build `head` into a fresh staging folder and record it there. The old
/// staged build (if any) is forgotten first, so a failed build never leaves
/// a record naming a half-built folder.
fn stage(
    input: &JobInput<'_>,
    host: &dyn UpdateHost,
    head: &str,
    log: &std::fs::File,
) -> anyhow::Result<()> {
    records::clear_staged(input.paths)?;
    let staging = input.paths.staged_dir();
    std::fs::create_dir_all(&staging).with_context(|| format!("create {}", staging.display()))?;
    host.build_staged(&input.install, &input.settings.repo, &staging, log)?;
    // The checkout must still be at `head`: the build read it, and the
    // record names it. A checkout moved under the build (by hand) would
    // stage one commit's build under another's name.
    let built = Checkout::open(&input.settings.repo)
        .and_then(|c| c.head())
        .map_err(anyhow::Error::new)?;
    if built != head {
        anyhow::bail!(
            "the checkout moved to {} while {} was building; try again",
            short(&built),
            short(head)
        );
    }
    records::write_staged(
        input.paths,
        &StagedRecord {
            commit: head.to_string(),
            staged_at: chrono::Utc::now(),
            base: input.running_commit.map(str::to_string),
        },
    )
}

/// Install the staged build and restart.
fn install(
    input: &JobInput<'_>,
    host: &dyn UpdateHost,
    record: &mut RunRecord,
    log: &mut std::fs::File,
    step: &mut Step<'_>,
) -> anyhow::Result<String> {
    let Some(downloaded) = records::read_staged(input.paths)? else {
        anyhow::bail!("Nothing is downloaded to install. Download the update first.");
    };
    // Downloaded over another build (something else got installed since):
    // installing it could go backwards. It is spent either way.
    let Some(staged) = downloaded.offer(input.running_commit) else {
        records::clear_staged(input.paths)?;
        anyhow::bail!(
            "The downloaded update ({}) no longer applies to the build running here. Download it again.",
            short(&downloaded.commit)
        );
    };
    let short = short(&staged.commit).to_string();
    step(record, &format!("Installing {short}; restarting"), log)?;
    host.install_staged(
        &input.install,
        &input.settings.repo,
        &input.paths.staged_dir(),
        log,
    )
    .map_err(|e| anyhow::anyhow!("Installing {short} failed: {e:#}. See the log."))?;
    // Installed: the staged copy is spent (the install copied it).
    records::clear_staged(input.paths)?;
    let restarted = host
        .restart(&input.install)
        .map_err(|e| anyhow::anyhow!("Installed {short}, but the restart failed: {e:#}"))?;
    Ok(match restarted {
        Restarted::ByZlo => format!("Updated to {short}; ZLO reopened."),
        Restarted::Service => format!("Updated to {short}; IronClaw restarted."),
        Restarted::No => {
            format!("Updated to {short}. Restart `zlo serve` on this machine to run it.")
        }
    })
}

pub(crate) fn short(commit: &str) -> &str {
    commit.get(..9).unwrap_or(commit)
}

// ── the detached job ─────────────────────────────────────────────────────

/// The command line that starts a detached job. Every piece comes from the
/// running process (its own path, its Reborn home and profile) — nothing
/// from a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetachedCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub env: Vec<(String, OsString)>,
}

pub(crate) fn detached_command(
    job: UpdateJob,
    updater: &Path,
    install_path: &Path,
    reborn_home: &Path,
    profile: Option<OsString>,
    under_systemd: bool,
) -> DetachedCommand {
    let mut env = vec![(
        REBORN_HOME_ENV.to_string(),
        reborn_home.as_os_str().to_owned(),
    )];
    if let Some(profile) = profile {
        env.push((REBORN_PROFILE_ENV.to_string(), profile));
    }
    let mut job_args: Vec<OsString> = vec![
        "update".into(),
        "--run-job".into(),
        "--install-path".into(),
        install_path.as_os_str().to_owned(),
    ];
    // A full update keeps the command line older updaters understand.
    if let Some(kind) = job_flag(job) {
        job_args.extend(["--job".into(), kind.into()]);
    }
    if under_systemd {
        // A unit of its own, so restarting the backend's unit (whose
        // default KillMode stops its whole cgroup) does not stop the job.
        let mut args: Vec<OsString> = vec![
            "--user".into(),
            "--collect".into(),
            "--quiet".into(),
            format!("--unit=zlo-update-{}", chrono::Utc::now().timestamp()).into(),
        ];
        for (key, value) in &env {
            let mut pair = OsString::from(format!("--setenv={key}="));
            pair.push(value);
            args.push(pair);
        }
        args.push(updater.as_os_str().to_owned());
        args.extend(job_args);
        return DetachedCommand {
            program: PathBuf::from("systemd-run"),
            args,
            env: Vec::new(),
        };
    }
    DetachedCommand {
        program: updater.to_path_buf(),
        args: job_args,
        env,
    }
}

/// `--job`'s value for `job`, `None` for a full update (the default).
pub(crate) fn job_flag(job: UpdateJob) -> Option<&'static str> {
    match job {
        UpdateJob::Update => None,
        UpdateJob::Download => Some("download"),
        UpdateJob::Install => Some("install"),
    }
}

/// Copy `exe` to the updater path (atomically) and start the job detached.
pub(crate) fn spawn_detached(
    job: UpdateJob,
    paths: &UpdatePaths,
    exe: &Path,
    reborn_home: &Path,
) -> anyhow::Result<()> {
    paths.ensure_dir()?;
    let updater = paths.updater_binary();
    copy_executable(exe, &updater)?;
    let under_systemd = cfg!(target_os = "linux") && std::env::var_os("INVOCATION_ID").is_some();
    let spec = detached_command(
        job,
        &updater,
        exe,
        reborn_home,
        ironclaw_config::profile_env_value(),
        under_systemd,
    );
    let log = records::append_log(paths)?;
    let log_err = log.try_clone().context("clone the update log handle")?;
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .current_dir(paths.dir())
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command
        .spawn()
        .with_context(|| format!("start the update job ({})", spec.program.display()))?;
    Ok(())
}

/// Copy an executable next to `dest` and rename it into place, mode 0755.
pub(crate) fn copy_executable(src: &Path, dest: &Path) -> anyhow::Result<()> {
    let dir = dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", dest.display()))?;
    let tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("create temp file in {}", dir.display()))?;
    std::fs::copy(src, tmp.path())
        .with_context(|| format!("copy {} to {}", src.display(), tmp.path().display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("set permissions on {}", tmp.path().display()))?;
    }
    tmp.persist(dest)
        .map_err(|e| e.error)
        .with_context(|| format!("replace {}", dest.display()))?;
    Ok(())
}

// ── the production host ──────────────────────────────────────────────────

/// What `bundle.sh` needs to rebuild `app` in place: its destination (a
/// moved ZLO.app is rebuilt where it is, not into `~/Applications`) and,
/// when the installed app is signed ad-hoc (`install.sh` on a Mac with no
/// Apple Development identity), permission to sign the new one that way.
pub(crate) fn bundle_env(app: &Path, adhoc: bool) -> Vec<(&'static str, OsString)> {
    let mut env = vec![("ZLO_INSTALL_DEST", app.as_os_str().to_owned())];
    if adhoc {
        env.push(("ZLO_ALLOW_ADHOC", "1".into()));
    }
    env
}

/// Whether `codesign -dv` says `app` is signed ad-hoc. Unreadable reads as
/// no: `bundle.sh` then asks for an identity, as it always did.
fn signed_adhoc(app: &Path) -> bool {
    match Command::new("/usr/bin/codesign")
        .arg("-dv")
        .arg(app)
        .stdin(Stdio::null())
        .output()
    {
        Ok(out) => is_adhoc_signature(&String::from_utf8_lossy(&out.stderr)),
        Err(error) => {
            tracing::debug!(%error, "update: could not read the app's signature");
            false
        }
    }
}

/// `codesign -dv` output (on stderr) for an ad-hoc signature carries
/// `Signature=adhoc` and the `adhoc` code-directory flag.
pub(crate) fn is_adhoc_signature(codesign_dv: &str) -> bool {
    codesign_dv.lines().any(|line| {
        let line = line.trim();
        line == "Signature=adhoc" || (line.starts_with("CodeDirectory") && line.contains("(adhoc"))
    })
}

/// A plain binary is replaced at the path it was started from. When that
/// path is itself a symlink, replacing it would turn the link into a copy
/// and leave the binary it pointed at stale, so refuse and name the binary.
pub(crate) fn refuse_symlinked_binary(target: &Path) -> anyhow::Result<()> {
    let is_link = std::fs::symlink_metadata(target)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false); // silent-ok: a missing target is created by the copy; nothing to refuse
    if is_link {
        let real = std::fs::canonicalize(target)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "its target".into()); // silent-ok: only names the target in the message
        anyhow::bail!(
            "{} is a symlink to {real}; update will not replace a link with a copy. \
             Run `{real} update` to update that binary.",
            target.display()
        );
    }
    Ok(())
}

/// Runs the real build, install and restart.
pub(crate) struct SystemUpdateHost;

impl SystemUpdateHost {
    /// A child that writes to the log, with a `PATH` that finds `cargo`
    /// and Node even when the backend was started by a GUI app, and no
    /// inherited `CARGO_*` build variables steering the build elsewhere.
    fn command(program: &str, repo: &Path, log: &std::fs::File) -> anyhow::Result<Command> {
        let mut command = Command::new(program);
        command
            .current_dir(repo)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log.try_clone()?)
            .env("PATH", build_path());
        for (key, _) in std::env::vars_os() {
            let Some(key) = key.to_str() else { continue };
            let keep = matches!(key, "CARGO_HOME");
            // `ZLO_LIGHT` would build the Light app; `ZLO_BACKEND` would
            // copy some other backend in.
            if (key.starts_with("CARGO_") && !keep) || matches!(key, "ZLO_BACKEND" | "ZLO_LIGHT") {
                command.env_remove(key);
            }
        }
        Ok(command)
    }
}

fn build_path() -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".cargo/bin"));
    }
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.extend(["/usr/bin", "/bin", "/usr/sbin", "/sbin"].map(PathBuf::from));
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    std::env::join_paths(dirs).unwrap_or_default() // silent-ok: a PATH entry with ':' is dropped to the default lookup.
}

fn run_checked(mut command: Command, what: &str) -> anyhow::Result<()> {
    let status = command
        .status()
        .with_context(|| format!("could not run {what}"))?;
    if !status.success() {
        anyhow::bail!("{what} exited with {status}");
    }
    Ok(())
}

impl UpdateHost for SystemUpdateHost {
    fn build_staged(
        &self,
        kind: &InstallKind,
        repo: &Path,
        staging: &Path,
        log: &std::fs::File,
    ) -> anyhow::Result<()> {
        match kind {
            InstallKind::Zlo { bundle_script, app } => {
                if !bundle_script.is_file() {
                    anyhow::bail!("{} is missing from the checkout", bundle_script.display());
                }
                // An earlier build's app must never pass for this one.
                let built = repo.join(BUILT_APP);
                if built.exists() {
                    std::fs::remove_dir_all(&built)
                        .with_context(|| format!("remove {}", built.display()))?;
                }
                let mut command = Self::command("/bin/sh", repo, log)?;
                command.arg(bundle_script).arg("--no-install");
                command.envs(bundle_env(app, signed_adhoc(app)));
                run_checked(command, "bundle.sh")?;
                if !built.join("Contents/MacOS/zlo").is_file() {
                    anyhow::bail!("bundle.sh left no app at {}", built.display());
                }
                // A copy of its own: a later build in the checkout must not
                // change what waits to be installed. `ditto` keeps the
                // signature intact.
                let mut copy = Self::command("/usr/bin/ditto", repo, log)?;
                copy.arg(&built).arg(staging.join("ZLO.app"));
                run_checked(copy, "ditto")?;
                for script in INSTALL_SCRIPTS {
                    let from = repo.join(script);
                    let name = from
                        .file_name()
                        .ok_or_else(|| anyhow::anyhow!("{script} has no file name"))?;
                    std::fs::copy(&from, staging.join(name))
                        .with_context(|| format!("stage {}", from.display()))?;
                }
                Ok(())
            }
            InstallKind::Binary { target } => {
                refuse_symlinked_binary(target)?;
                let mut command = Self::command("cargo", repo, log)?;
                command.args(["build", "--release", "-p", "ironclaw", "--bin", "zlo"]);
                run_checked(command, "cargo build")?;
                copy_executable(&repo.join(BUILT_BINARY), &staging.join("zlo"))
            }
        }
    }

    fn install_staged(
        &self,
        kind: &InstallKind,
        repo: &Path,
        staging: &Path,
        log: &std::fs::File,
    ) -> anyhow::Result<()> {
        match kind {
            InstallKind::Zlo { app, .. } => {
                let built = staging.join("ZLO.app");
                if !built.is_dir() {
                    anyhow::bail!("the downloaded app is missing ({})", built.display());
                }
                // The scripts staged with the app, never the checkout's
                // (it may have moved on, or back, since the download).
                let script = staging.join("install_app.sh");
                if !script.is_file() || !staging.join("running.sh").is_file() {
                    anyhow::bail!("the downloaded update is missing its install script");
                }
                let mut command = Self::command("/bin/sh", repo, log)?;
                command
                    .arg(script)
                    .arg(&built)
                    .arg(app)
                    .env(UPDATER_PID_ENV, std::process::id().to_string());
                run_checked(command, "install_app.sh")
            }
            InstallKind::Binary { target } => {
                refuse_symlinked_binary(target)?;
                let built = staging.join("zlo");
                if !built.is_file() {
                    anyhow::bail!("the downloaded binary is missing ({})", built.display());
                }
                copy_executable(&built, target)
            }
        }
    }

    fn restart(&self, kind: &InstallKind) -> anyhow::Result<Restarted> {
        match kind {
            InstallKind::Zlo { .. } => Ok(Restarted::ByZlo),
            InstallKind::Binary { .. } => {
                if crate::commands::service::restart_if_installed()? {
                    Ok(Restarted::Service)
                } else {
                    Ok(Restarted::No)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::super::git::tests::{commit, push_upstream, repos};
    use super::*;

    #[derive(Default)]
    struct RecordingHost {
        builds: RefCell<Vec<InstallKind>>,
        /// What each install found in the staging folder.
        installs: RefCell<Vec<Vec<u8>>>,
        fail_build: bool,
        restarted: Option<Restarted>,
    }

    impl UpdateHost for RecordingHost {
        fn build_staged(
            &self,
            kind: &InstallKind,
            _repo: &Path,
            staging: &Path,
            log: &std::fs::File,
        ) -> anyhow::Result<()> {
            self.builds.borrow_mut().push(kind.clone());
            writeln!(&*log, "Finished `release` profile")?;
            if self.fail_build {
                anyhow::bail!("cargo build exited with exit status: 101");
            }
            let n = self.builds.borrow().len();
            std::fs::write(staging.join("zlo"), format!("build {n}"))?;
            Ok(())
        }

        fn install_staged(
            &self,
            _kind: &InstallKind,
            _repo: &Path,
            staging: &Path,
            _log: &std::fs::File,
        ) -> anyhow::Result<()> {
            self.installs
                .borrow_mut()
                .push(std::fs::read(staging.join("zlo"))?);
            Ok(())
        }

        fn restart(&self, _kind: &InstallKind) -> anyhow::Result<Restarted> {
            Ok(self.restarted.unwrap_or(Restarted::No))
        }
    }

    fn binary() -> InstallKind {
        InstallKind::Binary {
            target: PathBuf::from("/opt/ironclaw/bin/ironclaw"),
        }
    }

    #[test]
    fn install_kind_follows_the_backend_path() {
        let repo = Path::new("/src/ironclaw");
        assert_eq!(
            InstallKind::detect(
                Path::new("/Users/me/Applications/ZLO.app/Contents/MacOS/zlo-backend"),
                repo
            ),
            InstallKind::Zlo {
                bundle_script: PathBuf::from("/src/ironclaw/tools/ironclaw_rune/bundle.sh"),
                app: PathBuf::from("/Users/me/Applications/ZLO.app"),
            }
        );
        for plain in [
            "/Users/me/.ironclaw/bin/ironclaw",
            "/Users/me/zlo-backend",
            "/Users/me/ZLO.app/Contents/MacOS/zlo",
        ] {
            assert_eq!(
                InstallKind::detect(Path::new(plain), repo),
                InstallKind::Binary {
                    target: PathBuf::from(plain)
                },
                "{plain}"
            );
        }
    }

    #[test]
    fn a_moved_or_ad_hoc_signed_app_is_rebuilt_where_it_is_and_the_same_way() {
        let moved = Path::new("/Volumes/Work/Apps/ZLO.app/Contents/MacOS/zlo-backend");
        let InstallKind::Zlo { app, .. } = InstallKind::detect(moved, Path::new("/src")) else {
            panic!("a bundle backend is a ZLO install");
        };
        assert_eq!(app, Path::new("/Volumes/Work/Apps/ZLO.app"));
        assert_eq!(
            bundle_env(&app, false),
            vec![(
                "ZLO_INSTALL_DEST",
                OsString::from("/Volumes/Work/Apps/ZLO.app")
            )]
        );
        assert!(bundle_env(&app, true).contains(&("ZLO_ALLOW_ADHOC", OsString::from("1"))));

        let adhoc = "Executable=/x/ZLO.app/Contents/MacOS/zlo\nCodeDirectory v=20400 size=1 flags=0x2(adhoc) hashes=1+7 location=embedded\nSignature=adhoc\nTeamIdentifier=not set\n";
        assert!(is_adhoc_signature(adhoc));
        let developer = "Executable=/x/ZLO.app/Contents/MacOS/zlo\nCodeDirectory v=20500 size=1 flags=0x0(none) hashes=1+7 location=embedded\nAuthority=Apple Development: Someone (ABCDE12345)\nTeamIdentifier=ABCDE12345\n";
        assert!(!is_adhoc_signature(developer));
        assert!(!is_adhoc_signature(
            "/x/ZLO.app: code object is not signed at all"
        ));
    }

    #[test]
    fn a_standalone_binary_is_replaced_at_its_own_path_and_a_symlink_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let real = dir.path().join("zlo");
        std::fs::write(&real, b"bin").expect("binary");
        let link = dir.path().join("zlo-link");
        std::os::unix::fs::symlink(&real, &link).expect("link");

        // The path as started is kept for a standalone binary, never its
        // resolved target; only a bundle backend resolves through the link.
        assert_eq!(super::super::install_path_for(real.clone()), real);
        assert_eq!(super::super::install_path_for(link.clone()), link);
        assert!(refuse_symlinked_binary(&real).is_ok());
        let refused = refuse_symlinked_binary(&link).expect_err("a symlink is refused");
        assert!(refused.to_string().contains("is a symlink"), "{refused}");
        // And the refusal comes before any build: nothing was run or replaced.
        let log = tempfile::tempfile().expect("log");
        let kind = InstallKind::Binary {
            target: link.clone(),
        };
        let err = SystemUpdateHost
            .build_staged(&kind, dir.path(), dir.path(), &log)
            .expect_err("refused");
        assert!(err.to_string().contains("is a symlink"), "{err}");
        let staging = dir.path().join("staged");
        std::fs::create_dir_all(&staging).expect("staging");
        std::fs::write(staging.join("zlo"), b"new").expect("staged");
        let err = SystemUpdateHost
            .install_staged(&kind, dir.path(), &staging, &log)
            .expect_err("refused");
        assert!(err.to_string().contains("is a symlink"), "{err}");
        assert!(
            std::fs::symlink_metadata(&link)
                .expect("link")
                .file_type()
                .is_symlink()
        );

        // A link into a ZLO.app resolves to the bundle, so update rebuilds the app.
        let macos = dir.path().join("Apps/ZLO.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).expect("bundle");
        std::fs::write(macos.join("zlo-backend"), b"bin").expect("backend");
        let cli = dir.path().join("bin/zlo");
        std::fs::create_dir_all(cli.parent().expect("bin")).expect("bin");
        std::os::unix::fs::symlink(macos.join("zlo-backend"), &cli).expect("cli link");
        let resolved = super::super::install_path_for(cli);
        assert_eq!(
            resolved,
            std::fs::canonicalize(macos.join("zlo-backend")).expect("canonical")
        );
        assert_eq!(InstallKind::scope_of(&resolved), Scope::App);
    }

    #[test]
    fn a_job_fast_forwards_builds_and_records_success() {
        let r = repos();
        let built = super::super::git::tests::sh(&r.checkout, &["rev-parse", "HEAD"]);
        let tip = push_upstream(&r, &["New feature"]);
        let home = tempfile::tempdir().expect("home");
        let paths = UpdatePaths::new(home.path());
        let settings = SourceUpdateSettings {
            repo: r.checkout.clone(),
            branch: None,
        };
        let host = RecordingHost {
            restarted: Some(Restarted::Service),
            ..Default::default()
        };
        let mut steps = Vec::new();
        let record = run_job(
            JobInput {
                job: UpdateJob::Update,
                paths: &paths,
                settings: &settings,
                running_commit: Some(&built),
                install: binary(),
            },
            &host,
            &mut |m| steps.push(m.to_string()),
        )
        .expect("runs")
        .expect("not locked");
        assert_eq!(record.state, UpdateRunState::Succeeded);
        let message = record.message.clone().expect("message");
        assert!(
            message.contains(short(&tip)) && message.contains("restarted"),
            "{message}"
        );
        assert_eq!(*host.builds.borrow(), vec![binary()]);
        assert_eq!(*host.installs.borrow(), vec![b"build 1".to_vec()]);
        assert_eq!(
            records::read_staged(&paths).expect("staged"),
            None,
            "a full update leaves nothing staged"
        );
        assert_eq!(
            super::super::git::tests::sh(&r.checkout, &["rev-parse", "HEAD"]),
            tip
        );
        assert_eq!(records::read_run(&paths).expect("read"), Some(record));
        assert!(
            records::log_tail(&paths)
                .expect("tail")
                .contains("Finished `release` profile")
        );
        assert!(steps.iter().any(|s| s.starts_with("Building")), "{steps:?}");
    }

    #[test]
    fn a_download_builds_without_installing_and_install_swaps_it_in() {
        let r = repos();
        let built = super::super::git::tests::sh(&r.checkout, &["rev-parse", "HEAD"]);
        let tip = push_upstream(&r, &["New feature"]);
        let home = tempfile::tempdir().expect("home");
        let paths = UpdatePaths::new(home.path());
        let settings = SourceUpdateSettings {
            repo: r.checkout.clone(),
            branch: None,
        };
        let host = RecordingHost {
            restarted: Some(Restarted::ByZlo),
            ..Default::default()
        };
        let run = |job: UpdateJob| {
            run_job(
                JobInput {
                    job,
                    paths: &paths,
                    settings: &settings,
                    running_commit: Some(&built),
                    install: binary(),
                },
                &host,
                &mut |_| {},
            )
            .expect("runs")
            .expect("not locked")
        };

        // Install before anything is downloaded: refused, nothing touched.
        let early = run(UpdateJob::Install);
        assert_eq!(early.state, UpdateRunState::Failed);
        assert_eq!(early.job, UpdateJob::Install);
        assert!(
            early
                .message
                .expect("message")
                .contains("Nothing is downloaded"),
            "says why"
        );
        assert!(host.installs.borrow().is_empty());

        // Download: built and staged, nothing installed or restarted.
        let downloaded = run(UpdateJob::Download);
        assert_eq!(downloaded.state, UpdateRunState::Succeeded);
        assert_eq!(downloaded.job, UpdateJob::Download);
        assert!(
            downloaded
                .message
                .expect("message")
                .contains("Restart to install"),
        );
        assert_eq!(host.builds.borrow().len(), 1);
        assert!(host.installs.borrow().is_empty(), "nothing installed yet");
        let staged = records::read_staged(&paths).expect("read").expect("staged");
        assert_eq!(staged.commit, tip);

        // A second download of the same commit builds nothing.
        let again = run(UpdateJob::Download);
        assert!(again.message.expect("message").contains("is downloaded"));
        assert_eq!(host.builds.borrow().len(), 1, "not rebuilt");

        // Install: the staged build goes in, then the restart.
        let installed = run(UpdateJob::Install);
        assert_eq!(installed.state, UpdateRunState::Succeeded);
        assert_eq!(installed.job, UpdateJob::Install);
        let message = installed.message.expect("message");
        assert!(
            message.contains(short(&tip)) && message.contains("ZLO reopened"),
            "{message}"
        );
        assert_eq!(*host.installs.borrow(), vec![b"build 1".to_vec()]);
        assert_eq!(host.builds.borrow().len(), 1, "install builds nothing");
        assert_eq!(records::read_staged(&paths).expect("read"), None, "spent");
    }

    #[test]
    fn a_failed_download_leaves_no_staged_build_and_an_up_to_date_one_drops_it() {
        let r = repos();
        let built = super::super::git::tests::sh(&r.checkout, &["rev-parse", "HEAD"]);
        let tip = push_upstream(&r, &["New feature"]);
        let home = tempfile::tempdir().expect("home");
        let paths = UpdatePaths::new(home.path());
        let settings = SourceUpdateSettings {
            repo: r.checkout.clone(),
            branch: None,
        };
        fn download<'a>(
            paths: &'a UpdatePaths,
            settings: &'a SourceUpdateSettings,
            running: &'a str,
        ) -> JobInput<'a> {
            JobInput {
                job: UpdateJob::Download,
                paths,
                settings,
                running_commit: Some(running),
                install: binary(),
            }
        }
        let input = |running| download(&paths, &settings, running);
        // An older staged build is replaced, not kept, when the build fails.
        std::fs::create_dir_all(paths.staged_dir()).expect("dir");
        records::write_staged(
            &paths,
            &StagedRecord {
                commit: "c".repeat(40),
                staged_at: chrono::Utc::now(),
                base: None,
            },
        )
        .expect("old staged");
        let failing = RecordingHost {
            fail_build: true,
            ..Default::default()
        };
        let failed = run_job(input(&built), &failing, &mut |_| {})
            .expect("runs")
            .expect("not locked");
        assert_eq!(failed.state, UpdateRunState::Failed);
        assert_eq!(records::read_staged(&paths).expect("read"), None);

        // Staged, then the running build catches up (installed some other
        // way): the next download drops the staged copy.
        let host = RecordingHost::default();
        run_job(input(&built), &host, &mut |_| {})
            .expect("runs")
            .expect("not locked");
        assert!(records::read_staged(&paths).expect("read").is_some());
        let caught_up = run_job(input(&tip), &host, &mut |_| {})
            .expect("runs")
            .expect("not locked");
        assert!(
            caught_up
                .message
                .expect("message")
                .contains("Already up to date")
        );
        assert_eq!(records::read_staged(&paths).expect("read"), None);
    }

    #[test]
    fn a_download_over_another_build_is_never_installed() {
        let r = repos();
        let home = tempfile::tempdir().expect("home");
        let paths = UpdatePaths::new(home.path());
        let settings = SourceUpdateSettings {
            repo: r.checkout.clone(),
            branch: None,
        };
        // Downloaded over A; then C got installed some other way.
        std::fs::create_dir_all(paths.staged_dir()).expect("dir");
        records::write_staged(
            &paths,
            &StagedRecord {
                commit: "b".repeat(40),
                staged_at: chrono::Utc::now(),
                base: Some("a".repeat(40)),
            },
        )
        .expect("staged");
        let host = RecordingHost::default();
        let running = "c".repeat(40);
        let refused = run_job(
            JobInput {
                job: UpdateJob::Install,
                paths: &paths,
                settings: &settings,
                running_commit: Some(&running),
                install: binary(),
            },
            &host,
            &mut |_| {},
        )
        .expect("runs")
        .expect("not locked");
        assert_eq!(refused.state, UpdateRunState::Failed);
        assert!(
            refused
                .message
                .expect("message")
                .contains("no longer applies"),
        );
        assert!(host.installs.borrow().is_empty(), "nothing went backwards");
        assert_eq!(records::read_staged(&paths).expect("read"), None, "spent");
    }

    #[test]
    fn an_up_to_date_build_is_not_rebuilt() {
        let r = repos();
        let head = super::super::git::tests::sh(&r.checkout, &["rev-parse", "HEAD"]);
        let home = tempfile::tempdir().expect("home");
        let paths = UpdatePaths::new(home.path());
        let settings = SourceUpdateSettings {
            repo: r.checkout.clone(),
            branch: None,
        };
        let host = RecordingHost::default();
        let record = run_job(
            JobInput {
                job: UpdateJob::Update,
                paths: &paths,
                settings: &settings,
                running_commit: Some(&head),
                install: binary(),
            },
            &host,
            &mut |_| {},
        )
        .expect("runs")
        .expect("not locked");
        assert_eq!(record.state, UpdateRunState::Succeeded);
        assert!(
            record
                .message
                .expect("message")
                .contains("Already up to date")
        );
        assert!(host.builds.borrow().is_empty());
    }

    #[test]
    fn commits_that_change_nothing_here_are_pulled_but_not_built() {
        let r = repos();
        let built = super::super::git::tests::sh(&r.checkout, &["rev-parse", "HEAD"]);
        std::fs::create_dir_all(r.other.join("tools/ironclaw_rune/app")).expect("dirs");
        let tip = commit(
            &r.other,
            "tools/ironclaw_rune/app/main.rs",
            "app\n",
            "App only",
        );
        super::super::git::tests::sh(&r.other, &["push", "-q", "origin", "main"]);
        let home = tempfile::tempdir().expect("home");
        let paths = UpdatePaths::new(home.path());
        let settings = SourceUpdateSettings {
            repo: r.checkout.clone(),
            branch: None,
        };
        let run = |install: InstallKind, host: &RecordingHost| {
            run_job(
                JobInput {
                    job: UpdateJob::Update,
                    paths: &paths,
                    settings: &settings,
                    running_commit: Some(&built),
                    install,
                },
                host,
                &mut |_| {},
            )
            .expect("runs")
            .expect("not locked")
        };

        // A plain backend: the app's sources are not its own.
        let host = RecordingHost::default();
        let record = run(binary(), &host);
        assert_eq!(record.state, UpdateRunState::Succeeded);
        assert!(
            record
                .message
                .expect("message")
                .contains("nothing new for this computer")
        );
        assert!(
            host.builds.borrow().is_empty(),
            "nothing built or restarted"
        );
        assert_eq!(
            super::super::git::tests::sh(&r.checkout, &["rev-parse", "HEAD"]),
            tip,
            "the checkout still follows its branch"
        );

        // ZLO.app is built from them: it rebuilds.
        let app = InstallKind::Zlo {
            bundle_script: r.checkout.join("tools/ironclaw_rune/bundle.sh"),
            app: PathBuf::from("/Users/me/Applications/ZLO.app"),
        };
        let host = RecordingHost::default();
        let record = run(app.clone(), &host);
        assert_eq!(record.state, UpdateRunState::Succeeded);
        assert_eq!(*host.builds.borrow(), vec![app]);
    }

    #[test]
    fn refusals_and_build_failures_are_recorded_as_failed() {
        let r = repos();
        push_upstream(&r, &["Upstream"]);
        commit(&r.checkout, "mine.txt", "x", "Local");
        let home = tempfile::tempdir().expect("home");
        let paths = UpdatePaths::new(home.path());
        let settings = SourceUpdateSettings {
            repo: r.checkout.clone(),
            branch: None,
        };
        let host = RecordingHost::default();
        let input = || JobInput {
            job: UpdateJob::Update,
            paths: &paths,
            settings: &settings,
            running_commit: None,
            install: binary(),
        };
        let diverged = run_job(input(), &host, &mut |_| {})
            .expect("runs")
            .expect("not locked");
        assert_eq!(diverged.state, UpdateRunState::Failed);
        assert!(
            diverged.message.expect("message").contains("fast-forward"),
            "the diverged refusal is the message"
        );
        assert!(host.builds.borrow().is_empty(), "nothing built");

        super::super::git::tests::sh(&r.checkout, &["reset", "-q", "--hard", "origin/main"]);
        push_upstream(&r, &["More"]);
        let failing = RecordingHost {
            fail_build: true,
            ..Default::default()
        };
        let failed = run_job(input(), &failing, &mut |_| {})
            .expect("runs")
            .expect("not locked");
        assert_eq!(failed.state, UpdateRunState::Failed);
        assert!(failed.message.expect("message").contains("See the log"));
        assert!(
            records::log_tail(&paths)
                .expect("tail")
                .contains("exit status: 101")
        );
    }

    #[test]
    fn a_status_probe_holding_the_lock_does_not_refuse_a_starting_job() {
        let r = repos();
        let head = super::super::git::tests::sh(&r.checkout, &["rev-parse", "HEAD"]);
        let home = tempfile::tempdir().expect("home");
        let paths = UpdatePaths::new(home.path());
        let probe = records::try_lock(&paths).expect("lock").expect("free");
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(probe);
        });
        let settings = SourceUpdateSettings {
            repo: r.checkout.clone(),
            branch: None,
        };
        let record = run_job(
            JobInput {
                job: UpdateJob::Update,
                paths: &paths,
                settings: &settings,
                running_commit: Some(&head),
                install: binary(),
            },
            &RecordingHost::default(),
            &mut |_| {},
        )
        .expect("runs")
        .expect("the job waited out the probe");
        release.join().expect("probe thread");
        assert_eq!(record.state, UpdateRunState::Succeeded);
    }

    #[test]
    fn one_job_at_a_time() {
        let r = repos();
        let home = tempfile::tempdir().expect("home");
        let paths = UpdatePaths::new(home.path());
        let _held = records::try_lock(&paths).expect("lock").expect("free");
        let settings = SourceUpdateSettings {
            repo: r.checkout.clone(),
            branch: None,
        };
        let host = RecordingHost::default();
        let refused = run_job(
            JobInput {
                job: UpdateJob::Update,
                paths: &paths,
                settings: &settings,
                running_commit: None,
                install: binary(),
            },
            &host,
            &mut |_| {},
        )
        .expect("runs");
        assert_eq!(refused, Err(JobRefused::AlreadyRunning));
        assert_eq!(
            records::read_run(&paths).expect("read"),
            None,
            "nothing recorded"
        );
    }

    #[test]
    fn the_detached_command_carries_only_local_values() {
        let spec = detached_command(
            UpdateJob::Update,
            Path::new("/home/me/.ironclaw/reborn/update/zlo-updater"),
            Path::new("/home/me/.ironclaw/bin/ironclaw"),
            Path::new("/home/me/.ironclaw/reborn"),
            None,
            false,
        );
        assert_eq!(
            spec.program,
            PathBuf::from("/home/me/.ironclaw/reborn/update/zlo-updater")
        );
        assert_eq!(
            spec.args,
            [
                "update",
                "--run-job",
                "--install-path",
                "/home/me/.ironclaw/bin/ironclaw"
            ]
            .map(OsString::from)
        );
        assert_eq!(
            spec.env,
            vec![(
                REBORN_HOME_ENV.to_string(),
                OsString::from("/home/me/.ironclaw/reborn")
            )]
        );

        let download = detached_command(
            UpdateJob::Download,
            Path::new("/u/zlo-updater"),
            Path::new("/u/ironclaw"),
            Path::new("/u/home"),
            None,
            false,
        );
        assert_eq!(
            &download.args[download.args.len() - 2..],
            ["--job", "download"].map(OsString::from)
        );
        assert_eq!(job_flag(UpdateJob::Install), Some("install"));

        let systemd = detached_command(
            UpdateJob::Update,
            Path::new("/u/zlo-updater"),
            Path::new("/u/ironclaw"),
            Path::new("/u/home"),
            Some("local".into()),
            true,
        );
        assert_eq!(systemd.program, PathBuf::from("systemd-run"));
        let args: Vec<String> = systemd
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..3], ["--user", "--collect", "--quiet"]);
        assert!(args[3].starts_with("--unit=zlo-update-"));
        assert!(args.contains(&format!("--setenv={REBORN_HOME_ENV}=/u/home")));
        assert!(args.contains(&format!("--setenv={REBORN_PROFILE_ENV}=local")));
        assert_eq!(
            &args[args.len() - 5..],
            [
                "/u/zlo-updater",
                "update",
                "--run-job",
                "--install-path",
                "/u/ironclaw"
            ]
        );
    }

    #[test]
    fn the_quit_walk_stops_at_the_updater_so_the_zlo_above_it_quits() {
        let running =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tools/ironclaw_rune/running.sh");
        // An outer shell stands in for the updater; the inner one is the
        // install script. Without the variable the walk climbs past the
        // updater into whatever started it (the ZLO that must quit).
        let walk = |updater_env: bool| {
            let inner = format!(
                ". '{}'; zlo_self_and_ancestors; echo; echo \"parent=$PPID\"",
                running.display()
            );
            // `; true` keeps the outer shell alive (no exec into the
            // inner one), as the updater stays alive under its script.
            let outer = if updater_env {
                format!("{UPDATER_PID_ENV}=$$ /bin/sh -c \"$0\"; true")
            } else {
                "/bin/sh -c \"$0\"; true".to_string()
            };
            let out = Command::new("/bin/sh")
                .args(["-c", &outer, &inner])
                .output()
                .expect("sh");
            String::from_utf8(out.stdout).expect("utf8")
        };
        let stopped = walk(true);
        let mut lines = stopped.lines();
        let pids: Vec<&str> = lines.next().expect("walk").split_whitespace().collect();
        let updater = lines
            .next()
            .and_then(|l| l.strip_prefix("parent="))
            .expect("parent");
        assert_eq!(pids.len(), 2, "self and the updater only: {stopped}");
        assert_eq!(pids[1], updater);
        let climbed = walk(false);
        let pids = climbed
            .lines()
            .next()
            .expect("walk")
            .split_whitespace()
            .count();
        assert!(pids > 2, "without it the walk climbs on: {climbed}");
    }

    #[test]
    fn executables_are_replaced_atomically() {
        let dir = tempfile::tempdir().expect("dir");
        let src = dir.path().join("new");
        let dest = dir.path().join("ironclaw");
        std::fs::write(&src, b"new build").expect("src");
        std::fs::write(&dest, b"old build").expect("dest");
        copy_executable(&src, &dest).expect("copy");
        assert_eq!(std::fs::read(&dest).expect("read"), b"new build");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dest).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o755);
        }
    }
}
