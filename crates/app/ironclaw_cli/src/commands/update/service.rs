//! The backend's [`SelfUpdateService`]: status from the records on disk,
//! a check that fetches and records, a start that spawns the detached job,
//! and turning self-update on from the checkout this binary was built from.
//! Wired into `zlo serve` by `serve.rs`, which also runs [`auto_check`].
//!
//! `[update]` is read from `config.toml` on every call, so `zlo update
//! enable` (or the enable route) takes effect at once, without a restart —
//! the detached job reads the same file when it starts.
// untriaged discards: predate the workspace-wide deny (Cargo.toml [workspace.lints.clippy]).
#![allow(clippy::let_underscore_must_use)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use ironclaw_config::{RebornConfigFile, SourceUpdateSettings};
use ironclaw_product_contracts::self_update::{BuildInfo, UpdateMethod, UpdateStatus};
use ironclaw_webui::{SelfUpdateError, SelfUpdateService};

use super::git::{Checkout, GitRefusal, Scope};
use super::job;
use super::records::{self, CheckRecord, RunRecord, UpdatePaths};

/// A backend that just started checks its source this soon.
const FIRST_CHECK_AFTER: Duration = Duration::from_secs(20);
/// And then this often (one `git fetch` of one branch).
const CHECK_EVERY: Duration = Duration::from_secs(120);

/// Starts the detached job. The production spawner copies the running
/// binary and launches it (`job::spawn_detached`); tests record instead.
pub(crate) trait JobSpawner: Send + Sync {
    fn spawn(&self, paths: &UpdatePaths) -> anyhow::Result<()>;
}

/// Launches `<running binary> update --run-job --install-path <running binary>`.
struct DetachedSpawner {
    exe: PathBuf,
    reborn_home: PathBuf,
}

impl JobSpawner for DetachedSpawner {
    fn spawn(&self, paths: &UpdatePaths) -> anyhow::Result<()> {
        job::spawn_detached(paths, &self.exe, &self.reborn_home)
    }
}

/// Where `[update]` comes from.
enum Settings {
    /// Read from this `config.toml` on every call.
    File(PathBuf),
    /// Given once (the CLI's own run, tests).
    Fixed(Option<SourceUpdateSettings>),
}

pub(crate) struct SourceSelfUpdate {
    paths: UpdatePaths,
    settings: Settings,
    build: BuildInfo,
    /// What this installation is built from (the app, or the backend alone).
    scope: Scope,
    /// The checkout this binary was built from, when it knows one.
    build_checkout: Option<PathBuf>,
    spawner: Option<Arc<dyn JobSpawner>>,
    /// Serializes check-then-spawn, and the background check against a
    /// start, inside this process; the file lock serializes jobs across
    /// processes.
    starting: tokio::sync::Mutex<()>,
}

impl SourceSelfUpdate {
    /// For the CLI: reads and checks, never spawns.
    pub(crate) fn new(
        reborn_home: &Path,
        settings: Option<SourceUpdateSettings>,
        build: BuildInfo,
        scope: Scope,
    ) -> Self {
        Self {
            paths: UpdatePaths::new(reborn_home),
            settings: Settings::Fixed(settings),
            build,
            scope,
            build_checkout: None,
            spawner: None,
            starting: tokio::sync::Mutex::new(()),
        }
    }

    /// For `serve`: `[update]` is followed live from `config_path`, the
    /// detached job runs this process's own binary, and self-update can be
    /// turned on from `build_checkout`.
    pub(crate) fn for_serve(
        reborn_home: &Path,
        config_path: PathBuf,
        build: BuildInfo,
        build_checkout: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let exe = std::env::current_exe()?;
        Ok(Self {
            settings: Settings::File(config_path),
            build_checkout,
            spawner: Some(Arc::new(DetachedSpawner {
                exe: exe.clone(),
                reborn_home: reborn_home.to_path_buf(),
            })),
            ..Self::new(reborn_home, None, build, job::InstallKind::scope_of(&exe))
        })
    }

