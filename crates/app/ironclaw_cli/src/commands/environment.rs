//! `zlo environment` — link this machine to your other machines so agents
//! here can work there (`t3_*` tools with `environment`, `t3_delegate` to
//! another machine).
//!
//! On the other machine, `zlo pair --link operate` makes a peer-link code;
//! here, `zlo environment link <its address> --pairing-code <code> --access
//! operate` redeems it. The token stays in this machine's secret store.

use anyhow::Context as _;
use clap::{Args, Subcommand};
use ironclaw_product_contracts::environment_access::PeerLinkAccess;
use ironclaw_product_contracts::peer::LinkEnvironmentRequest;

use crate::backend_client::RunningBackend;
use crate::context::RebornCliContext;

#[derive(Debug, Args)]
pub(crate) struct EnvironmentCommand {
    #[command(subcommand)]
    action: EnvironmentAction,
}

#[derive(Debug, Subcommand)]
enum EnvironmentAction {
    /// Link another machine with a peer-link code from it (`zlo pair --link`).
    Link {
        /// Its address: https (e.g. its Tailscale HTTPS name) or loopback.
        url: String,
        #[arg(long)]
        pairing_code: String,
        /// `read_only`, `operate` or `full`; must match the code.
        #[arg(long, default_value = "operate")]
        access: String,
    },
    /// List linked machines.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Forget a linked machine here (revoke it there to end its grant).
    Forget {
        /// Its environment id or label.
        environment: String,
    },
}

impl EnvironmentCommand {
    pub(crate) fn execute(self, context: RebornCliContext) -> anyhow::Result<()> {
        let backend = RunningBackend::from_context(&context)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build tokio runtime for `environment`")?;
        runtime.block_on(async move {
            match self.action {
                EnvironmentAction::Link {
                    url,
                    pairing_code,
                    access,
                } => {
                    let access = PeerLinkAccess::parse(&access).ok_or_else(|| {
                        anyhow::anyhow!("--access must be read_only, operate or full")
                    })?;
                    let link = backend
                        .link_environment(&LinkEnvironmentRequest {
                            url,
                            pairing_code,
                            access,
                        })
                        .await?;
                    println!(
                        "Linked {} ({}) with {} access until {}.",
                        link.label,
                        link.environment_id,
                        link.access,
                        link.expires_at.format("%Y-%m-%d")
                    );
                    println!("Agents here can now pass environment=\"{}\" to t3_* tools.", link.label);
                }
                EnvironmentAction::List { json } => {
                    let list = backend.peer_links().await?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&list)?);
                    } else if list.links.is_empty() {
                        println!("No linked machines. Link one with `zlo environment link`.");
                    } else {
                        for link in list.links {
                            println!(
                                "{:<20} {:<9} until {}  {}{}",
                                link.label,
                                link.access.as_str(),
                                link.expires_at.format("%Y-%m-%d"),
                                link.urls.join(", "),
                                if link.revoked { "  (revoked)" } else { "" }
                            );
                        }
                    }
                }
                EnvironmentAction::Forget { environment } => {
                    let list = backend.peer_links().await?;
                    let lower = environment.to_ascii_lowercase();
                    let link = list
                        .links
                        .iter()
                        .find(|l| l.environment_id == environment)
                        .or_else(|| list.links.iter().find(|l| l.label.to_ascii_lowercase() == lower))
                        .ok_or_else(|| anyhow::anyhow!("no linked machine named {environment:?}"))?;
                    backend.forget_environment(&link.environment_id).await?;
                    println!(
                        "Forgot {} here. Revoke its grant on that machine with `zlo devices revoke`.",
                        link.label
                    );
                }
            }
            Ok(())
        })
    }
}
