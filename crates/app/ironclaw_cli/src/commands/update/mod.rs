//! `zlo update [--check] [--json]` — update this installation from its
//! source checkout (`[update] method = "source"`).
//!
//! - `enable [--repo <checkout>] [--branch <name>]`: turn self-update on.
//!   The checkout defaults to the one this binary was built from, so on a
//!   computer that built ZLO from source there is nothing to type. A
//!   running backend follows the change at once.
//! - `disable`: turn it off (the checkout is remembered).
//! - `--check`: fetch the configured branch and report the running build's
//!   commit against the upstream tip (count and subjects, bounded).
//! - no flag: fast-forward the checkout (`git merge --ff-only` after the
//!   fetch, the same effect as `git pull --ff-only`; a dirty or diverged
//!   checkout is refused), rebuild the way this host is installed (ZLO's
//!   `bundle.sh` for the `zlo-backend` inside a ZLO.app, else
//!   `cargo build --release -p ironclaw` and an atomic replace of this
//!   binary), then restart through `zlo service` when it is installed.
//!
//! The backend runs the same job detached for `POST /api/operator/update`
//! (`job.rs`); its records live under `<reborn_home>/update/`
//! (`records.rs`). Everything comes from local config: the HTTP routes take
//! no input at all.

mod git;
mod job;
mod records;
mod service;

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::{Args, Subcommand};
use ironclaw_config::{
    RebornConfigFile, SourceUpdateSettings, UpdateMethodSetting, UpdateSection,
    write_update_section,
};
use ironclaw_product_contracts::self_update::{BuildInfo, UpdateMethod, UpdateStatus};

use crate::context::RebornCliContext;

pub(crate) use service::{SourceSelfUpdate, auto_check};

/// The bearer-authed self-update routes over `service`.
pub(crate) fn self_update_routes(
    service: std::sync::Arc<SourceSelfUpdate>,
) -> ironclaw_webui::ProtectedRouteMount {
    ironclaw_webui::self_update_route_mount(std::sync::Arc::new(service::SelfUpdateRoutes(service)))
}

/// How to turn self-update on, shown wherever it is off.
pub(crate) const ENABLE_HINT: &str = "Self-update is not set up. Run `zlo update enable` (add `--repo /path/to/checkout` if it does not find its source checkout).";

/// What to do when this binary knows no usable source checkout.
pub(crate) const NO_CHECKOUT_HINT: &str = "This ZLO does not know a source checkout it was built from. On this computer, clone the source and run `zlo update enable --repo /path/to/checkout`.";

