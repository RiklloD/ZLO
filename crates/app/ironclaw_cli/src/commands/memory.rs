//! `zlo memory reembed` — give every stored memory chunk a vector in the
//! current embedding space (`[embeddings]`), now rather than at each
//! document's next write. The job is native memory's own
//! (`ironclaw_memory_native::MemoryReembedJob`, reached through the composed
//! runtime): the same provider, network egress, and host-side key as
//! `memory.write`, a durable cursor so an interrupted run resumes, and no
//! work at all once the backlog is done. `--dry-run` only counts.

use anyhow::Context;
use clap::{Args, Subcommand};
use ironclaw_composition::{RebornRuntimeInput, build_reborn_runtime};
use serde_json::json;

use crate::context::RebornCliContext;
use crate::runtime::{RuntimeInputCaller, RuntimeInputOptions};

#[derive(Debug, Args)]
pub(crate) struct MemoryCommand {
    /// Confirm trusted-laptop host filesystem access for the unrestricted standalone profile.
    #[arg(long = "confirm-host-access", global = true)]
    confirm_host_access: bool,

    #[command(subcommand)]
    command: MemorySubcommand,
}

#[derive(Debug, Subcommand)]
enum MemorySubcommand {
    /// Embed memory chunks that have no vector for the configured embedding model.
    Reembed(MemoryReembedCommand),
}

#[derive(Debug, Args)]
struct MemoryReembedCommand {
    /// Count the backlog only: no embedding call, no write.
    #[arg(long)]
    dry_run: bool,

    /// Embed at most this many chunks in this run (the rest resume next run).
    #[arg(long)]
    limit: Option<u64>,

    /// Output the final report as JSON.
    #[arg(long)]
    json: bool,
}

impl MemoryCommand {
    pub(crate) fn execute(self, context: RebornCliContext) -> anyhow::Result<()> {
        crate::runtime::init_tracing();
        let MemorySubcommand::Reembed(command) = self.command;
        let runtime_services = crate::runtime::build_services_input_with_options(
            context.boot_config(),
            RuntimeInputCaller::Run,
            RuntimeInputOptions {
                confirm_host_access: self.confirm_host_access,
            },
        )?;
        let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to build tokio runtime for `memory reembed`")?;
        tokio_runtime.block_on(async move {
            let services_input =
                crate::runtime::with_binary_host_extension_bindings(runtime_services.services_input)?;
            let runtime = build_reborn_runtime(RebornRuntimeInput::from_build_input(services_input))
                .await
                .context("failed to assemble Reborn runtime for `memory reembed`")?;
            let show_progress = !command.json && !command.dry_run;
            let result = runtime
                .reembed_memory(command.dry_run, command.limit, &mut |progress| {
                    if show_progress && progress.embedded > 0 {
                        eprintln!(
                            "embedded {} chunk(s), {} left",
                            progress.embedded_total, progress.backlog
                        );
                    }
                })
                .await;
            runtime
                .shutdown()
                .await
                .context("failed to shut down Reborn runtime after `memory reembed`")?;
            let Some(report) = result.context("memory re-embed failed on storage")? else {
                if command.json {
                    println!("{}", json!({"configured": false}));
                } else {
                    println!(
                        "Nothing to re-embed: no [embeddings] provider is configured for native memory."
                    );
                }
                return Ok(());
            };
            if command.json {
                println!(
                    "{}",
                    json!({
                        "configured": true,
                        "fingerprint": report.fingerprint,
                        "outcome": report.outcome.as_str(),
                        "backlog": report.backlog,
                        "ineligible": report.ineligible,
                        "embedded": report.embedded,
                        "superseded": report.superseded,
                        "remaining": report.remaining,
                        "provider_calls": report.provider_calls,
                        "resumed": report.resumed,
                    })
                );
            } else if command.dry_run {
                println!(
                    "Backlog: {} chunk(s) without a vector for {} ({} too long or blank to embed).",
                    report.backlog, report.fingerprint, report.ineligible
                );
            } else {
                println!(
                    "Re-embed {}: embedded {} of {} chunk(s), {} left ({} replaced by newer writes, {} too long or blank).",
                    report.outcome.as_str(),
                    report.embedded,
                    report.backlog,
                    report.remaining,
                    report.superseded,
                    report.ineligible
                );
            }
            if report.outcome.is_failure() {
                anyhow::bail!(
                    "memory re-embed stopped: {} (progress is saved; run it again to resume)",
                    report.outcome.as_str()
                );
            }
            Ok(())
        })
    }
}
