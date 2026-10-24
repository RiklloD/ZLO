//! Webhook schedules (`t3_schedule` with `{type:'webhook'}`): each request to
//! the schedule's loopback URL is one run. The URL carries the schedule id
//! and an unguessable token; an optional signature adds an HMAC-SHA256
//! check of the raw body (`{header, encoding:'hex'|'base64', prefix,
//! secret}`), as in the Hermes t3code orchestrator.
//!
//! The prompt may quote the request through placeholders: `{{body}}`,
//! `{{body.a.b}}` (JSON body path), `{{headers.name}}`, `{{query.name}}` and
//! `{{request}}`. The request is untrusted, so the rendered prompt is bounded
//! and runs through the planner's unattended-prompt scan before it is posted.
//! The HTTP route lives in composition; this module decides everything.

use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::engine::{Engine, Result};
use crate::error::OrchestratorError;
use crate::model::{ScheduleId, now_ms};
use crate::schedules::{ScheduleRecord, is_webhook};
use crate::store::s;

/// Path prefix of the webhook route; the last segment is `<id>.<token>`.
pub const WEBHOOK_PATH_PREFIX: &str = "/api/orchestrator/webhooks/";
/// Largest request body a webhook accepts.
pub const MAX_WEBHOOK_BODY_BYTES: usize = 64 * 1024;
/// Same bound as a stored prompt.
const MAX_RENDERED_BYTES: usize = 32 * 1024;
/// Headers never quoted into a prompt.
const HIDDEN_HEADERS: [&str; 3] = ["authorization", "cookie", "proxy-authorization"];

/// One webhook request as the route saw it.
#[derive(Debug, Clone, Default)]
pub struct WebhookRequest {
    pub method: String,
    pub path: String,
    /// Names lowercase.
    pub headers: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Why a webhook request did not run; [`WebhookRefusal::status`] is the
/// HTTP answer. Details stay server-side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookRefusal {
    /// Unknown schedule, wrong token, or not a webhook schedule.
    NotFound,
    /// Signature missing or wrong.
    Unauthorized,
    /// The schedule is paused.
    Paused,
    /// The rendered prompt is too large or the safety scan refused it.
    Rejected,
    Internal,
}

impl WebhookRefusal {
    pub fn status(&self) -> u16 {
        match self {
            Self::NotFound => 404,
            Self::Unauthorized => 401,
            Self::Paused => 409,
            Self::Rejected => 422,
            Self::Internal => 500,
        }
    }
}

pub(crate) fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// Validate a `signature` argument against the spec; `None` clears it.
pub(crate) fn check_signature(spec: &Value, raw: Option<&Value>) -> Result<Option<Value>> {
    let Some(raw) = raw.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    if !is_webhook(spec) {
        return Err(OrchestratorError::invalid(
            "signature applies only to webhook schedules",
        ));
    }
    let text = |key: &str| raw.get(key).and_then(Value::as_str).map(str::trim);
    let header = text("header")
        .filter(|h| !h.is_empty())
        .ok_or_else(|| OrchestratorError::invalid("signature.header is required"))?
        .to_ascii_lowercase();
    let secret = text("secret")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| OrchestratorError::invalid("signature.secret is required"))?;
    let encoding = match text("encoding").unwrap_or("hex") {
        e @ ("hex" | "base64") => e,
        _ => {
            return Err(OrchestratorError::invalid(
                "signature.encoding must be hex or base64",
            ));
        }
    };
    Ok(Some(json!({
        "header": header,
        "encoding": encoding,
        "prefix": text("prefix").unwrap_or(""),
        "secret": secret,
    })))
}

fn signature_ok(signature: &Value, request: &WebhookRequest) -> bool {
    let field = |key: &str| signature.get(key).and_then(Value::as_str).unwrap_or("");
    let Some(presented) = header(request, field("header")) else {
        return false;
    };
    let Some(presented) = presented.trim().strip_prefix(field("prefix")) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(field("secret").as_bytes()) else {
        return false;
    };
    mac.update(&request.body);
    let digest = mac.finalize().into_bytes();
    let expected = match field("encoding") {
        "base64" => base64::engine::general_purpose::STANDARD.encode(digest),
        _ => hex::encode(digest),
    };
    bool::from(expected.as_bytes().ct_eq(presented.as_bytes()))
}

