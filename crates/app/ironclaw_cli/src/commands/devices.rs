//! `ironclaw devices` — list paired devices, or revoke one (by id or label)
//! or all of them, on the running backend. Revocation is durable: a revoked
//! token gets 401 everywhere, including after a restart.

use anyhow::Context as _;
use clap::{Args, Subcommand};
use ironclaw_product_contracts::environment_access::DeviceSummary;

use crate::backend_client::RunningBackend;
use crate::context::RebornCliContext;
use crate::render::terminal_safe_text;

#[derive(Debug, Args)]
pub(crate) struct DevicesCommand {
    /// Output the device list as JSON.
    #[arg(long)]
    json: bool,
    #[command(subcommand)]
    action: Option<DevicesAction>,
}

#[derive(Debug, Subcommand)]
enum DevicesAction {
    /// Revoke a paired device by id or label, or every device with `--all`.
    Revoke {
        /// Device id (32 hex) or its exact label.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        device: Option<String>,
        /// Revoke every device that is not already revoked.
        #[arg(long)]
        all: bool,
    },
}

impl DevicesCommand {
    pub(crate) fn execute(self, context: RebornCliContext) -> anyhow::Result<()> {
        let backend = RunningBackend::from_context(&context)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build tokio runtime for `devices`")?;
        runtime.block_on(async {
            let devices = backend.list_devices().await?.devices;
            match self.action {
                None => {
                    if self.json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(
                                &serde_json::json!({ "devices": devices })
                            )?
                        );
                    } else {
                        print_devices(&devices);
                    }
                    Ok(())
                }
                Some(DevicesAction::Revoke { device, all }) => {
                    let targets = revoke_targets(&devices, device.as_deref(), all)?;
                    for target in &targets {
                        backend.revoke_device(&target.device_id).await?;
                        println!(
                            "Revoked {} ({})",
                            terminal_safe_text(&target.label),
                            target.device_id
                        );
                    }
                    if targets.is_empty() {
                        println!("No devices to revoke.");
                    }
                    Ok(())
                }
            }
        })
    }
}

fn print_devices(devices: &[DeviceSummary]) {
    if devices.is_empty() {
        println!("No paired devices. Run `ironclaw pair` to add one.");
        return;
    }
    for device in devices {
        let state = match device.revoked_at {
            Some(at) => format!("revoked {}", at.to_rfc3339()),
            None => match device.last_seen_at {
                Some(at) => format!("last seen {}", at.to_rfc3339()),
                None => "never used".to_string(),
            },
        };
        println!(
            "{}  {}  {} {}  paired {} via {}  {}",
            device.device_id,
            terminal_safe_text(&device.label),
            terminal_safe_text(&device.os),
            terminal_safe_text(&device.app_version),
            device.created_at.to_rfc3339(),
            match device.paired_by {
                ironclaw_product_contracts::environment_access::DevicePairedBy::OneTimeCode =>
                    "code",
                ironclaw_product_contracts::environment_access::DevicePairedBy::TailnetIdentity =>
                    "tailnet",
            },
            state,
        );
    }
}

/// Which devices a `revoke` names: `--all` → every live device; else an
/// exact id, else an exact (case-insensitive) label that must be unique
/// among live devices.
fn revoke_targets<'a>(
    devices: &'a [DeviceSummary],
    selector: Option<&str>,
    all: bool,
) -> anyhow::Result<Vec<&'a DeviceSummary>> {
    if all {
        return Ok(devices.iter().filter(|d| d.revoked_at.is_none()).collect());
    }
    let selector =
        selector.ok_or_else(|| anyhow::anyhow!("name a device id or label, or pass --all"))?;
    if let Some(device) = devices.iter().find(|d| d.device_id == selector) {
        return Ok(vec![device]);
    }
    let by_label: Vec<&DeviceSummary> = devices
        .iter()
        .filter(|d| d.revoked_at.is_none() && d.label.eq_ignore_ascii_case(selector))
        .collect();
    match by_label.len() {
        0 => anyhow::bail!(
            "no live device with id or label `{}`",
            terminal_safe_text(selector)
        ),
        1 => Ok(by_label),
        _ => anyhow::bail!(
            "{} live devices are labelled `{}`; revoke by id: {}",
            by_label.len(),
            terminal_safe_text(selector),
            by_label
                .iter()
                .map(|d| d.device_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use ironclaw_product_contracts::environment_access::DevicePairedBy;

    use super::*;

    fn device(id: char, label: &str, revoked: bool) -> DeviceSummary {
        DeviceSummary {
            device_id: id.to_string().repeat(32),
            label: label.to_string(),
            os: "macOS".into(),
            app_version: "0.1.0".into(),
            paired_by: DevicePairedBy::OneTimeCode,
            created_at: Utc::now(),
            last_seen_at: None,
            revoked_at: revoked.then(Utc::now),
            current: false,
        }
    }

    #[test]
    fn revoke_selects_by_id_then_unique_live_label() {
        let devices = vec![
            device('a', "Air", false),
            device('b', "Phone", false),
            device('c', "phone", false),
            device('d', "Old", true),
        ];
        let ids = |v: Vec<&DeviceSummary>| {
            v.into_iter()
                .map(|d| d.device_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(revoke_targets(&devices, Some(&"b".repeat(32)), false).expect("id")),
            vec!["b".repeat(32)]
        );
        assert_eq!(
            ids(revoke_targets(&devices, Some("air"), false).expect("label")),
            vec!["a".repeat(32)]
        );
        assert!(
            revoke_targets(&devices, Some("PHONE"), false).is_err(),
            "ambiguous label"
        );
        assert!(
            revoke_targets(&devices, Some("Old"), false).is_err(),
            "revoked labels do not match"
        );
        assert!(revoke_targets(&devices, Some("nobody"), false).is_err());
        assert_eq!(revoke_targets(&devices, None, true).expect("all").len(), 3);
    }
}