    /// `[update]` as it is now. An unreadable `config.toml` reads as "not
    /// set up" (and is logged): nothing updates from a file that does not
    /// parse.
    fn settings(&self) -> Option<SourceUpdateSettings> {
        match &self.settings {
            Settings::Fixed(settings) => settings.clone(),
            Settings::File(path) => match RebornConfigFile::load(path) {
                Ok(file) => super::source_settings(file.as_ref()),
                Err(error) => {
                    tracing::debug!(target: "ironclaw::reborn::cli::update", %error, "config.toml unreadable; self-update reads as off");
                    None
                }
            },
        }
    }

    /// Whether `[update]` is set up (the descriptor's `self_update`).
    pub(crate) fn configured(&self) -> bool {
        self.settings().is_some()
    }

    /// The config file and checkout the enable route would use.
    fn enable_target(&self) -> Option<(&Path, &Path)> {
        let Settings::File(config_path) = &self.settings else {
            return None;
        };
        Some((config_path, self.build_checkout.as_deref()?))
    }

    /// Whether the enable route would work right now: the same vetting
    /// enable itself does (a few local git reads, no network).
    fn can_enable(&self) -> bool {
        if let Some((repo, branch)) = self.recorded_checkout() {
            return Checkout::open(&repo)
                .and_then(|c| c.tracking(branch.as_deref()))
                .is_ok();
        }
        self.enable_target().is_some_and(|(_, checkout)| {
            Checkout::open_build_checkout(checkout, self.build.commit.as_deref()).is_ok()
        })
    }

    /// The checkout a turned-off `[update]` still names: the one this
    /// machine followed before, which enable turns back on.
    fn recorded_checkout(&self) -> Option<(PathBuf, Option<String>)> {
        match &self.settings {
            Settings::File(config_path) => super::recorded_checkout(config_path),
            Settings::Fixed(_) => None,
        }
    }

    /// Turn self-update off, keeping the checkout on record. A job that is
    /// already running finishes: it read its settings when it started.
    fn disable_blocking(&self) -> Result<UpdateStatus, SelfUpdateError> {
        if self.configured() {
            let Settings::File(config_path) = &self.settings else {
                return Err(SelfUpdateError::Unavailable);
            };
            super::disable_source(config_path).map_err(unavailable)?;
        }
        self.status_blocking().map_err(unavailable)
    }

    pub(crate) fn status_blocking(&self) -> anyhow::Result<UpdateStatus> {
        let configured = self.configured();
        let locked = records::is_locked(&self.paths)?;
        let available = records::read_check(&self.paths)?
            .filter(|_| configured)
            .and_then(|check| check.available(self.build.commit.as_deref()));
        Ok(UpdateStatus {
            method: if configured {
                UpdateMethod::Source
            } else {
                UpdateMethod::None
            },
            current: self.build.clone(),
            available,
            last_run: records::last_run(&self.paths, locked, Utc::now())?,
            can_enable: !configured && self.can_enable(),
        })
    }

    /// Fetch and record. Returns the status and how many new commits
    /// change nothing this installation is built from.
    pub(crate) fn check_blocking(
        &self,
        settings: &SourceUpdateSettings,
    ) -> anyhow::Result<(UpdateStatus, u32)> {
        self.check_within(settings, None)
    }

    /// [`Self::check_blocking`], with the fetch cut off after `fetch_timeout`.
    fn check_within(
        &self,
        settings: &SourceUpdateSettings,
        fetch_timeout: Option<std::time::Duration>,
    ) -> anyhow::Result<(UpdateStatus, u32)> {
        let outcome = Checkout::open(&settings.repo).and_then(|c| {
            let c = match fetch_timeout {
                Some(timeout) => c.fetching_within(timeout),
                None => c,
            };
            c.check(
                settings.branch.as_deref(),
                self.build.commit.as_deref(),
                self.scope,
            )
        })?;
        records::write_check(
            &self.paths,
            &CheckRecord {
                upstream: outcome.upstream,
                behind: outcome.behind,
                subjects: outcome.subjects,
                checked_at: Utc::now(),
            },
        )?;
        Ok((self.status_blocking()?, outcome.not_for_this_install))
    }

