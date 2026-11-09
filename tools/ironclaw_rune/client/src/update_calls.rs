//! "Update all machines": the operator self-update routes
//! (`ironclaw_product_contracts::self_update`). None takes a body; the
//! backend runs only what its own `[update]` config names.

use std::time::Duration;

use ironclaw_product_contracts::self_update::{
    OPERATOR_UPDATE_CHECK_PATH, OPERATOR_UPDATE_DISABLE_PATH, OPERATOR_UPDATE_ENABLE_PATH,
    OPERATOR_UPDATE_PATH, UpdateErrorCode, UpdateStatus,
};
use reqwest::Method;
use serde::Deserialize;

use crate::api::Client;

/// A check fetches over the network on the backend's side.
const CHECK_TIMEOUT: Duration = Duration::from_secs(150);
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// Why an update call did not answer a status.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateCallError {
    /// No answer at all (asleep, restarting, offline).
    #[error("not reachable: {0}")]
    Unreachable(String),
    /// `[update]` is not set up there; the backend's message says how.
    #[error("{0}")]
    NotSetUp(String),
    /// An update is already running there.
    #[error("an update is already running")]
    Running,
    /// The check was refused (dirty or diverged checkout, fetch failed).
    #[error("{0}")]
    Refused(String),
    /// The backend predates self-update.
    #[error("IronClaw there is too old to update itself")]
    TooOld,
    #[error("the saved token was rejected; pair it again")]
    Unauthorized,
    #[error("{0}")]
    Other(String),
}

#[derive(Deserialize, Default)]
struct ErrorBody {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

fn classify(status: u16, text: &str) -> UpdateCallError {
    let body: ErrorBody = serde_json::from_str(text).unwrap_or_default();
    let message = body.message.unwrap_or_else(|| format!("HTTP {status}"));
    let code = body.error.as_deref();
    match (status, code) {
        (401, _) => UpdateCallError::Unauthorized,
        (404 | 405, _) => UpdateCallError::TooOld,
        (_, Some(c)) if c == UpdateErrorCode::NotConfigured.as_str() => {
            UpdateCallError::NotSetUp(message)
        }
        (_, Some(c)) if c == UpdateErrorCode::UpdateRunning.as_str() => UpdateCallError::Running,
        (_, Some(c))
            if c == UpdateErrorCode::CheckFailed.as_str()
                || c == UpdateErrorCode::NoCheckout.as_str() =>
        {
            UpdateCallError::Refused(message)
        }
        // A proxy or a backend mid-restart.
        (502..=504, _) => UpdateCallError::Unreachable(message),
        _ => UpdateCallError::Other(message),
    }
}

impl Client {
    async fn update_call(
        &self,
        method: Method,
        path: &str,
        timeout: Duration,
    ) -> Result<UpdateStatus, UpdateCallError> {
        let resp = self
            .request(method, path)
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| UpdateCallError::Unreachable(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| UpdateCallError::Unreachable(e.to_string()))?;
        if !status.is_success() {
            return Err(classify(status.as_u16(), &text));
        }
        serde_json::from_str(&text).map_err(|e| UpdateCallError::Other(e.to_string()))
    }

    /// `GET /api/operator/update`: no network on the backend's side.
    pub async fn update_status(&self) -> Result<UpdateStatus, UpdateCallError> {
        self.update_call(Method::GET, OPERATOR_UPDATE_PATH, CALL_TIMEOUT)
            .await
    }

    /// `POST /api/operator/update/check`: the backend fetches its branch.
    pub async fn check_update(&self) -> Result<UpdateStatus, UpdateCallError> {
        self.update_call(Method::POST, OPERATOR_UPDATE_CHECK_PATH, CHECK_TIMEOUT)
            .await
    }

    /// `POST /api/operator/update/enable`: turn self-update on from the
    /// checkout that backend was built from. A backend that predates the
    /// route answers [`UpdateCallError::TooOld`].
    pub async fn enable_update(&self) -> Result<UpdateStatus, UpdateCallError> {
        self.update_call(Method::POST, OPERATOR_UPDATE_ENABLE_PATH, CALL_TIMEOUT)
            .await
    }

    /// `POST /api/operator/update/disable`: turn self-update off there. A
    /// backend that predates the route answers [`UpdateCallError::TooOld`].
    pub async fn disable_update(&self) -> Result<UpdateStatus, UpdateCallError> {
        self.update_call(Method::POST, OPERATOR_UPDATE_DISABLE_PATH, CALL_TIMEOUT)
            .await
    }

    /// `POST /api/operator/update`: start the update (`202`).
    pub async fn start_update(&self) -> Result<UpdateStatus, UpdateCallError> {
        self.update_call(Method::POST, OPERATOR_UPDATE_PATH, CALL_TIMEOUT)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{Connection, Token};
    use crate::test_http::serve;

    const STATUS: &str = r#"{"method":"source","current":{"version":"1.2.0","commit":"aaaa"},"available":{"commit":"bbbb","behind":3,"subjects":["c","b","a"],"checked_at":"2026-11-08T21:00:00Z"},"last_run":{"state":"idle","started_at":null,"finished_at":null,"message":null,"log_tail":""}}"#;

    fn client(url: String) -> Client {
        Client::new(Connection {
            base_url: url,
            token: Token::new("icd_dev_tok").unwrap(),
            token_source: "test".into(),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn the_calls_hit_their_routes_with_no_body() {
        let (url, seen) = serve(200, STATUS).await;
        let c = client(url);
        let status = c.update_status().await.unwrap();
        assert!(!status.can_enable, "absent from an older backend's answer");
        assert_eq!(status.available.unwrap().behind, 3);
        c.check_update().await.unwrap();
        c.start_update().await.unwrap();
        c.enable_update().await.unwrap();
        c.disable_update().await.unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[4].line, "POST /api/operator/update/disable HTTP/1.1");
        assert_eq!(seen[0].line, "GET /api/operator/update HTTP/1.1");
        assert_eq!(seen[1].line, "POST /api/operator/update/check HTTP/1.1");
        assert_eq!(seen[2].line, "POST /api/operator/update HTTP/1.1");
        assert_eq!(seen[3].line, "POST /api/operator/update/enable HTTP/1.1");
        for s in seen.iter() {
            assert_eq!(s.auth.as_deref(), Some("Bearer icd_dev_tok"));
            assert!(s.body.is_empty(), "nothing is sent: {:?}", s.body);
        }
    }

    #[tokio::test]
    async fn refusals_are_typed() {
        for (status, body, want) in [
            (
                409,
                r#"{"error":"not_configured","message":"Add [update]"}"#,
                UpdateCallError::NotSetUp("Add [update]".into()),
            ),
            (
                409,
                r#"{"error":"update_running","message":"busy"}"#,
                UpdateCallError::Running,
            ),
            (
                502,
                r#"{"error":"check_failed","message":"The checkout has local changes"}"#,
                UpdateCallError::Refused("The checkout has local changes".into()),
            ),
            (
                409,
                r#"{"error":"no_checkout","message":"Run zlo update enable --repo"}"#,
                UpdateCallError::Refused("Run zlo update enable --repo".into()),
            ),
            (404, "", UpdateCallError::TooOld),
            (401, "", UpdateCallError::Unauthorized),
        ] {
            let (url, _) = serve(status, body).await;
            assert_eq!(
                client(url).check_update().await.unwrap_err(),
                want,
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn a_backend_that_went_away_is_unreachable() {
        let c = client("http://127.0.0.1:9".into());
        assert!(matches!(
            c.update_status().await,
            Err(UpdateCallError::Unreachable(_))
        ));
    }
}
