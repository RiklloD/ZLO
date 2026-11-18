use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Args, Subcommand};
use ironclaw_composition::{
    LifecycleProductResponse, RebornRuntime, RebornRuntimeInput, build_reborn_runtime,
};
use ironclaw_extension_contracts::state::InstallationState;
use ironclaw_extension_manager::extension_lifecycle_command::{
    RebornExtensionLifecycleCommand, execute_reborn_extension_lifecycle_command,
    render_consent_summary, render_reborn_extension_lifecycle_response,
};
use ironclaw_host_api::ids::ExtensionId;
use ironclaw_product_contracts::package_lifecycle::public_lifecycle_response_json;

use crate::context::RebornCliContext;
use crate::runtime::{RuntimeInputCaller, RuntimeInputOptions};

#[derive(Debug, Args)]
pub(crate) struct ExtensionCommand {
    /// Confirm trusted-laptop host filesystem access for the unrestricted standalone profile.
    #[arg(long = "confirm-host-access", global = true)]
    confirm_host_access: bool,

    #[command(subcommand)]
    command: ExtensionSubcommand,
}

#[derive(Debug, Subcommand)]
enum ExtensionSubcommand {
    /// Search local Reborn extension packages.
    Search(ExtensionSearchCommand),
    /// Scaffold a new plugin package where local packages are discovered.
    New(ExtensionNewCommand),
    /// Install an extension by id, or a plugin folder by path.
    Install(ExtensionInstallCommand),
    /// Grant consent to a plugin for the digest you reviewed, then activate it.
    Grant(ExtensionGrantCommand),
    /// Remove an installed local Reborn extension package.
    Remove(ExtensionPackageCommand),
}

#[derive(Debug, Args)]
struct ExtensionSearchCommand {
    /// Query extension id, name, or description. Omit to list all local packages.
    query: Option<String>,

    /// Output the lifecycle response as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ExtensionNewCommand {
    /// Id of the new plugin: lowercase letters, digits, `-`, `_`, `.`.
    id: String,

    /// Output the result as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ExtensionInstallCommand {
    /// Extension id from `zlo extension search`, or a path to a plugin folder.
    target: String,

    /// Grant consent without asking (for scripts). The summary is still printed.
    #[arg(long)]
    yes: bool,

    /// Output the lifecycle response as JSON. Without `--yes`, a plugin that
    /// needs consent is left waiting and its consent summary is returned.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ExtensionGrantCommand {
    /// Extension id.
    id: String,

    /// The consent digest shown by `install` (64 hex characters).
    #[arg(long)]
    digest: String,

    /// Output the lifecycle response as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ExtensionPackageCommand {
    /// Extension id from `zlo extension search`.
    id: String,

    /// Output the lifecycle response as JSON.
    #[arg(long)]
    json: bool,
}

/// How an install answers a plugin that needs consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsentMode {
    /// `--yes`: grant the digest that was shown.
    Grant,
    /// Interactive terminal: show the summary and ask.
    Ask,
    /// Non-interactive without `--yes`: report and leave it waiting.
    Report,
}

impl ExtensionCommand {
    pub(crate) fn execute(self, context: RebornCliContext) -> anyhow::Result<()> {
        crate::runtime::init_tracing();
        let confirm_host_access = self.confirm_host_access;
        match self.command {
            ExtensionSubcommand::Search(command) => {
                let response = with_lifecycle_runtime(context, confirm_host_access, {
                    let query = command.query.unwrap_or_default();
                    async move |runtime: &RebornRuntime| {
                        run(runtime, RebornExtensionLifecycleCommand::Search { query }).await
                    }
                })?;
                print_response("search", &response, command.json)
            }
            ExtensionSubcommand::New(command) => scaffold(&context, command),
            ExtensionSubcommand::Install(command) => {
                let mode = if command.yes {
                    ConsentMode::Grant
                } else if !command.json && std::io::stdin().is_terminal() {
                    ConsentMode::Ask
                } else {
                    ConsentMode::Report
                };
                let lifecycle_command = install_command(&command.target)?;
                let json = command.json;
                let response = with_lifecycle_runtime(
                    context,
                    confirm_host_access,
                    async move |runtime: &RebornRuntime| {
                        let response = run(runtime, lifecycle_command).await?;
                        settle_consent(runtime, response, mode, json).await
                    },
                )?;
                print_response("install", &response, json)
            }
            ExtensionSubcommand::Grant(command) => {
                let lifecycle_command = RebornExtensionLifecycleCommand::GrantConsent {
                    id: command.id,
                    digest: command.digest,
                };
                let response = with_lifecycle_runtime(
                    context,
                    confirm_host_access,
                    async move |runtime: &RebornRuntime| run(runtime, lifecycle_command).await,
                )?;
                print_response("grant", &response, command.json)
            }
            ExtensionSubcommand::Remove(command) => {
                let lifecycle_command = RebornExtensionLifecycleCommand::Remove { id: command.id };
                let response = with_lifecycle_runtime(
                    context,
                    confirm_host_access,
                    async move |runtime: &RebornRuntime| run(runtime, lifecycle_command).await,
                )?;
                print_response("remove", &response, command.json)
            }
        }
    }
}

/// `install <id>` or `install <folder>`. A target is a folder when it names
/// an existing directory or contains a path separator; extension ids never
/// contain one.
fn install_command(target: &str) -> anyhow::Result<RebornExtensionLifecycleCommand> {
    let path = Path::new(target);
    if target.contains(std::path::MAIN_SEPARATOR) || target.contains('/') || path.is_dir() {
        let files = ironclaw_extension_host::read_extension_folder(path)
            .map_err(|error| anyhow::anyhow!("{}: {}", path.display(), error.reason()))?;
        return Ok(RebornExtensionLifecycleCommand::InstallFiles { files });
    }
    Ok(RebornExtensionLifecycleCommand::Install {
        id: target.to_string(),
    })
}