    fn start_blocking(&self) -> Result<UpdateStatus, SelfUpdateError> {
        if !self.configured() {
            return Err(SelfUpdateError::NotConfigured);
        }
        let spawner = self.spawner.as_ref().ok_or(SelfUpdateError::Unavailable)?;
        let locked = records::is_locked(&self.paths).map_err(unavailable)?;
        let starting = records::read_run(&self.paths)
            .map_err(unavailable)?
            .is_some_and(|run| run.just_started(Utc::now()));
        if locked || starting {
            return Err(SelfUpdateError::AlreadyRunning);
        }
        records::write_run(&self.paths, &RunRecord::running("Starting the update"))
            .map_err(unavailable)?;
        if let Err(error) = spawner.spawn(&self.paths) {
            tracing::error!(target: "ironclaw::reborn::cli::update", error = %format!("{error:#}"), "update job did not start");
            let failed = RunRecord::running("Starting the update").finish(
                ironclaw_product_contracts::self_update::UpdateRunState::Failed,
                "The update job could not be started. See the backend log.".into(),
            );
            records::write_run(&self.paths, &failed).map_err(unavailable)?;
        }
        self.status_blocking().map_err(unavailable)
    }

    /// Turn self-update on from the checkout this binary was built from.
    /// Nothing here comes from a request.
    fn enable_blocking(&self) -> Result<UpdateStatus, SelfUpdateError> {
        if self.configured() {
            return self.status_blocking().map_err(unavailable);
        }
        let refused = |error: anyhow::Error| match error.downcast::<GitRefusal>() {
            Ok(refusal) => {
                SelfUpdateError::NoCheckout(format!("{refusal} {}", super::NO_CHECKOUT_HINT))
            }
            Err(other) => unavailable(other),
        };
        // Turned off earlier: back on from the checkout it followed then.
        if let (Settings::File(config_path), Some((repo, branch))) =
            (&self.settings, self.recorded_checkout())
        {
            super::enable_source(config_path, &repo, branch).map_err(refused)?;
            return self.status_blocking().map_err(unavailable);
        }
        let Some((config_path, checkout)) = self.enable_target() else {
            return Err(SelfUpdateError::NoCheckout(super::NO_CHECKOUT_HINT.into()));
        };
        super::enable_from_build_checkout(config_path, checkout, self.build.commit.as_deref())
            .map_err(|error| match error.downcast::<GitRefusal>() {
                Ok(refusal) => {
                    SelfUpdateError::NoCheckout(format!("{refusal} {}", super::NO_CHECKOUT_HINT))
                }
                Err(other) => unavailable(other),
            })?;
        self.status_blocking().map_err(unavailable)
    }
}

fn unavailable(error: anyhow::Error) -> SelfUpdateError {
    tracing::error!(target: "ironclaw::reborn::cli::update", error = %format!("{error:#}"), "update records unavailable");
    SelfUpdateError::Unavailable
}

/// Run blocking file/git work off the async runtime.
async fn blocking<T: Send + 'static>(
    this: &Arc<SourceSelfUpdate>,
    work: impl FnOnce(&SourceSelfUpdate) -> Result<T, SelfUpdateError> + Send + 'static,
) -> Result<T, SelfUpdateError> {
    let this = Arc::clone(this);
    tokio::task::spawn_blocking(move || work(&this))
        .await
        .map_err(|join| unavailable(anyhow::anyhow!("update task failed: {join}")))?
}

/// One background check: skipped while self-update is off or a job runs.
/// On its own thread, so a slow fetch never holds up the backend's
/// shutdown. Failures (a dirty checkout, no network) are for the debug log:
/// the next explicit check or update reports them to the person.
async fn check_in_background(this: &Arc<SourceSelfUpdate>) {
    let _guard = this.starting.lock().await;
    let (done, finished) = tokio::sync::oneshot::channel();
    let worker = Arc::clone(this);
    let spawned = std::thread::Builder::new()
        .name("zlo-update-check".into())
        .spawn(move || {
            let outcome = (|| -> anyhow::Result<()> {
                let Some(settings) = worker.settings() else {
                    return Ok(());
                };
                if records::is_locked(&worker.paths)? {
                    return Ok(());
                }
                // Short: a start or enable may be waiting behind this.
                worker
                    .check_within(&settings, Some(super::git::BACKGROUND_FETCH_TIMEOUT))
                    .map(drop)
            })();
            // silent-ok: the backend is shutting down and no longer listens.
            let _ = done.send(outcome);
        });
    if let Err(error) = spawned {
        tracing::debug!(target: "ironclaw::reborn::cli::update", %error, "update check thread did not start");
        return;
    }
    match finished.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::debug!(target: "ironclaw::reborn::cli::update", error = %format!("{error:#}"), "background update check failed");
        }
        Err(_closed) => {
            tracing::debug!(target: "ironclaw::reborn::cli::update", "background update check ended without an answer");
        }
    }
}

