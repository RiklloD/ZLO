//! `zlo transfer user@host` — move this computer's setup to another one
//! over SSH in one step: `zlo export` into a private temporary file, then
//! `zlo import` on the other computer with the passphrase and the archive
//! sent over the SSH session's stdin (never on a command line, never in a
//! file there). Uses your SSH key through the system `ssh`; see
//! `crate::setup_bundle::ssh` for the argument rules.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::Context as _;
use clap::Args;
use secrecy::ExposeSecret as _;

use crate::context::RebornCliContext;
use crate::setup_bundle::{export, format, passphrase, ssh};

#[derive(Debug, Args)]
pub(crate) struct TransferCommand {
    /// The other computer, as user@host or user@host:port.
    #[arg(value_name = "USER@HOST")]
    target: String,
    /// Replace a setup the other computer already has (it is moved aside
    /// there, never deleted).
    #[arg(long)]
    replace: bool,
    /// Read the passphrase from this file's first line instead of asking.
    #[arg(long, value_name = "FILE")]
    passphrase_file: Option<PathBuf>,
}

impl TransferCommand {
    pub(crate) fn execute(self, context: RebornCliContext) -> anyhow::Result<()> {
        let target = ssh::parse_target(&self.target)?;
        let passphrase = match &self.passphrase_file {
            Some(path) => passphrase::from_file(path)?,
            None => passphrase::prompt(true)?,
        };
        let data_folder = context.boot_config().home().path().to_path_buf();
        let user_home = super::import::user_home()?;
        let hostname = crate::host_info::default_environment_label();
        let scratch = tempfile::Builder::new()
            .prefix("zlo-transfer-")
            .tempdir()
            .context("could not create a private temporary folder")?;
        let archive = scratch.path().join("setup.zlobundle");
        eprintln!("Exporting {} …", data_folder.display());
        let report = {
            let passphrase = passphrase.clone();
            let archive = archive.clone();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    export::export_setup(export::ExportRequest {
                        data_folder: &data_folder,
                        user_home: &user_home,
                        hostname: &hostname,
                        out: &archive,
                        passphrase: &passphrase,
                        iterations: format::DEFAULT_ITERATIONS,
                    })
                    .await
                })?
        };
        eprintln!(
            "Sending {} to {} …",
            super::export::human_bytes(report.bytes),
            target.destination()
        );
        let mut child = Command::new(ssh::SSH)
            .args(ssh::import_args(&target, self.replace))
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("could not run {}", ssh::SSH))?;
        let send = (|| -> anyhow::Result<()> {
            let mut stdin = child.stdin.take().context("ssh has no stdin")?;
            let mut line = zeroize::Zeroizing::new(passphrase.expose_secret().as_bytes().to_vec());
            line.push(b'\n');
            stdin.write_all(&line)?;
            let mut file = std::fs::File::open(&archive)?;
            std::io::copy(&mut file, &mut stdin)?;
            stdin.flush()?;
            Ok(())
        })();
        let status = child.wait().context("ssh did not finish")?;
        if !status.success() {
            anyhow::bail!(
                "the import on {} did not complete (ssh exit {}); its message is above. Your \
                 setup here is unchanged.",
                target.destination(),
                status
                    .code()
                    .map_or_else(|| "signal".to_string(), |c| c.to_string())
            );
        }
        send.context("could not send the setup over ssh")?;
        println!(
            "Moved the setup to {}. Start its backend there: ssh {} zlo service start",
            target.destination(),
            target.destination()
        );
        Ok(())
    }
}
