use clap::Subcommand;

pub(crate) mod completion;
pub(crate) mod config;
pub(crate) mod devices;
pub(crate) mod doctor;
pub(crate) mod export;
pub(crate) mod extension;
pub(crate) mod import;
pub(crate) mod ironhub;
pub(crate) mod models;
pub(crate) mod onboard;
pub(crate) mod pair;
pub(crate) mod profile;
pub(crate) mod remote;
pub(crate) mod repl;
pub(crate) mod run;
pub(crate) mod serve;
pub(crate) mod serve_remote;
pub(crate) mod serve_sso;
pub(crate) mod service;
pub(crate) mod skills;
pub(crate) mod status;
pub(crate) mod traces;
pub(crate) mod transfer;
pub(crate) mod update;
pub(crate) mod user_directory;
pub(crate) mod webui_auth;

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Generate shell completion scripts.
    Completion(completion::CompletionCommand),
    /// Inspect Reborn configuration paths without creating state.
    Config(config::ConfigCommand),
    /// List or revoke devices paired with the running backend.
    Devices(devices::DevicesCommand),
    /// Check Reborn binary configuration without creating state.
    Doctor(doctor::DoctorCommand),
    /// Write this computer's ZLO setup to one encrypted file for another computer.
    Export(export::ExportCommand),
    /// Manage local Reborn extension lifecycle.
    Extension(extension::ExtensionCommand),
    /// Restore a setup written by `zlo export` on this computer.
    Import(import::ImportCommand),
    /// Search and install signed registry packages from IronHub.
    #[command(name = "ironhub", visible_alias = "iron-hub", visible_alias = "hub")]
    IronHub(ironhub::IronHubCommand),
    /// Inspect Reborn model slots and route status.
    Models(models::ModelsCommand),
    /// Initialize the standalone Reborn home and first-run setup marker.
    Onboard(onboard::OnboardCommand),
    /// Issue a one-time code to pair another device with this backend.
    Pair(pair::PairCommand),
    /// Inspect supported Reborn boot profiles.
    Profile(profile::ProfileCommand),
    /// Reach this backend from your other machines over your tailnet.
    Remote(remote::RemoteCommand),
    /// Start the composed Reborn CLI REPL.
    Repl(repl::ReplCommand),
    /// Initialize the minimal Reborn runtime shell and exit.
    Run(run::RunCommand),
    /// Start the Reborn WebUI service.
    Serve(serve::ServeCommand),
    /// Install/start/stop/status/uninstall the standalone Reborn binary
    /// as an OS-native service (launchd on macOS, systemd on Linux). The
    /// installed unit runs `serve`.
    Service(service::ServiceCommand),
    /// Inspect configured Reborn skills.
    Skills(skills::SkillsCommand),
    /// Show Reborn runtime status snapshot.
    Status(status::StatusCommand),
    /// Manage trace contributions to TraceCommons.
    Traces(Box<traces::TracesCommand>),
    /// Move this computer's ZLO setup to another computer over SSH.
    Transfer(transfer::TransferCommand),
    /// Update this installation from its source checkout (`[update]`).
    Update(update::UpdateCommand),
}

impl Command {
    pub(crate) fn execute(self) -> anyhow::Result<()> {
        match self {
            Self::Completion(command) => command.execute(),
            Self::Config(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Devices(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Doctor(command) => {
                // `doctor --bundle` is read-only: it must not move a legacy
                // home to `~/.zlo` before collecting from it.
                let context = if command.writes_bundle() {
                    crate::context::RebornCliContext::resolve_from_env_without_migrating()?
                } else {
                    crate::context::RebornCliContext::resolve_from_env()?
                };
                command.execute(context)
            }
            Self::Export(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Extension(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Import(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::IronHub(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Models(command) => command.execute(),
            Self::Onboard(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Pair(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Profile(command) => command.execute(),
            Self::Remote(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Repl(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Run(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Serve(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Service(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Skills(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Status(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Traces(command) => command.execute(),
            Self::Transfer(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
            Self::Update(command) => {
                command.execute(crate::context::RebornCliContext::resolve_from_env()?)
            }
        }
    }
}