#[derive(Debug, Subcommand)]
enum UpdateAction {
    /// Turn self-update on, from the checkout this ZLO was built from.
    Enable {
        /// The source checkout to update from (default: the one this
        /// binary was built from).
        #[arg(long, value_name = "CHECKOUT")]
        repo: Option<PathBuf>,
        /// The branch to follow (default: the one the checkout is on).
        #[arg(long)]
        branch: Option<String>,
    },
    /// Turn self-update off.
    Disable,
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub(crate) struct UpdateCommand {
    #[command(subcommand)]
    action: Option<UpdateAction>,
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

/// The source checkout this binary was built from (`build.rs`), when it was
/// built in one and the folder is still there.
pub(crate) fn build_checkout() -> Option<PathBuf> {
    option_env!("IRONCLAW_BUILD_SOURCE_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute() && dir.is_dir())
}

/// Write `[update] method = "source"` for `repo` into `config_path`, after
/// checking that `repo` is a git checkout on a branch update can follow.
/// No network. A refused checkout is a [`git::GitRefusal`]; nothing is
/// written then.
pub(crate) fn enable_source(
    config_path: &Path,
    repo: &Path,
    branch: Option<String>,
) -> anyhow::Result<(SourceUpdateSettings, git::Tracking)> {
    let repo = std::fs::canonicalize(repo)
        .map_err(|_| git::GitRefusal::RepoMissing(repo.to_path_buf()))?;
    let tracking =
        git::Checkout::open(&repo).and_then(|checkout| checkout.tracking(branch.as_deref()))?;
    write_source_section(config_path, repo, branch, tracking)
}

/// [`enable_source`] for the checkout the binary itself names: nobody
/// chose that path just now, so it is vetted first
/// ([`git::Checkout::open_build_checkout`]).
pub(crate) fn enable_from_build_checkout(
    config_path: &Path,
    checkout: &Path,
    build_commit: Option<&str>,
) -> anyhow::Result<(SourceUpdateSettings, git::Tracking)> {
    let (_, tracking) = git::Checkout::open_build_checkout(checkout, build_commit)?;
    let repo = std::fs::canonicalize(checkout)
        .map_err(|_| git::GitRefusal::RepoMissing(checkout.to_path_buf()))?;
    write_source_section(config_path, repo, None, tracking)
}

fn write_source_section(
    config_path: &Path,
    repo: PathBuf,
    branch: Option<String>,
    tracking: git::Tracking,
) -> anyhow::Result<(SourceUpdateSettings, git::Tracking)> {
    write_update_section(
        config_path,
        &UpdateSection {
            method: Some(UpdateMethodSetting::Source),
            repo: Some(repo.clone()),
            branch: branch.clone(),
        },
    )
    .with_context(|| format!("write [update] to {}", config_path.display()))?;
    Ok((SourceUpdateSettings { repo, branch }, tracking))
}

/// Write `[update] method = "none"`, keeping the recorded checkout and
/// branch so turning self-update back on follows the same source.
pub(crate) fn disable_source(config_path: &Path) -> anyhow::Result<()> {
    let existing = RebornConfigFile::load(config_path)
        .with_context(|| format!("read {}", config_path.display()))?
        .and_then(|file| file.update)
        .unwrap_or_default();
    write_update_section(
        config_path,
        &UpdateSection {
            method: Some(UpdateMethodSetting::None),
            ..existing
        },
    )
    .with_context(|| format!("write [update] to {}", config_path.display()))
}

/// The checkout a turned-off `[update]` still records (set earlier on this
/// machine by `zlo update enable`, never by a request).
pub(crate) fn recorded_checkout(config_path: &Path) -> Option<(PathBuf, Option<String>)> {
    let section = RebornConfigFile::load(config_path).ok()??.update?; // silent-ok: an unreadable config records no checkout
    Some((section.repo?, section.branch))
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

        match self.action {
            Some(UpdateAction::Enable { repo, branch }) => {
                let (settings, tracking) = match repo {
                    // Named by the person at the keyboard: taken as given.
                    Some(repo) => enable_source(&config_path, &repo, branch)?,
                    None => {
                        let checkout =
                            build_checkout().ok_or_else(|| anyhow::anyhow!(NO_CHECKOUT_HINT))?;
                        match branch {
                            Some(branch) => enable_source(&config_path, &checkout, Some(branch))?,
                            None => enable_from_build_checkout(
                                &config_path,
                                &checkout,
                                build.commit.as_deref(),
                            )
                            .map_err(|e| e.context(NO_CHECKOUT_HINT))?,
                        }
                    }
                };
                println!(
                    "Self-update is on: {} follows {}.",
                    settings.repo.display(),
                    tracking.upstream_name()
                );
                println!(
                    "A running backend picks this up by itself. ZLO shows when an update is \
                     ready; `zlo update` installs it from here."
                );
                return Ok(());
            }
            Some(UpdateAction::Disable) => {
                disable_source(&config_path)?;
                println!("Self-update is off.");
                return Ok(());
            }
            None => {}
        }

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
                eprintln!("zlo update: another update is already running");
            }
            return Ok(());
        }

        let exe = std::env::current_exe().context("resolve the running binary")?;
        let service = SourceSelfUpdate::new(
            &home,
            settings.clone(),
            build.clone(),
            job::InstallKind::scope_of(&exe),
        );
        if self.check {
            let Some(settings) = settings else {
                return self.not_configured(&service);
            };
            let (status, not_for_here) = service.check_blocking(&settings)?;
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
                None if not_for_here > 0 => format!(
                    "Up to date: {not_for_here} new commit(s) change nothing this computer runs."
                ),
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
        if exe.starts_with(settings.repo.join("target")) {
            anyhow::bail!(
                "this `zlo` is the checkout's own build output ({}); run the installed binary instead",
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
                    eprintln!("zlo update: {step}");
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
