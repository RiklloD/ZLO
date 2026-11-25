//! Which Claude account a CLI login file is signed in to.
//!
//! The CLI writes `.claude.json` into its config dir on any run, signed in or
//! not, so the file's presence says nothing. A signed-in login carries an
//! `oauthAccount` with the account's `accountUuid`; that is what "Ready" and
//! "is this a second account" are decided from.

use std::path::Path;

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginFile {
    #[serde(default)]
    oauth_account: Option<OauthAccount>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OauthAccount {
    #[serde(default)]
    account_uuid: Option<String>,
}

/// The `accountUuid` a login file's text is signed in to; `None` when it is
/// not signed in or not a login file.
pub(crate) fn account_in(text: &str) -> Option<String> {
    let login: LoginFile = match serde_json::from_str(text) {
        Ok(login) => login,
        Err(error) => {
            tracing::debug!(%error, "claude cli: login file not readable as JSON");
            return None;
        }
    };
    login
        .oauth_account?
        .account_uuid
        .filter(|uuid| !uuid.trim().is_empty())
}

/// [`account_in`] for the file at `path`, read blocking (startup only).
pub(crate) fn account_at(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => account_in(&text),
        Err(error) => {
            tracing::debug!(%error, path = %path.display(), "claude cli: no login file");
            None
        }
    }
}

/// [`account_in`] for the file at `path`.
pub(crate) async fn signed_in(path: &Path) -> bool {
    match tokio::fs::read_to_string(path).await {
        Ok(text) => account_in(&text).is_some(),
        Err(error) => {
            tracing::debug!(%error, "claude cli: no login file");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::account_in;

    #[test]
    fn only_a_login_with_an_oauth_account_is_signed_in() {
        assert_eq!(
            account_in(r#"{"oauthAccount":{"accountUuid":"a-1"}}"#).as_deref(),
            Some("a-1")
        );
        // What the CLI leaves behind after a run that never signed in.
        assert_eq!(account_in(r#"{"numStartups":1,"projects":{}}"#), None);
        assert_eq!(account_in(r#"{"oauthAccount":null}"#), None);
        assert_eq!(account_in(r#"{"oauthAccount":{"accountUuid":" "}}"#), None);
        assert_eq!(account_in("not json"), None);
    }
}
