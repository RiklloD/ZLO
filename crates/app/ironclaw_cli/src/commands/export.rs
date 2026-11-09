//! `zlo export` — write this computer's ZLO setup (threads, memory,
//! profiles, skills, settings, providers and their keys, automations) to
//! one passphrase-encrypted `.zlobundle` file for `zlo import` on another
//! computer. Safe while the backend runs: the database is copied with
//! SQLite's own consistent snapshot. See `crate::setup_bundle`.

use std::path::PathBuf;

use clap::Args;

use crate::context::RebornCliContext;
use crate::setup_bundle::{export, format, passphrase};

#[derive(Debug, Args)]
pub(crate) struct ExportCommand {
    /// Where to write the setup file (default:
    /// ./zlo-setup-<computer>-<date>.zlobundle).
    #[arg(long, value_name = "FILE")]
    out: Option<PathBuf>,
    /// Read the passphrase from this file's first line instead of asking.
    #[arg(long, value_name = "FILE")]
    passphrase_file: Option<PathBuf>,
}

impl ExportCommand {
    pub(crate) fn execute(self, context: RebornCliContext) -> anyhow::Result<()> {
        let data_folder = context.boot_config().home().path().to_path_buf();
        let user_home = super::import::user_home()?;
        let hostname = crate::host_info::default_environment_label();
        let out = self
            .out
            .unwrap_or_else(|| export::default_out(&hostname, chrono::Local::now().date_naive()));
        let passphrase = match &self.passphrase_file {
            Some(path) => passphrase::from_file(path)?,
            None => passphrase::prompt(true)?,
        };
        eprintln!("Exporting {} …", data_folder.display());
        let report = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                export::export_setup(export::ExportRequest {
                    data_folder: &data_folder,
                    user_home: &user_home,
                    hostname: &hostname,
                    out: &out,
                    passphrase: &passphrase,
                    iterations: format::DEFAULT_ITERATIONS,
                })
                .await
            })?;
        println!("Wrote {}", report.out.display());
        println!(
            "  {} items, {}; {} database snapshot(s), {} stored secret(s), {} master key(s)",
            report.items,
            human_bytes(report.bytes),
            report.databases,
            report.secrets,
            report.master_keys
        );
        if report.environment_records_left_out > 0 {
            println!(
                "  Left out this computer's identity and paired devices ({} records).",
                report.environment_records_left_out
            );
        }
        for storage in &report.keyless_storage_roots {
            println!(
                "  Warning: {storage} has stored secrets but no master key was found; they will \
                 not decrypt on the other computer (re-enter those keys there)."
            );
        }
        println!("  Not included:");
        for note in &report.left_out {
            println!("    - {note}");
        }
        println!();
        println!(
            "Copy the file to the other computer and run:  zlo import {}",
            report.out.display()
        );
        println!("The file is encrypted with your passphrase; keep both safe.");
        Ok(())
    }
}

pub(crate) fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
