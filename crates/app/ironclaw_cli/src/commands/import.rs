//! `zlo import <file>` — restore a setup written by `zlo export` into this
//! computer's data folder. Verifies the passphrase, format and every item
//! before anything is moved into place; refuses while this computer's
//! backend runs; with `--replace` moves an existing setup aside instead of
//! deleting it. See `crate::setup_bundle::import`.

use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

use clap::Args;

use crate::context::RebornCliContext;
use crate::setup_bundle::{import, passphrase};

#[derive(Debug, Args)]
pub(crate) struct ImportCommand {
    /// The `.zlobundle` file from `zlo export` (`-` reads it from stdin).
    #[arg(value_name = "FILE")]
    file: PathBuf,
    /// Read the passphrase from this file's first line instead of asking.
    #[arg(long, value_name = "FILE", conflicts_with = "passphrase_stdin")]
    passphrase_file: Option<PathBuf>,
    /// Read the passphrase from the first line of stdin (what `zlo
    /// transfer` sends over SSH; the archive may follow on stdin).
    #[arg(long)]
    passphrase_stdin: bool,
    /// Import even though this computer already has a setup: it is moved
    /// to `<data folder>.before-import-<time>` first, never deleted.
    #[arg(long)]
    replace: bool,
}

/// The user's home folder (`HOME`, else `USERPROFILE`).
pub(crate) fn user_home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set"))
}

/// Why this computer's backend counts as running, if it does: the OS
/// service is running, or something answers on the configured loopback
/// port.
fn backend_running(context: &RebornCliContext) -> anyhow::Result<Option<String>> {
    if let Ok(platform) = super::service::ServicePlatform::detect() {
        match platform.current_state() {
            Ok(super::service::ServiceState::Running) => {
                return Ok(Some("the zlo service is running".to_string()));
            }
            Ok(_) => {}
            // The listener check below still runs; an unreadable service
            // manager alone is no reason to refuse.
            Err(error) => tracing::debug!(%error, "could not read the service state"),
        }
    }
    let home = context.boot_config().home();
    let config = ironclaw_config::RebornConfigFile::load(&home.config_file_path())
        .map_err(anyhow::Error::from)?;
    let webui = config.as_ref().and_then(|file| file.webui.as_ref());
    let origin = crate::backend_client::loopback_origin(
        webui.and_then(|section| section.listen_host.as_deref()),
        webui.and_then(|section| section.listen_port),
    )?;
    let address = origin.trim_start_matches("http://");
    if let Ok(address) = address.parse::<std::net::SocketAddr>()
        && std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(500))
            .is_ok()
    {
        return Ok(Some(format!("something is answering on {origin}")));
    }
    Ok(None)
}

impl ImportCommand {
    pub(crate) fn execute(self, context: RebornCliContext) -> anyhow::Result<()> {
        let data_folder = context.boot_config().home().path().to_path_buf();
        self.execute_with(&data_folder, &user_home()?, || backend_running(&context))
    }

    fn execute_with(
        self,
        data_folder: &Path,
        user_home: &Path,
        running: impl FnOnce() -> anyhow::Result<Option<String>>,
    ) -> anyhow::Result<()> {
        let from_stdin = self.file.as_os_str() == "-";
        if from_stdin && !self.passphrase_stdin && self.passphrase_file.is_none() {
            anyhow::bail!(
                "reading the setup file from stdin needs --passphrase-stdin or --passphrase-file"
            );
        }
        let file = if from_stdin {
            None
        } else {
            Some(import::open_archive(&self.file)?)
        };
        if let Some(why) = running()? {
            anyhow::bail!(
                "ZLO's backend is running on this computer ({why}). Stop it first with \
                 `zlo service stop` (or quit `zlo serve`), then import again."
            );
        }
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        let passphrase = if self.passphrase_stdin {
            passphrase::from_first_line(&mut stdin)?
        } else if let Some(path) = &self.passphrase_file {
            passphrase::from_file(path)?
        } else {
            passphrase::prompt(false)?
        };
        let input: Box<dyn Read> = match file {
            Some(file) => Box::new(std::io::BufReader::new(file)),
            None => Box::new(StdinRest(stdin)),
        };
        eprintln!("Importing into {} …", data_folder.display());
        let request = import::ImportRequest {
            data_folder,
            user_home,
            passphrase: &passphrase,
            replace: self.replace,
            now: chrono::Utc::now(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let report = runtime.block_on(import::import_setup(input, request))?;
        print_report(&report, data_folder);
        Ok(())
    }
}

/// The rest of stdin after the passphrase line (the lock's buffer first).
struct StdinRest<'a>(std::io::StdinLock<'a>);

impl Read for StdinRest<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let available = self.0.fill_buf()?;
        let n = available.len().min(buf.len());
        buf[..n].copy_from_slice(&available[..n]);
        self.0.consume(n);
        Ok(n)
    }
}

fn print_report(report: &import::ImportReport, data_folder: &Path) {
    println!(
        "Imported the setup from {} ({}, exported {}) into {}",
        report.source.hostname,
        report.source.os,
        report.created_at.format("%Y-%m-%d %H:%M UTC"),
        data_folder.display()
    );
    println!(
        "  {} items, {}",
        report.items,
        super::export::human_bytes(report.bytes)
    );
    for (database, summary) in &report.databases {
        println!(
            "  {database}: {} threads, {} profiles, {} automations, {} memory documents, \
             {} stored secrets",
            summary.threads,
            summary.profiles,
            summary.automations,
            summary.memory_documents,
            summary.secrets
        );
    }
    println!(
        "  Provider keys and secrets: master key restored for {} storage folder(s)",
        report.master_keys
    );
    if report.paths_rewritten > 0 {
        println!(
            "  Rewrote {} path(s) from {} to {}",
            report.paths_rewritten,
            report.source.user_home,
            user_home()
                .map(|h| h.display().to_string())
                .unwrap_or_default() // silent-ok: display only
        );
    }
    if let Some(aside) = &report.moved_aside {
        println!("  The previous setup was moved to {}", aside.display());
    }
    if report.environment_records_kept > 0 {
        println!("  Kept this computer's own identity and paired devices.");
    } else {
        println!(
            "  This computer gets its own identity on first start; pair your devices with it \
             again (`zlo pair`)."
        );
    }
    println!("  Not carried over:");
    for note in &report.left_out {
        println!("    - {note}");
    }
    if !report.missing_folders.is_empty() {
        println!();
        println!("These folders are used by the imported setup but are not on this computer.");
        println!("Clone or copy them here (same paths) so those profiles and threads can work:");
        for folder in &report.missing_folders {
            println!("    {folder}");
        }
    }
    println!();
    println!("Start the backend: `zlo service start` (or `zlo service install` the first time).");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_backend_is_refused_before_anything_is_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("setup.zlobundle");
        std::fs::write(&file, b"never read").expect("archive");
        let command = ImportCommand {
            file,
            passphrase_file: None,
            passphrase_stdin: false,
            replace: false,
        };
        let data = dir.path().join(".zlo");
        let error = command
            .execute_with(&data, dir.path(), || {
                Ok(Some("the zlo service is running".to_string()))
            })
            .expect_err("must refuse");
        assert!(error.to_string().contains("zlo service stop"), "{error}");
        assert!(!data.exists());
    }
}
