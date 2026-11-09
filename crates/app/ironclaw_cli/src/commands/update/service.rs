//! The backend's [`SelfUpdateService`]: status from the records on disk,
//! a check that fetches and records, and a start that spawns the detached
//! job. Wired into `ironclaw serve` by `serve.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use ironclaw_config::SourceUpdateSettings;
use ironclaw_product_contracts::self_update::{BuildInfo, UpdateMethod, UpdateStatus};
use ironclaw_webui::{SelfUpdateError, SelfUpdateService};

use super::git::{Checkout, GitRefusal};
use super::job;
use super::records::{self, CheckRecord, RunRecord, UpdatePaths};

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

pub(crate) struct SourceSelfUpdate {
    paths: UpdatePaths,
    settings: Option<SourceUpdateSettings>,
    build: BuildInfo,
    spawner: Option<Arc<dyn JobSpawner>>,
    /// Serializes check-then-spawn inside this process; the file lock
    /// serializes jobs across processes.
    starting: tokio::sync::Mutex<()>,
}

impl SourceSelfUpdate {
    /// For the CLI: reads and checks, never spawns.
    pub(crate) fn new(
        reborn_home: &Path,
        settings: Option<SourceUpdateSettings>,
        build: BuildInfo,
    ) -> Self {
        Self {
            paths: UpdatePaths::new(reborn_home),
            settings,
            build,
            spawner: None,
            starting: tokio::sync::Mutex::new(()),
        }
    }

    /// For `serve`: the detached job runs this process's own binary.
    pub(crate) fn for_serve(
        reborn_home: &Path,
        settings: Option<SourceUpdateSettings>,
        build: BuildInfo,
    ) -> anyhow::Result<Self> {
        let exe = std::env::current_exe()?;
        Ok(Self::with_spawner(
            reborn_home,
            settings,
            build,
            Arc::new(DetachedSpawner {
                exe,
                reborn_home: reborn_home.to_path_buf(),
            }),
        ))
    }

    pub(crate) fn with_spawner(
        reborn_home: &Path,
        settings: Option<SourceUpdateSettings>,
        build: BuildInfo,
        spawner: Arc<dyn JobSpawner>,
    ) -> Self {
        Self {
            spawner: Some(spawner),
            ..Self::new(reborn_home, settings, build)
        }
    }

    /// Whether `[update]` is set up (the descriptor's `self_update`).
    pub(crate) fn configured(&self) -> bool {
        self.settings.is_some()
    }

    pub(crate) fn status_blocking(&self) -> anyhow::Result<UpdateStatus> {
        let locked = records::is_locked(&self.paths)?;
        let available = records::read_check(&self.paths)?
            .and_then(|check| check.available(self.build.commit.as_deref()));
        Ok(UpdateStatus {
            method: if self.configured() {
                UpdateMethod::Source
            } else {
                UpdateMethod::None
            },
            current: self.build.clone(),
            available,
            last_run: records::last_run(&self.paths, locked, Utc::now())?,
        })
    }

    pub(crate) fn check_blocking(
        &self,
        settings: &SourceUpdateSettings,
    ) -> anyhow::Result<UpdateStatus> {
        let outcome = Checkout::open(&settings.repo)
            .and_then(|c| c.check(settings.branch.as_deref(), self.build.commit.as_deref()))?;
        records::write_check(
            &self.paths,
            &CheckRecord {
                upstream: outcome.upstream,
                behind: outcome.behind,
                subjects: outcome.subjects,
                checked_at: Utc::now(),
            },
        )?;
        self.status_blocking()
    }

    fn start_blocking(&self) -> Result<UpdateStatus, SelfUpdateError> {
        if self.settings.is_none() {
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

/// The port over a shared instance (the handlers hold it as `Arc<dyn …>`).
pub(crate) struct SelfUpdateRoutes(pub Arc<SourceSelfUpdate>);

#[async_trait]
impl SelfUpdateService for SelfUpdateRoutes {
    async fn status(&self) -> Result<UpdateStatus, SelfUpdateError> {
        blocking(&self.0, |s| s.status_blocking().map_err(unavailable)).await
    }

    async fn check(&self) -> Result<UpdateStatus, SelfUpdateError> {
        blocking(&self.0, |s| {
            let settings = s.settings.clone().ok_or(SelfUpdateError::NotConfigured)?;
            s.check_blocking(&settings)
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
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ironclaw_product_contracts::self_update::UpdateRunState;

    use super::super::git::tests::{push_upstream, repos, sh};
    use super::*;

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
