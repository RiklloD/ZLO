//! One update job: lock, fetch, fast-forward, rebuild the way this host is
//! installed, restart, and record the outcome in `run.json`.
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
use ironclaw_product_contracts::self_update::UpdateRunState;

use super::git::{Checkout, Scope};
use super::records::{self, RunRecord, UpdatePaths};

/// Where `cargo build --release -p ironclaw --bin zlo` leaves the binary.
const BUILT_BINARY: &str = "target/release/zlo";

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
/// `bundle.sh`, replaces the binary and restarts the service; tests record.
pub(crate) trait UpdateHost {
    /// Build from `repo` and install over `kind`, writing tool output to
    /// `log`.
    fn build_and_install(
        &self,
        kind: &InstallKind,
        repo: &Path,
        log: &std::fs::File,
    ) -> anyhow::Result<()>;

    /// Restart what runs the backend, after a successful install.
    fn restart(&self, kind: &InstallKind) -> anyhow::Result<Restarted>;
}

/// Everything one job needs, all from local config and the process itself.
pub(crate) struct JobInput<'a> {
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
    let mut record = RunRecord::running("Fetching the source");
    let mut step = |record: &mut RunRecord, message: &str, log: &mut std::fs::File| {
        record.message = Some(message.to_string());
        if let Err(error) = writeln!(log, "== {message}") {
            tracing::debug!(%error, "update: could not append to the run log");
        }
        progress(message);
        records::write_run(input.paths, record)
    };
    step(&mut record, "Fetching the source", &mut log)?;
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
        return Ok(format!("Already up to date ({short})."));
    }
    // New commits that change nothing this computer runs (the app's sources
    // on a plain backend, docs anywhere): no rebuild, no restart.
    if let Some(running) = input.running_commit
        && !checkout
            .changes_install(input.install.scope(), running, &head)
            .map_err(anyhow::Error::new)?
    {
        return Ok(format!(
            "Up to date: nothing new for this computer (source is at {short})."
        ));
    }
    step(
        record,
        &format!("Building {short} (this takes a few minutes)"),
        log,
    )?;
    host.build_and_install(&input.install, &input.settings.repo, log)
        .map_err(|e| anyhow::anyhow!("The build or install failed: {e:#}. See the log."))?;
    step(record, &format!("Installed {short}; restarting"), log)?;
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
    let job_args: Vec<OsString> = vec![
        "update".into(),
        "--run-job".into(),
        "--install-path".into(),
        install_path.as_os_str().to_owned(),
    ];
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

/// Copy `exe` to the updater path (atomically) and start the job detached.
pub(crate) fn spawn_detached(
    paths: &UpdatePaths,
    exe: &Path,
    reborn_home: &Path,
) -> anyhow::Result<()> {
    paths.ensure_dir()?;
    let updater = paths.updater_binary();
    copy_executable(exe, &updater)?;
    let under_systemd = cfg!(target_os = "linux") && std::env::var_os("INVOCATION_ID").is_some();
    let spec = detached_command(
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
            if (key.starts_with("CARGO_") && !keep) || key == "ZLO_BACKEND" {
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
    fn build_and_install(
        &self,
        kind: &InstallKind,
        repo: &Path,
        log: &std::fs::File,
    ) -> anyhow::Result<()> {
        match kind {
            InstallKind::Zlo { bundle_script, app } => {
                if !bundle_script.is_file() {
                    anyhow::bail!("{} is missing from the checkout", bundle_script.display());
                }
                let mut command = Self::command("/bin/sh", repo, log)?;
                command.arg(bundle_script);
                command.envs(bundle_env(app, signed_adhoc(app)));
                run_checked(command, "bundle.sh")
            }
            InstallKind::Binary { target } => {
                refuse_symlinked_binary(target)?;
                let mut command = Self::command("cargo", repo, log)?;
                command.args(["build", "--release", "-p", "ironclaw", "--bin", "zlo"]);
                run_checked(command, "cargo build")?;
                copy_executable(&repo.join(BUILT_BINARY), target)
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
        fail_build: bool,
        restarted: Option<Restarted>,
    }

    impl UpdateHost for RecordingHost {
        fn build_and_install(
            &self,
            kind: &InstallKind,
            _repo: &Path,
            log: &std::fs::File,
        ) -> anyhow::Result<()> {
            self.builds.borrow_mut().push(kind.clone());
            writeln!(&*log, "Finished `release` profile")?;
            if self.fail_build {
                anyhow::bail!("cargo build exited with exit status: 101");
            }
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
        let err = SystemUpdateHost
            .build_and_install(
                &InstallKind::Binary {
                    target: link.clone(),
                },
                dir.path(),
                &log,
            )
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

        let systemd = detached_command(
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
