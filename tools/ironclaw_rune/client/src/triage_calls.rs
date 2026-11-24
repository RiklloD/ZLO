//! "Triage an issue" (`ironclaw_product_contracts::triage`): ask the
//! backend to prepare a session, and tell whether a message is a triage
//! draft. Moving a draft to another computer swaps its playbook chip in the
//! app (`workspace/triage.rs`).

use ironclaw_product_contracts::triage::{OPERATOR_TRIAGE_PATH, PrepareTriageRequest};
// The app names these without depending on the contracts crate itself.
pub use ironclaw_product_contracts::triage::{
    PrepareTriageResponse, TRIAGE_PROMPT_FILE_NAME, TriageClient,
};

use crate::api::Client;
use crate::error::ClientError;

/// What a user can run when the app cannot start triage.
pub const TRIAGE_CLI_COMMAND: &str = "zlo triage";

impl Client {
    /// `POST /api/operator/triage`: the backend writes the session files,
    /// starts fetching its source, and answers the composer text.
    pub async fn prepare_triage(
        &self,
        client: TriageClient,
    ) -> Result<PrepareTriageResponse, ClientError> {
        self.post(OPERATOR_TRIAGE_PATH, &PrepareTriageRequest { client })
            .await
    }
}

/// The start of a playbook link: `[@triage-playbook.md](`.
fn link_start() -> String {
    format!("[@{TRIAGE_PROMPT_FILE_NAME}](")
}

/// A draft is a triage draft while its text still links the playbook file.
pub fn is_triage_prompt(text: &str) -> bool {
    text.contains(&link_start())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{Connection, Token};
    use crate::test_http::serve;
    use ironclaw_product_contracts::mention::MentionUri;

    fn app_prompt(path: &str) -> String {
        let link = MentionUri::File {
            path: path.into(),
            line_range: None,
        }
        .to_link(TRIAGE_PROMPT_FILE_NAME);
        format!("Read {link} and follow it. ")
    }

    const MACHINE_A: &str = "/Users/a/.zlo/triage/1/triage-playbook.md";

    #[test]
    fn a_draft_is_a_triage_draft_while_it_links_the_playbook() {
        let text = "The app crashes, see [@log.md](file:///Users/a/log.md) please.";
        assert!(!is_triage_prompt(text));
        assert!(is_triage_prompt(&app_prompt(MACHINE_A)));
        assert!(is_triage_prompt(&app_prompt(
            "/Users/a b/zlo (dev)/triage-playbook.md"
        )));
    }

    #[tokio::test]
    async fn prepare_posts_the_client_and_reads_the_prompt() {
        let (url, seen) = serve(
            200,
            r#"{"prompt":"Read [@triage-playbook.md](file:///h/t/triage-playbook.md) and follow it. ","server_version":"1.2.0"}"#,
        )
        .await;
        let client = Client::new(Connection {
            base_url: url,
            token: Token::new("icd_dev_tok").unwrap(),
            token_source: "test".into(),
        })
        .unwrap();
        let prepared = client
            .prepare_triage(TriageClient {
                label: Some("ZLO desktop".into()),
                app_version: None,
                os: None,
            })
            .await
            .unwrap();
        assert!(is_triage_prompt(&prepared.prompt));
        assert_eq!(prepared.server_version, "1.2.0");
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].line, "POST /api/operator/triage HTTP/1.1");
        assert!(
            seen[0].body.contains(r#""label":"ZLO desktop""#),
            "{}",
            seen[0].body
        );
    }
}