/// Keep what `GET /api/operator/update` answers fresh: check the source
/// soon after start, then every [`CHECK_EVERY`], for as long as the backend
/// runs. A client then only has to read the status.
pub(crate) async fn auto_check(this: Arc<SourceSelfUpdate>) {
    tokio::time::sleep(FIRST_CHECK_AFTER).await;
    loop {
        check_in_background(&this).await;
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

/// The port over a shared instance (the handlers hold it as `Arc<dyn …>`).
pub(crate) struct SelfUpdateRoutes(pub Arc<SourceSelfUpdate>);

#[async_trait]
impl SelfUpdateService for SelfUpdateRoutes {
    async fn status(&self) -> Result<UpdateStatus, SelfUpdateError> {
        blocking(&self.0, |s| s.status_blocking().map_err(unavailable)).await
    }

    async fn check(&self) -> Result<UpdateStatus, SelfUpdateError> {
        blocking(&self.0, |s| {
            let settings = s.settings().ok_or(SelfUpdateError::NotConfigured)?;
            s.check_blocking(&settings)
                .map(|(status, _)| status)
                .map_err(|error| match error.downcast::<GitRefusal>() {
                    Ok(refusal) => SelfUpdateError::CheckFailed(refusal.to_string()),
                    Err(other) => unavailable(other),
                })
        })
        .await
    }

    async fn start(&self) -> Result<UpdateStatus, SelfUpdateError> {
        let _guard = self.0.starting.lock().await;
        blocking(&self.0, SourceSelfUpdate::start_blocking).await
    }

    async fn enable(&self) -> Result<UpdateStatus, SelfUpdateError> {
        let _guard = self.0.starting.lock().await;
        blocking(&self.0, SourceSelfUpdate::enable_blocking).await
    }

    async fn disable(&self) -> Result<UpdateStatus, SelfUpdateError> {
        let _guard = self.0.starting.lock().await;
        blocking(&self.0, SourceSelfUpdate::disable_blocking).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ironclaw_product_contracts::self_update::UpdateRunState;

    use super::super::git::tests::{commit, push_upstream, repos, sh};
    use super::*;

    impl SourceSelfUpdate {
        fn with_spawner(
            reborn_home: &Path,
            settings: Option<SourceUpdateSettings>,
            build: BuildInfo,
            spawner: Arc<dyn JobSpawner>,
        ) -> Self {
            Self {
                spawner: Some(spawner),
                ..Self::new(reborn_home, settings, build, Scope::App)
            }
        }
    }

    #[derive(Default)]
    struct RecordingSpawner {
        spawned: Mutex<u32>,
        fail: bool,
    }

    impl JobSpawner for RecordingSpawner {
        fn spawn(&self, _paths: &UpdatePaths) -> anyhow::Result<()> {
            *self.spawned.lock().expect("lock") += 1;
            if self.fail {
                anyhow::bail!("exec failed");
            }
            Ok(())
        }
    }

    fn build(commit: Option<String>) -> BuildInfo {
        BuildInfo {
            version: "1.2.0".into(),
            commit,
        }
    }

    #[tokio::test]
    async fn status_check_and_start_through_the_port() {
        let r = repos();
        let head = sh(&r.checkout, &["rev-parse", "HEAD"]);
        let tip = push_upstream(&r, &["Second", "Third"]);
        let home = tempfile::tempdir().expect("home");
        let spawner = Arc::new(RecordingSpawner::default());
        let routes = SelfUpdateRoutes(Arc::new(SourceSelfUpdate::with_spawner(
            home.path(),
            Some(SourceUpdateSettings {
                repo: r.checkout.clone(),
                branch: None,
            }),
            build(Some(head)),
            spawner.clone(),
        )));

        let idle = routes.status().await.expect("status");
        assert_eq!(idle.method, UpdateMethod::Source);
        assert_eq!(idle.available, None, "no check yet");
        assert_eq!(idle.last_run.state, UpdateRunState::Idle);

        let checked = routes.check().await.expect("check");
        let available = checked.available.expect("behind");
        assert_eq!(
            (available.commit.as_str(), available.behind),
            (tip.as_str(), 2)
        );
        assert_eq!(available.subjects, vec!["Third", "Second"]);
        assert_eq!(
            routes
                .status()
                .await
                .expect("status")
                .available
                .map(|a| a.behind),
            Some(2),
            "the check is recorded"
        );

        let started = routes.start().await.expect("start");
        assert_eq!(started.last_run.state, UpdateRunState::Running);
        assert_eq!(*spawner.spawned.lock().expect("lock"), 1);
        assert_eq!(
            routes.start().await.expect_err("second start"),
            SelfUpdateError::AlreadyRunning,
            "a job that is still starting counts"
        );
        let _job = records::try_lock(&UpdatePaths::new(home.path()))
            .expect("lock")
            .expect("free");
        assert_eq!(
            routes.start().await.expect_err("locked"),
            SelfUpdateError::AlreadyRunning
        );
        assert_eq!(*spawner.spawned.lock().expect("lock"), 1);
    }

    #[tokio::test]
    async fn a_finished_job_reads_back_from_a_new_backend() {
        let home = tempfile::tempdir().expect("home");
        let paths = UpdatePaths::new(home.path());
        let done = RunRecord::running("Building").finish(
            UpdateRunState::Succeeded,
            "Updated to bbbbbbbbb; IronClaw restarted.".into(),
        );
        records::write_run(&paths, &done).expect("job wrote");
        // A fresh instance, as after the restart.
        let routes = SelfUpdateRoutes(Arc::new(SourceSelfUpdate::new(
            home.path(),
            Some(SourceUpdateSettings {
                repo: home.path().to_path_buf(),
                branch: None,
            }),
            build(None),
            Scope::App,
        )));
        let status = routes.status().await.expect("status");
        assert_eq!(status.last_run.state, UpdateRunState::Succeeded);
        assert_eq!(status.last_run.finished_at, done.finished_at);
        assert_eq!(status.last_run.message, done.message);
    }

    #[tokio::test]
    async fn method_none_refuses_check_and_start() {
        let home = tempfile::tempdir().expect("home");
        let spawner = Arc::new(RecordingSpawner::default());
        let routes = SelfUpdateRoutes(Arc::new(SourceSelfUpdate::with_spawner(
            home.path(),
            None,
            build(None),
            spawner.clone(),
        )));
        assert_eq!(
            routes.status().await.expect("status").method,
            UpdateMethod::None
        );
        assert_eq!(
            routes.check().await.expect_err("check"),
            SelfUpdateError::NotConfigured
        );
        assert_eq!(
            routes.start().await.expect_err("start"),
            SelfUpdateError::NotConfigured
        );
        assert_eq!(*spawner.spawned.lock().expect("lock"), 0);
    }

    /// A backend as `serve` builds it: `[update]` followed from a config
    /// file, built from `checkout`.
    fn served(home: &Path, checkout: Option<PathBuf>) -> SelfUpdateRoutes {
        // Built from the checkout's HEAD when it is one.
        let checkout_head = checkout
            .as_deref()
            .filter(|dir| dir.join(".git").exists())
            .map(|dir| sh(dir, &["rev-parse", "HEAD"]));
        SelfUpdateRoutes(Arc::new(SourceSelfUpdate {
            settings: Settings::File(home.join("config.toml")),
            build_checkout: checkout,
            ..SourceSelfUpdate::with_spawner(
                home,
                None,
                build(checkout_head),
                Arc::new(RecordingSpawner::default()),
            )
        }))
    }

    #[tokio::test]
    async fn enable_turns_updates_on_from_the_build_checkout_without_a_restart() {
        let r = repos();
        let home = tempfile::tempdir().expect("home");
        std::fs::write(
            home.path().join("config.toml"),
            "# mine\n[webui]\nlisten_port = 3000\n",
        )
        .expect("config");
        let routes = served(home.path(), Some(r.checkout.clone()));

        let off = routes.status().await.expect("status");
        assert_eq!(off.method, UpdateMethod::None);
        assert!(off.can_enable, "it knows its checkout");
        assert_eq!(
            routes.start().await.expect_err("off"),
            SelfUpdateError::NotConfigured
        );

        let on = routes.enable().await.expect("enable");
        assert_eq!(on.method, UpdateMethod::Source);
        assert!(!on.can_enable);
        let config = std::fs::read_to_string(home.path().join("config.toml")).expect("config");
        assert!(config.contains("# mine"), "{config}");
        let file = RebornConfigFile::load(&home.path().join("config.toml"))
            .expect("load")
            .expect("present");
        assert_eq!(
            super::super::source_settings(Some(&file)).map(|s| s.repo),
            Some(std::fs::canonicalize(&r.checkout).expect("canonical"))
        );
        // The same running backend now checks and starts.
        push_upstream(&r, &["Second"]);
        let checked = routes.check().await.expect("check");
        assert_eq!(checked.available.map(|a| a.behind), Some(1));
        assert_eq!(
            routes.start().await.expect("start").last_run.state,
            UpdateRunState::Running
        );
        // Enabling again changes nothing.
        assert_eq!(
            routes.enable().await.expect("again").method,
            UpdateMethod::Source
        );
        // Turned off through the port: off at once, the checkout stays on
        // record, and turning it on again follows the same one.
        let off = routes.disable().await.expect("disable");
        assert_eq!(off.method, UpdateMethod::None);
        assert!(
            off.can_enable,
            "the recorded checkout can be turned back on"
        );
        assert_eq!(
            routes.start().await.expect_err("off"),
            SelfUpdateError::NotConfigured
        );
        assert_eq!(
            routes.disable().await.expect("again").method,
            UpdateMethod::None,
            "turning off twice changes nothing"
        );
        let config = std::fs::read_to_string(home.path().join("config.toml")).expect("config");
        assert!(config.contains("# mine"), "{config}");
        assert_eq!(
            routes.enable().await.expect("back on").method,
            UpdateMethod::Source
        );
        // Turned off by hand: the running backend follows the file.
        ironclaw_config::write_update_section(
            &home.path().join("config.toml"),
            &ironclaw_config::UpdateSection {
                method: Some(ironclaw_config::UpdateMethodSetting::None),
                ..file.update.expect("section")
            },
        )
        .expect("off");
        assert_eq!(
            routes.status().await.expect("status").method,
            UpdateMethod::None
        );
    }

    #[tokio::test]
    async fn enable_is_refused_without_a_usable_checkout() {
        let home = tempfile::tempdir().expect("home");
        // No checkout known to the build.
        let none = served(home.path(), None);
        assert!(!none.status().await.expect("status").can_enable);
        match none.enable().await.expect_err("no checkout") {
            SelfUpdateError::NoCheckout(message) => {
                assert!(message.contains("zlo update enable --repo"), "{message}")
            }
            other => panic!("expected NoCheckout, got {other:?}"),
        }
        // The build's checkout is gone.
        let gone = served(home.path(), Some(home.path().join("deleted")));
        assert!(!gone.status().await.expect("status").can_enable);
        assert!(matches!(
            gone.enable().await.expect_err("gone"),
            SelfUpdateError::NoCheckout(_)
        ));
        // A folder that is not a git checkout.
        let plain = tempfile::tempdir().expect("plain");
        let not_git = served(home.path(), Some(plain.path().to_path_buf()));
        assert!(
            !not_git.status().await.expect("status").can_enable,
            "Turn on is only offered when it would work"
        );
        match not_git.enable().await.expect_err("not git") {
            SelfUpdateError::NoCheckout(message) => {
                assert!(message.contains("not a git checkout"), "{message}")
            }
            other => panic!("expected NoCheckout, got {other:?}"),
        }
        // The folder was replaced by another repository since the build:
        // same path, but it does not hold the commit this binary came from.
        let r = repos();
        let other = repos();
        let replaced = SelfUpdateRoutes(Arc::new(SourceSelfUpdate {
            settings: Settings::File(home.path().join("config.toml")),
            build_checkout: Some(other.checkout.clone()),
            ..SourceSelfUpdate::with_spawner(
                home.path(),
                None,
                build(Some(commit(
                    &r.checkout,
                    "only-here.txt",
                    "x",
                    "Only in the original",
                ))),
                Arc::new(RecordingSpawner::default()),
            )
        }));
        assert!(!replaced.status().await.expect("status").can_enable);
        match replaced.enable().await.expect_err("replaced") {
            SelfUpdateError::NoCheckout(message) => {
                assert!(
                    message.contains("not the checkout this ZLO was built from"),
                    "{message}"
                )
            }
            other => panic!("expected NoCheckout, got {other:?}"),
        }
        // A link to a real checkout where the folder used to be.
        #[cfg(unix)]
        {
            let link = home.path().join("linked");
            std::os::unix::fs::symlink(&r.checkout, &link).expect("symlink");
            let linked = served(home.path(), Some(link));
            assert!(!linked.status().await.expect("status").can_enable);
            assert!(matches!(
                linked.enable().await.expect_err("link"),
                SelfUpdateError::NoCheckout(_)
            ));
        }
        // On a branch with no remote: not offered, and refused.
        let lone = tempfile::tempdir().expect("lone");
        sh(lone.path(), &["init", "-q", "-b", "main"]);
        commit(lone.path(), "a.txt", "a", "First");
        let no_remote = served(home.path(), Some(lone.path().to_path_buf()));
        assert!(!no_remote.status().await.expect("status").can_enable);
        assert!(
            !home.path().join("config.toml").exists(),
            "a refused enable writes nothing"
        );
    }

    #[tokio::test]
    async fn the_background_check_records_what_is_new_and_stays_out_of_a_jobs_way() {
        let r = repos();
        let head = sh(&r.checkout, &["rev-parse", "HEAD"]);
        let home = tempfile::tempdir().expect("home");
        let service = Arc::new(SourceSelfUpdate::with_spawner(
            home.path(),
            Some(SourceUpdateSettings {
                repo: r.checkout.clone(),
                branch: None,
            }),
            build(Some(head)),
            Arc::new(RecordingSpawner::default()),
        ));
        push_upstream(&r, &["Second"]);
        check_in_background(&service).await;
        let status = service.status_blocking().expect("status");
        assert_eq!(status.available.map(|a| a.behind), Some(1));

        // While a job holds the lock the check does not fetch.
        push_upstream(&r, &["Third"]);
        let job = records::try_lock(&UpdatePaths::new(home.path()))
            .expect("lock")
            .expect("free");
        check_in_background(&service).await;
        drop(job);
        let status = service.status_blocking().expect("status");
        assert_eq!(status.available.map(|a| a.behind), Some(1), "unchanged");

        // Off: nothing is fetched or recorded.
        let off_home = tempfile::tempdir().expect("home");
        let off = Arc::new(SourceSelfUpdate::new(
            off_home.path(),
            None,
            build(None),
            Scope::App,
        ));
        check_in_background(&off).await;
        assert!(!off_home.path().join("update").exists());
    }

    #[tokio::test]
    async fn a_refused_check_carries_the_reason() {
        let r = repos();
        std::fs::write(r.checkout.join("README"), "edited").expect("edit");
        sh(&r.checkout, &["checkout", "-q", "-b", "other"]);
        let home = tempfile::tempdir().expect("home");
        let routes = SelfUpdateRoutes(Arc::new(SourceSelfUpdate::new(
            home.path(),
            Some(SourceUpdateSettings {
                repo: r.checkout.clone(),
                branch: Some("main".into()),
            }),
            build(None),
            Scope::App,
        )));
        match routes.check().await.expect_err("refused") {
            SelfUpdateError::CheckFailed(message) => {
                assert!(
                    message.contains("`other`") && message.contains("`main`"),
                    "{message}"
                )
            }
            other => panic!("expected CheckFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_job_that_cannot_start_is_recorded_as_failed() {
        let home = tempfile::tempdir().expect("home");
        let routes = SelfUpdateRoutes(Arc::new(SourceSelfUpdate::with_spawner(
            home.path(),
            Some(SourceUpdateSettings {
                repo: home.path().to_path_buf(),
                branch: None,
            }),
            build(None),
            Arc::new(RecordingSpawner {
                fail: true,
                ..Default::default()
            }),
        )));
        let status = routes.start().await.expect("answered");
        assert_eq!(status.last_run.state, UpdateRunState::Failed);
        assert!(
            routes.start().await.is_ok(),
            "a failed start does not block the next"
        );
    }
}