fn header<'a>(request: &'a WebhookRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Fill the prompt's placeholders from the request. Unknown ones stay.
fn render(prompt: &str, request: &WebhookRequest) -> String {
    let body_text = String::from_utf8_lossy(&request.body);
    let body_json: Option<Value> = serde_json::from_slice(&request.body).ok(); // silent-ok: a non-JSON body only has {{body}}
    let visible = |name: &str| !HIDDEN_HEADERS.contains(&name.to_ascii_lowercase().as_str());
    let mut out = String::with_capacity(prompt.len());
    let mut rest = prompt;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = after[..end].trim();
        let value = match key.split_once('.') {
            None if key == "body" => Some(body_text.to_string()),
            None if key == "request" => Some(
                json!({
                    "method": request.method,
                    "path": request.path,
                    "headers": request.headers.iter().filter(|(k, _)| visible(k))
                        .map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<_, _>>(),
                    "query": request.query.iter()
                        .map(|(k, v)| (k.clone(), json!(v))).collect::<serde_json::Map<_, _>>(),
                    "body": body_json.clone().unwrap_or_else(|| json!(body_text)),
                })
                .to_string(),
            ),
            Some(("body", path)) => Some(
                body_json
                    .as_ref()
                    .and_then(|b| b.pointer(&format!("/{}", path.replace('.', "/"))))
                    .map(|v| match v {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .unwrap_or_default(),
            ),
            Some(("headers", name)) if visible(name) => {
                Some(header(request, name).unwrap_or_default().to_string())
            }
            Some(("headers", _)) => Some(String::new()),
            Some(("query", name)) => Some(
                request
                    .query
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default(),
            ),
            _ => None,
        };
        match value {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

impl Engine {
    /// The gateway's loopback origin, set once the listener is bound.
    pub fn set_webhook_base(&self, origin: String) {
        let _ = self.webhook_base.set(origin); // silent-ok: the gateway binds once per process
    }

    pub(crate) fn webhook_url(&self, r: &ScheduleRecord) -> Option<String> {
        let base = self.webhook_base.get()?;
        let token = r.webhook_token.as_deref()?;
        Some(format!("{base}{WEBHOOK_PATH_PREFIX}{}.{token}", r.id))
    }

    /// Run the webhook schedule named by the URL's last segment
    /// (`<id>.<token>`) for one request.
    pub async fn fire_webhook(
        &self,
        key: &str,
        request: &WebhookRequest,
    ) -> std::result::Result<Value, WebhookRefusal> {
        let (id, token) = key.split_once('.').ok_or(WebhookRefusal::NotFound)?;
        let id = ScheduleId::new(id).map_err(|_| WebhookRefusal::NotFound)?;
        let record = self
            .schedule(&id)
            .await
            .map_err(|error| internal("schedule lookup", &error))?
            .ok_or(WebhookRefusal::NotFound)?;
        let known = record.webhook_token.as_deref().unwrap_or_default();
        if !is_webhook(&record.spec)
            || known.is_empty()
            || !bool::from(known.as_bytes().ct_eq(token.as_bytes()))
        {
            return Err(WebhookRefusal::NotFound);
        }
        if let Some(signature) = &record.signature
            && !signature_ok(signature, request)
        {
            return Err(WebhookRefusal::Unauthorized);
        }
        if !record.enabled {
            return Err(WebhookRefusal::Paused);
        }
        let prompt = render(&record.prompt, request);
        let scanned = if prompt.len() > MAX_RENDERED_BYTES {
            Err(OrchestratorError::invalid("rendered prompt is over 32 KiB"))
        } else {
            self.planner()
                .and_then(|planner| planner.check_prompt(&prompt))
        };
        if let Err(error) = scanned {
            tracing::debug!(%error, schedule = %record.id, "webhook prompt refused");
            let _ = self
                .store
                .exec(
                    "UPDATE orch_schedules SET last_error = ?2 WHERE id = ?1",
                    vec![s(record.id.as_str()), s(error.to_string())],
                )
                .await; // silent-ok: the refusal itself is the answer; last_error is advisory
            return Err(WebhookRefusal::Rejected);
        }
        let (queued, thread) = self
            .post_schedule(&record, &prompt, now_ms())
            .await
            .map_err(|error| internal("webhook post", &error))?;
        Ok(json!({"ok": true, "queuedId": queued.as_str(), "threadId": thread.as_str()}))
    }
}

fn internal(what: &str, error: &OrchestratorError) -> WebhookRefusal {
    tracing::debug!(%error, "{what} failed");
    WebhookRefusal::Internal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(body: &str) -> WebhookRequest {
        WebhookRequest {
            method: "POST".into(),
            path: "/hook".into(),
            headers: vec![
                ("x-event".into(), "push".into()),
                ("authorization".into(), "Bearer secret".into()),
            ],
            query: vec![("ref".into(), "main".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn placeholders_quote_the_request_but_never_its_credentials() {
        let req = request(r#"{"repo":{"name":"ironclaw"},"n":3}"#);
        assert_eq!(
            render(
                "{{headers.x-event}} on {{body.repo.name}} ({{query.ref}}, {{body.n}}) {{nope}}",
                &req
            ),
            "push on ironclaw (main, 3) {{nope}}"
        );
        assert_eq!(render("{{headers.authorization}}", &req), "");
        let whole = render("{{request}}", &req);
        assert!(
            whole.contains("ironclaw") && !whole.contains("Bearer"),
            "{whole}"
        );
        assert_eq!(render("{{ body }} {{", &request("hi")), "hi {{");
    }

    #[test]
    fn signatures_check_the_raw_body() {
        let spec = json!({"type": "webhook"});
        let sig = check_signature(
            &spec,
            Some(&json!({"header": "X-Sig", "prefix": "sha256=", "secret": "k"})),
        )
        .unwrap()
        .unwrap();
        let mut req = request("payload");
        let mut mac = Hmac::<Sha256>::new_from_slice(b"k").unwrap();
        mac.update(b"payload");
        let good = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        req.headers.push(("x-sig".into(), good));
        assert!(signature_ok(&sig, &req));
        req.body = b"tampered".to_vec();
        assert!(!signature_ok(&sig, &req));
        assert!(check_signature(&json!({"type": "interval"}), Some(&sig)).is_err());
    }
}
