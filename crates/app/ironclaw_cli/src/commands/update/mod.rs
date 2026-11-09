//! `ironclaw update [--check] [--json]` — update this installation from its
//! source checkout (`[update] method = "source"`).
//!
//! - `--check`: fetch the configured branch and report the running build's
//!   commit against the upstream tip (count and subjects, bounded).
//! - no flag: fast-forward the checkout (`git merge --ff-only` after the
//!   fetch, the same effect as `git pull --ff-only`; a dirty or diverged
//!   checkout is refused), rebuild the way this host is installed (ZLO's
//!   `bundle.sh` for the `zlo-backend` inside a ZLO.app, else
//!   `cargo build --release -p ironclaw` and an atomic replace of this
//!   binary), then restart through `ironclaw service` when it is installed.
//!
//! The backend runs the same job detached for `POST /api/operator/update`
//! (`job.rs`); its records live under `<reborn_home>/update/`
//! (`records.rs`). Everything comes from local config: the HTTP routes take
//! no input at all.

mod git;
mod job;
mod records;
mod service;

use std::path::PathBuf;

use anyhow::Context as _;
use clap::Args;
use ironclaw_config::{RebornConfigFile, SourceUpdateSettings};
use ironclaw_product_contracts::self_update::{BuildInfo, UpdateMethod, UpdateStatus};

use crate::context::RebornCliContext;

pub(crate) use service::SourceSelfUpdate;

/// The bearer-authed self-update routes over `service`.
pub(crate) fn self_update_routes(
    service: std::sync::Arc<SourceSelfUpdate>,
) -> ironclaw_webui::ProtectedRouteMount {
    ironclaw_webui::self_update_route_mount(std::sync::Arc::new(service::SelfUpdateRoutes(service)))
}

/// How to turn self-update on, shown wherever it is off.
pub(crate) const ENABLE_HINT: &str = "Self-update is not set up. Add to config.toml:\n\n  [update]\n  method = \"source\"\n  repo = \"/path/to/your/ironclaw/checkout\"\n";

#[derive(Debug, Args)]
pub(crate) struct UpdateCommand {
    /// Fetch and report what is available; change nothing.
    #[arg(long)]
    check: bool,
    /// Print the update status as JSON (the `GET /api/operator/update` shape).
    #[arg(long)]
    json: bool,
    /// Run as the detached job the backend starts (internal).
    #[arg(long, hide = true)]
    run_job: bool,
    /// The installed binary the detached job updates (internal; set by the
    /// backend to its own path).
    #[arg(long, hide = true, requires = "run_job")]
    install_path: Option<PathBuf>,
}

/// The build this process is.
pub(crate) fn current_build() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION").to_string(),
        commit: option_env!("IRONCLAW_BUILD_COMMIT")
            .filter(|c| git::is_full_hash(c))
            .map(str::to_string),
    }
}

/// `[update]` from the home's config.toml, `None` for method none.
pub(crate) fn source_settings(
    config_file: Option<&RebornConfigFile>,
) -> Option<SourceUpdateSettings> {
    config_file
        .and_then(|file| file.update.as_ref())
        .and_then(|section| section.source())
}

impl UpdateCommand {
    pub(crate) fn execute(self, context: RebornCliContext) -> anyhow::Result<()> {
        let home = context.boot_config().home().path().to_path_buf();
        let config_path = context.boot_config().home().config_file_path();
        let config_file = RebornConfigFile::load(&config_path)
            .with_context(|| format!("read {}", config_path.display()))?;
        let settings = source_settings(config_file.as_ref());
        let paths = records::UpdatePaths::new(&home);
        let build = current_build();

        if self.run_job {
            let settings = settings.ok_or_else(|| anyhow::anyhow!(ENABLE_HINT))?;
            let install_path = self
                .install_path
                .ok_or_else(|| anyhow::anyhow!("--run-job needs --install-path"))?;
            let install = job::InstallKind::detect(&install_path, &settings.repo);
            let outcome = job::run_job(
                job::JobInput {
                    paths: &paths,
                    settings: &settings,
                    running_commit: build.commit.as_deref(),
                    install,
                },
                &job::SystemUpdateHost,
                &mut |_| {},
            )?;
            if outcome.is_err() {
                // Another job holds the lock; it owns run.json.
                eprintln!("ironclaw update: another update is already running");
            }
            return Ok(());
        }

        let service = SourceSelfUpdate::new(&home, settings.clone(), build.clone());
        if self.check {
            let Some(settings) = settings else {
                return self.not_configured(&service);
            };
            let status = service.check_blocking(&settings)?;
            return self.print(&status, |status| match &status.available {
                Some(a) => {
                    let mut out = format!(
                        "{} commit(s) behind {} (running {}):",
                        a.behind,
                        job::short(&a.commit),
                        status
                            .current
                            .commit
                            .as_deref()
                            .map(job::short)
                            .unwrap_or("unknown")
                    );
                    for subject in &a.subjects {
                        out.push_str(&format!("\n  {subject}"));
                    }
                    if a.behind as usize > a.subjects.len() {
                        out.push_str(&format!(
                            "\n  … and {} more",
                            a.behind as usize - a.subjects.len()
                        ));
                    }
                    out
                }
                None => format!(
                    "Up to date ({}).",
                    status
                        .current
                        .commit
                        .as_deref()
                        .map(job::short)
                        .unwrap_or("unknown build")
                ),
            });
        }

        let Some(settings) = settings else {
            return self.not_configured(&service);
        };
        let exe = std::env::current_exe().context("resolve the running binary")?;
        if exe.starts_with(settings.repo.join("target")) {
            anyhow::bail!(
                "this `ironclaw` is the checkout's own build output ({}); run the installed binary instead",
                exe.display()
            );
        }
        let install = job::InstallKind::detect(&exe, &settings.repo);
        let json = self.json;
        let outcome = job::run_job(
            job::JobInput {
                paths: &paths,
                settings: &settings,
                running_commit: build.commit.as_deref(),
                install,
            },
            &job::SystemUpdateHost,
            &mut |step| {
                if !json {
                    eprintln!("ironclaw update: {step}");
                }
            },
        )?;
        if outcome.is_err() {
            anyhow::bail!("another update is already running");
        }
        let status = service.status_blocking()?;
        if json {
            println!("{}", serde_json::to_string_pretty(&status)?);
        } else {
            eprintln!("log: {}", paths.log_file().display());
        }
        if status.last_run.state == ironclaw_product_contracts::self_update::UpdateRunState::Failed
        {
            anyhow::bail!(
                "{}",
                status
                    .last_run
                    .message
                    .unwrap_or_else(|| "update failed".into())
            );
        }
        Ok(())
    }

    fn not_configured(&self, service: &SourceSelfUpdate) -> anyhow::Result<()> {
        if self.json {
            let status = service.status_blocking()?;
            debug_assert_eq!(status.method, UpdateMethod::None);
            println!("{}", serde_json::to_string_pretty(&status)?);
            return Ok(());
        }
        anyhow::bail!("{ENABLE_HINT}")
    }

    fn print(
        &self,
        status: &UpdateStatus,
        text: impl FnOnce(&UpdateStatus) -> String,
    ) -> anyhow::Result<()> {
        if self.json {
            println!("{}", serde_json::to_string_pretty(status)?);
        } else {
            println!("{}", text(status));
        }
        Ok(())
    }
}
