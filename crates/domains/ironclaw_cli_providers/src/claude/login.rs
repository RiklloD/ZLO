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
        .map(|uuid| uuid.trim().to_string())
        .filter(|uuid| !uuid.is_empty())
}

/// Whether a login file's text holds a console API key (`/login` with an
/// Anthropic Console account), which runs turns without an `oauthAccount`.
fn has_api_key(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok() // silent-ok: unparsable is reported by account_in
        .and_then(|v| {
            v.get("primaryApiKey")?
                .as_str()
                .map(|k| !k.trim().is_empty())
        })
        .unwrap_or(false)
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

/// Whether turns on this login can run: an OAuth account or console key in
/// the file at `path`, or an `ANTHROPIC_API_KEY` the CLI inherits (it takes
/// precedence over the login).
pub(crate) async fn signed_in(path: &Path) -> bool {
    if std::env::var_os("ANTHROPIC_API_KEY").is_some_and(|k| !k.is_empty()) {
        return true;
    }
    match tokio::fs::read_to_string(path).await {
        Ok(text) => account_in(&text).is_some() || has_api_key(&text),
        Err(error) => {
            tracing::debug!(%error, "claude cli: no login file");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{account_in, has_api_key};

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
        assert_eq!(
            account_in(r#"{"oauthAccount":{"accountUuid":" a-1 "}}"#).as_deref(),
            Some("a-1"),
            "trimmed, as the Usage page compares them"
        );
        assert!(has_api_key(r#"{"primaryApiKey":"k"}"#));
        assert!(!has_api_key(r#"{"primaryApiKey":""}"#));
        assert_eq!(account_in("not json"), None);
    }
}