async fn run(
    runtime: &RebornRuntime,
    command: RebornExtensionLifecycleCommand,
) -> anyhow::Result<LifecycleProductResponse> {
    execute_reborn_extension_lifecycle_command(runtime, command)
        .await
        .map_err(anyhow::Error::from)
}

/// Answer a plugin's consent request per `mode`, in the same runtime the
/// install ran in.
async fn settle_consent(
    runtime: &RebornRuntime,
    response: LifecycleProductResponse,
    mode: ConsentMode,
    json: bool,
) -> anyhow::Result<LifecycleProductResponse> {
    if !matches!(
        response.phase,
        InstallationState::ConsentRequired | InstallationState::Tampered
    ) {
        return Ok(response);
    }
    let (Some(consent), Some(package_ref)) = (&response.consent, &response.package_ref) else {
        return Ok(response);
    };
    let granted = match mode {
        ConsentMode::Report => false,
        ConsentMode::Grant => {
            if !json {
                let mut summary = String::new();
                render_consent_summary(&mut summary, consent);
                eprint!("{summary}");
            }
            true
        }
        ConsentMode::Ask => {
            let mut summary = String::new();
            render_consent_summary(&mut summary, consent);
            eprint!("{summary}");
            ask_yes_no(&format!(
                "Allow plugin \"{}\" to run? [y/N] ",
                package_ref.id.as_str()
            ))?
        }
    };
    if !granted {
        return Ok(response);
    }
    run(
        runtime,
        RebornExtensionLifecycleCommand::GrantConsent {
            id: package_ref.id.as_str().to_string(),
            digest: consent.digest.as_str().to_string(),
        },
    )
    .await
}

fn ask_yes_no(prompt: &str) -> anyhow::Result<bool> {
    let mut stderr = std::io::stderr();
    stderr
        .write_all(prompt.as_bytes())
        .and_then(|()| stderr.flush())
        .context("failed to write the consent prompt")?;
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .context("failed to read the consent answer")?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn print_response(
    label: &str,
    response: &LifecycleProductResponse,
    json: bool,
) -> anyhow::Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string(&public_lifecycle_response_json(response)?)?
        );
    } else {
        print!(
            "{}",
            render_reborn_extension_lifecycle_response(label, response)
        );
        if matches!(
            response.phase,
            InstallationState::ConsentRequired | InstallationState::Tampered
        ) && let (Some(package_ref), Some(consent)) = (&response.package_ref, &response.consent)
        {
            println!(
                "to allow it: zlo extension grant {} --digest {}",
                package_ref.id.as_str(),
                consent.digest.as_str()
            );
        }
    }
    Ok(())
}

/// Where the local runtime discovers packages: `<storage root>/system/extensions`,
/// the on-disk backing of the `/system/extensions` mount.
fn local_extensions_dir(context: &RebornCliContext) -> PathBuf {
    let boot = context.boot_config();
    boot.home()
        .path()
        .join(boot.profile().local_runtime_storage_subdir())
        .join("system")
        .join("extensions")
}

fn scaffold(context: &RebornCliContext, command: ExtensionNewCommand) -> anyhow::Result<()> {
    let id = ExtensionId::new(command.id.clone())
        .map_err(|error| anyhow::anyhow!("invalid plugin id `{}`: {error}", command.id))?;
    let reserved = ironclaw_extension_host::first_party_reserved_extension_ids(
        &crate::first_party::bundled_first_party_bundles(),
    );
    if ironclaw_extension_host::available_extensions::reserved_host_bundled_extension_id(
        &id, &reserved,
    ) {
        anyhow::bail!(
            "`{}` is the id of an extension that ships with ZLO; pick another id",
            id.as_str()
        );
    }
    let root = local_extensions_dir(context).join(id.as_str());
    if root.exists() {
        anyhow::bail!("{} already exists", root.display());
    }
    let files = ironclaw_extension_host::extension_scaffold::scaffold_extension_package(&id);
    for (relative, bytes) in &files {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, bytes)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    if command.json {
        println!(
            "{}",
            serde_json::json!({
                "extension_id": id.as_str(),
                "path": root.display().to_string(),
                "files": files.iter().map(|(path, _)| path.as_str()).collect::<Vec<_>>(),
            })
        );
    } else {
        println!("created plugin {} at {}", id.as_str(), root.display());
        for (relative, _) in &files {
            println!("  {relative}");
        }
        println!(
            "next: set [mcp].server in manifest.toml, then `zlo extension install {}`",
            id.as_str()
        );
    }
    Ok(())
}

/// Build the runtime once, run `body` against it, and shut it down — so an
/// install, its consent grant, and the activation share one boot.
fn with_lifecycle_runtime<T>(
    context: RebornCliContext,
    confirm_host_access: bool,
    body: impl AsyncFnOnce(&RebornRuntime) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let runtime_services = crate::runtime::build_services_input_with_options(
        context.boot_config(),
        RuntimeInputCaller::Run,
        RuntimeInputOptions {
            confirm_host_access,
        },
    )?;
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build tokio runtime for extension lifecycle command")?;
    tokio_runtime.block_on(async move {
        let services_input =
            crate::runtime::with_binary_host_extension_bindings(runtime_services.services_input)?;
        let runtime = build_reborn_runtime(RebornRuntimeInput::from_build_input(services_input))
            .await
            .context("failed to assemble Reborn runtime for extension lifecycle command")?;
        let result = body(&runtime).await;
        runtime
            .shutdown()
            .await
            .context("failed to shut down Reborn runtime after extension lifecycle command")?;
        result
    })
}
