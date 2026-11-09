//! `zlo pair` — issue a one-time pairing code on the running backend
//! and print it with a pairing link. The code is valid five minutes and
//! works once; a device redeems it at `POST /api/auth/pair`.

use anyhow::Context as _;
use clap::Args;
use serde::Serialize;

use crate::backend_client::RunningBackend;
use crate::context::RebornCliContext;

#[derive(Debug, Args)]
pub(crate) struct PairCommand {
    /// Output `{"code", "expires_at", "link"}` as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Serialize)]
struct PairOutput {
    code: String,
    expires_at: chrono::DateTime<chrono::Utc>,
    link: String,
}

impl PairCommand {
    pub(crate) fn execute(self, context: RebornCliContext) -> anyhow::Result<()> {
        let backend = RunningBackend::from_context(&context)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build tokio runtime for `pair`")?;
        let (issued, descriptor) = runtime.block_on(async {
            let issued = backend.issue_pairing_code().await?;
            let descriptor = backend.descriptor().await?;
            Ok::<_, anyhow::Error>((issued, descriptor))
        })?;
        let base = descriptor
            .tailnet_url
            .unwrap_or_else(|| backend.origin().to_string());
        let output = PairOutput {
            link: pairing_link(&base, &issued.code),
            code: issued.code,
            expires_at: issued.expires_at,
        };
        if self.json {
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("Pairing code: {}", output.code);
            println!(
                "Expires:      {} (single use)",
                output.expires_at.to_rfc3339()
            );
            println!("Link:         {}", output.link);
            println!();
            println!(
                "Open the link on the new device, or enter the code in ZLO under \
                 Settings > Computers."
            );
        }
        Ok(())
    }
}

/// `<base>/pair#code=<code>`. The code rides the fragment so it never
/// reaches a server log.
pub(crate) fn pairing_link(base: &str, code: &str) -> String {
    format!("{}/pair#code={code}", base.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_puts_the_code_in_the_fragment() {
        assert_eq!(
            pairing_link("https://studio.tail9915dd.ts.net/", "7KQ2-M9XD-4TRA"),
            "https://studio.tail9915dd.ts.net/pair#code=7KQ2-M9XD-4TRA"
        );
        assert_eq!(
            pairing_link("http://127.0.0.1:3000", "7KQ2-M9XD-4TRA"),
            "http://127.0.0.1:3000/pair#code=7KQ2-M9XD-4TRA"
        );
    }
}
