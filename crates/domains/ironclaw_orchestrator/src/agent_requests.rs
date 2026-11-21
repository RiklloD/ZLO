//! Requests an agent raises for the person, answered from any device
//! connected to this environment: a secret (`t3_request_secret`) and a
//! machine to link (`t3_environment_link`).
//!
//! A secret never enters the conversation or this table. Answered, it goes
//! either into the host's [`SecretVault`] under a one-use `secretRef` that a
//! tool spends (the agent never sees the value), or into an owner-only file
//! the agent's commands read, deleted after 24 hours. A secret reference
//! (`op://vault/item/field`) is read once, when the person answers.
//!
//! Rows are kept: the request, its status and the delivery (ref or file
//! path), never the value.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_product_contracts::environment_access::PeerLinkAccess;
use ironclaw_product_contracts::peer::{
    AgentRequestAnswer, AgentRequestError, AgentRequestKind, AgentRequestList, AgentRequestService,
    AgentRequestStatus, AgentRequestView, LinkEnvironmentRequest, SecretDelivery,
    SecretReferenceReader,
};
use libsql::Value as Sql;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::engine::{CallContext, Engine, Result, lock_poisoned};
use crate::error::OrchestratorError;
use crate::inbox::AwaitingKind;
use crate::model::*;
use crate::store::{Cols, Store, s};

pub(crate) const AGENT_REQUESTS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_agent_requests (
  id TEXT PRIMARY KEY,
  thread_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  status TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  answered_at INTEGER,
  result TEXT,
  file_path TEXT,
  file_removed_at INTEGER
);
CREATE INDEX IF NOT EXISTS orch_agent_requests_status ON orch_agent_requests(status, expires_at);
"#;

/// Default wait for an answer.
const DEFAULT_TIMEOUT_MS: i64 = 10 * 60_000;
const MAX_TIMEOUT_MS: i64 = 60 * 60_000;
/// How long a delivered secret file stays readable.
const SECRET_FILE_TTL_MS: i64 = 24 * 60 * 60_000;
/// Largest secret accepted.
const MAX_SECRET_BYTES: usize = 64 * 1024;
/// Prefix of a one-use secret reference.
pub const SECRET_REF_PREFIX: &str = "secretref:";

/// Where answered secrets are kept until a tool spends them. Implemented by
/// the host over its encrypted operator secret store.
#[async_trait]
pub trait SecretVault: Send + Sync {
    async fn put(&self, handle: &str, value: SecretString) -> std::result::Result<(), String>;
    /// Read and delete: a `secretRef` is spent once.
    async fn take(&self, handle: &str) -> std::result::Result<Option<SecretString>, String>;
    /// Reads secret references (1Password) on this machine, when available.
    fn reference_reader(&self) -> Option<Arc<dyn SecretReferenceReader>>;
    /// Owner-only folder for `delivery: file` secrets.
    fn files_dir(&self) -> PathBuf;
}

#[derive(Debug, Clone, PartialEq)]
struct RequestRow {
    id: String,
    thread_id: ThreadId,
    kind: AgentRequestKind,
    status: AgentRequestStatus,
    created_at: Millis,
    expires_at: Millis,
    result: Option<Value>,
}

fn request_row(r: &libsql::Row) -> Result<RequestRow> {
    let mut c = Cols::new(r);
    let id = c.text()?;
    let thread_id = ThreadId::new(c.text()?)?;
    let kind: AgentRequestKind = serde_json::from_str(&c.text()?)?;
    let raw_status = c.text()?;
    let status = AgentRequestStatus::parse(&raw_status).ok_or_else(|| {
        OrchestratorError::Corrupt(format!("agent request status {raw_status:?}"))
    })?;
    let created_at = c.int()?;
    let expires_at = c.int()?;
    let result = c
        .otext()?
        .map(|raw| serde_json::from_str::<Value>(&raw))
        .transpose()?;
    Ok(RequestRow {
        id,
        thread_id,
        kind,
        status,
        created_at,
        expires_at,
        result,
    })
}

const REQUEST_COLS: &str = "id, thread_id, kind, status, created_at, expires_at, result";

impl Store {
    async fn insert_agent_request(&self, row: &RequestRow) -> Result<()> {
        self.exec(
            "INSERT INTO orch_agent_requests (id, thread_id, kind, status, created_at, expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            vec![
                s(&row.id),
                s(row.thread_id.as_str()),
                s(serde_json::to_string(&row.kind)?),
                s(row.status.as_str()),
                Sql::Integer(row.created_at),
                Sql::Integer(row.expires_at),
            ],
        )
        .await?;
        Ok(())
    }

    async fn agent_request(&self, id: &str) -> Result<Option<RequestRow>> {
        self.row(
            &format!("SELECT {REQUEST_COLS} FROM orch_agent_requests WHERE id = ?1"),
            vec![s(id)],
            request_row,
        )
        .await
    }

    async fn pending_agent_requests(&self) -> Result<Vec<RequestRow>> {
        self.rows(
            &format!(
                "SELECT {REQUEST_COLS} FROM orch_agent_requests WHERE status = 'pending' \
                 ORDER BY created_at LIMIT 200"
            ),
            vec![],
            request_row,
        )
        .await
    }

    /// Close a pending request; `false` when another writer closed it first.
    async fn close_agent_request(
        &self,
        id: &str,
        status: AgentRequestStatus,
        result: Option<&Value>,
        file_path: Option<&str>,
    ) -> Result<bool> {
        let n = self
            .exec(
                "UPDATE orch_agent_requests SET status = ?2, answered_at = ?3, result = ?4, \
                 file_path = ?5 WHERE id = ?1 AND status = 'pending'",
                vec![
                    s(id),
                    s(status.as_str()),
                    Sql::Integer(now_ms()),
                    match result {
                        Some(v) => s(serde_json::to_string(v)?),
                        None => Sql::Null,
                    },
                    file_path.map_or(Sql::Null, s),
                ],
            )
            .await?;
        Ok(n > 0)
    }

    async fn expired_agent_requests(&self, now: Millis) -> Result<Vec<String>> {
        self.rows(
            "SELECT id FROM orch_agent_requests WHERE status = 'pending' AND expires_at <= ?1",
            vec![Sql::Integer(now)],
            |r| Cols::new(r).text(),
        )
        .await
    }

    async fn stale_secret_files(&self, before: Millis) -> Result<Vec<(String, String)>> {
        self.rows(
            "SELECT id, file_path FROM orch_agent_requests WHERE file_path IS NOT NULL \
             AND file_removed_at IS NULL AND answered_at <= ?1",
            vec![Sql::Integer(before)],
            |r| {
                let mut c = Cols::new(r);
                Ok((c.text()?, c.text()?))
            },
        )
        .await
    }

    async fn mark_secret_file_removed(&self, id: &str) -> Result<()> {
        self.exec(
            "UPDATE orch_agent_requests SET file_removed_at = ?2 WHERE id = ?1",
            vec![s(id), Sql::Integer(now_ms())],
        )
        .await?;
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SecretArgs {
    label: String,
    reason: String,
    url: Option<String>,
    placeholder: Option<String>,
    delivery: Option<String>,
    /// Pasted into the chat already: stored without a card.
    value: Option<String>,
    timeout_ms: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct LinkArgs {
    environment_id: Option<String>,
    url: Option<String>,
    hint: Option<String>,
    access: Option<String>,
    timeout_ms: Option<i64>,
}

fn vault_handle(request_id: &str) -> String {
    format!("agent_secret_{request_id}")
}

fn timeout_of(raw: Option<i64>) -> i64 {
    raw.unwrap_or(DEFAULT_TIMEOUT_MS)
        .clamp(1_000, MAX_TIMEOUT_MS)
}

fn view_of(row: RequestRow, title: Option<String>) -> AgentRequestView {
    AgentRequestView {
        request_id: row.id,
        thread_id: row.thread_id.as_str().to_string(),
        thread_title: title,
        status: row.status,
        created_at: row.created_at,
        expires_at: row.expires_at,
        kind: row.kind,
    }
}

impl Engine {
    /// Install where answered secrets go (first call wins).
    pub fn set_secret_vault(&self, vault: Arc<dyn SecretVault>) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "first call wins: the host wires one vault per process"
        )]
        let _ = self.secret_vault.set(vault);
    }

    fn vault(&self) -> Result<&Arc<dyn SecretVault>> {
        self.secret_vault.get().ok_or_else(|| {
            OrchestratorError::request(
                "secrets_unavailable",
                "Secret requests are not available on this machine.",
            )
        })
    }

    /// Spend a one-use `secretRef` from `t3_request_secret`.
    pub async fn take_secret_ref(&self, secret_ref: &str) -> Result<SecretString> {
        let id = secret_ref
            .trim()
            .strip_prefix(SECRET_REF_PREFIX)
            .ok_or_else(|| {
                OrchestratorError::invalid("secretRef must come from t3_request_secret")
            })?;
        self.vault()?
            .take(&vault_handle(id))
            .await
            .map_err(|e| OrchestratorError::request("secrets_unavailable", e))?
            .ok_or_else(|| {
                OrchestratorError::request(
                    "secret_ref_spent",
                    "That secretRef was already used or never saved; ask for the secret again.",
                )
            })
    }

    /// `{secret_ref}` in a schedule signature becomes its `secret`.
    pub(crate) async fn resolve_signature_secret_ref(&self, signature: &mut Value) -> Result<()> {
        let Some(map) = signature.as_object_mut() else {
            return Ok(());
        };
        let Some(raw) = map.remove("secret_ref").or_else(|| map.remove("secretRef")) else {
            return Ok(());
        };
        let reference = raw
            .as_str()
            .ok_or_else(|| OrchestratorError::invalid("secret_ref must be a string"))?;
        let secret = self.take_secret_ref(reference).await?;
        map.insert("secret".into(), json!(secret.expose_secret()));
        Ok(())
    }

    fn waiter(&self, id: &str) -> Result<Arc<tokio::sync::Notify>> {
        let mut map = self.request_waiters.lock().map_err(lock_poisoned)?;
        Ok(Arc::clone(
            map.entry(id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Notify::new())),
        ))
    }

    fn wake(&self, id: &str) {
        if let Ok(mut map) = self.request_waiters.lock()
            && let Some(notify) = map.remove(id)
        {
            notify.notify_waiters();
        }
    }

    /// Wait until the request closes or `until`; then answer the tool.
    async fn await_request(&self, id: &str, thread: &ThreadId, until: Millis) -> Result<Value> {
        loop {
            let notify = self.waiter(id)?;
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let row = self
                .store
                .agent_request(id)
                .await?
                .ok_or_else(|| OrchestratorError::Corrupt("agent request vanished".into()))?;
            if row.status != AgentRequestStatus::Pending {
                self.store.clear_awaiting(thread).await?;
                return Ok(self.request_result(&row));
            }
            let left = until - now_ms();
            if left <= 0 {
                self.store
                    .close_agent_request(id, AgentRequestStatus::TimedOut, None, None)
                    .await?;
                self.store.clear_awaiting(thread).await?;
                return Ok(json!({"status": "timed_out", "requestId": id,
                    "message": "Nobody answered in time. Ask again only if the work still needs it."}));
            }
            // Re-read at least every minute: an answer from another
            // process (or a missed wake) is still seen.
            let wait = Duration::from_millis(left.clamp(1, 60_000) as u64);
            if tokio::time::timeout(wait, notified).await.is_err() {
                continue;
            }
        }
    }

    fn request_result(&self, row: &RequestRow) -> Value {
        match row.status {
            AgentRequestStatus::Answered => {
                let mut out = row.result.clone().unwrap_or_else(|| json!({}));
                out["status"] = json!(match row.kind {
                    AgentRequestKind::Secret { .. } => "saved",
                    AgentRequestKind::EnvironmentLink { .. } => "linked",
                });
                out["requestId"] = json!(row.id);
                out
            }
            other => json!({"status": other.as_str(), "requestId": row.id}),
        }
    }

    /// `t3_request_secret`.
    pub(crate) async fn request_secret_tool(
        &self,
        ctx: &CallContext,
        input: &Value,
    ) -> Result<Value> {
        let a: SecretArgs = serde_json::from_value(input.clone())
            .map_err(|e| OrchestratorError::invalid(format!("invalid arguments: {e}")))?;
        let label = a.label.trim();
        let reason = a.reason.trim();
        if label.is_empty() || reason.is_empty() {
            return Err(OrchestratorError::invalid("label and reason are required"));
        }
        if let Some(url) = &a.url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            return Err(OrchestratorError::invalid("url must be an http(s) URL"));
        }
        let delivery = match a.delivery.as_deref() {
            None => SecretDelivery::SecretRef,
            Some(raw) => SecretDelivery::parse(raw)
                .ok_or_else(|| OrchestratorError::invalid("delivery must be secretRef or file"))?,
        };
        self.vault()?;
        let now = now_ms();
        let row = RequestRow {
            id: format!("sec_{}", &uuid::Uuid::new_v4().simple().to_string()[..20]),
            thread_id: ctx.thread_id.clone(),
            kind: AgentRequestKind::Secret {
                label: label.to_string(),
                reason: reason.to_string(),
                url: a.url.clone(),
                placeholder: a.placeholder.clone(),
                delivery,
            },
            status: AgentRequestStatus::Pending,
            created_at: now,
            expires_at: now + timeout_of(a.timeout_ms),
            result: None,
        };
        self.store.insert_agent_request(&row).await?;
        if let Some(value) = a.value.filter(|v| !v.is_empty()) {
            // Already in the chat: store it without a card.
            let view = self
                .answer(&row.id, AgentRequestAnswer::Secret { value })
                .await
                .map_err(|e| OrchestratorError::invalid(e.to_string()))?;
            let row = self
                .store
                .agent_request(&view.request_id)
                .await?
                .ok_or_else(|| OrchestratorError::Corrupt("agent request vanished".into()))?;
            return Ok(self.request_result(&row));
        }
        self.store
            .set_awaiting(&ctx.thread_id, None, AwaitingKind::Question, now)
            .await?;
        self.await_request(&row.id, &ctx.thread_id, row.expires_at)
            .await
    }

    /// `t3_environment_link`.
    pub(crate) async fn environment_link_tool(
        &self,
        ctx: &CallContext,
        input: &Value,
    ) -> Result<Value> {
        let a: LinkArgs = serde_json::from_value(input.clone())
            .map_err(|e| OrchestratorError::invalid(format!("invalid arguments: {e}")))?;
        self.peers()?;
        let access = match a.access.as_deref() {
            None => None,
            Some(raw) => Some(PeerLinkAccess::parse(raw).ok_or_else(|| {
                OrchestratorError::invalid("access must be read_only, operate or full")
            })?),
        };
        if let Some(url) = &a.url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            return Err(OrchestratorError::invalid(
                "url must be an address the person gave: https, Tailscale, LAN or loopback",
            ));
        }
        let now = now_ms();
        let row = RequestRow {
            id: format!("lnk_{}", &uuid::Uuid::new_v4().simple().to_string()[..20]),
            thread_id: ctx.thread_id.clone(),
            kind: AgentRequestKind::EnvironmentLink {
                hint: a.hint.filter(|h| !h.trim().is_empty()),
                environment_id: a.environment_id.filter(|h| !h.trim().is_empty()),
                url: a.url,
                access,
            },
            status: AgentRequestStatus::Pending,
            created_at: now,
            expires_at: now + timeout_of(a.timeout_ms),
            result: None,
        };
        self.store.insert_agent_request(&row).await?;
        self.store
            .set_awaiting(&ctx.thread_id, None, AwaitingKind::Question, now)
            .await?;
        self.await_request(&row.id, &ctx.thread_id, row.expires_at)
            .await
    }

    async fn answer(
        &self,
        id: &str,
        answer: AgentRequestAnswer,
    ) -> std::result::Result<AgentRequestView, AgentRequestError> {
        let row = self
            .store
            .agent_request(id)
            .await
            .map_err(|_| AgentRequestError::Unavailable)?
            .ok_or(AgentRequestError::NotFound)?;
        if row.status != AgentRequestStatus::Pending {
            return Err(AgentRequestError::Closed);
        }
        let (status, result, file) = match (&row.kind, answer) {
            (_, AgentRequestAnswer::Decline) => (AgentRequestStatus::Declined, None, None),
            (AgentRequestKind::Secret { delivery, .. }, AgentRequestAnswer::Secret { value }) => {
                let (result, file) = self.deliver_secret(&row.id, *delivery, value).await?;
                (AgentRequestStatus::Answered, Some(result), file)
            }
            (
                AgentRequestKind::EnvironmentLink { .. },
                AgentRequestAnswer::Link {
                    environment_id,
                    url,
                    pairing_code,
                    access,
                },
            ) => {
                let result = self
                    .answer_link(environment_id, url, pairing_code, access)
                    .await?;
                (AgentRequestStatus::Answered, Some(result), None)
            }
            _ => {
                return Err(AgentRequestError::Invalid(
                    "That answer does not fit this request.".into(),
                ));
            }
        };
        let closed = self
            .store
            .close_agent_request(&row.id, status, result.as_ref(), file.as_deref())
            .await
            .map_err(|_| AgentRequestError::Unavailable)?;
        if !closed {
            return Err(AgentRequestError::Closed);
        }
        self.wake(&row.id);
        let row = self
            .store
            .agent_request(id)
            .await
            .map_err(|_| AgentRequestError::Unavailable)?
            .ok_or(AgentRequestError::NotFound)?;
        Ok(view_of(row, None))
    }

    async fn deliver_secret(
        &self,
        id: &str,
        delivery: SecretDelivery,
        value: String,
    ) -> std::result::Result<(Value, Option<String>), AgentRequestError> {
        let vault = self.vault().map_err(|_| AgentRequestError::Unavailable)?;
        let trimmed = value.trim();
        let secret = match vault.reference_reader() {
            Some(reader) if reader.is_reference(trimmed) => reader
                .read(trimmed)
                .await
                .map_err(AgentRequestError::Invalid)?,
            _ => SecretString::from(value),
        };
        if secret.expose_secret().is_empty() {
            return Err(AgentRequestError::Invalid("The secret is empty.".into()));
        }
        if secret.expose_secret().len() > MAX_SECRET_BYTES {
            return Err(AgentRequestError::Invalid(
                "The secret is too large.".into(),
            ));
        }
        match delivery {
            SecretDelivery::SecretRef => {
                vault
                    .put(&vault_handle(id), secret)
                    .await
                    .map_err(|_| AgentRequestError::Unavailable)?;
                Ok((
                    json!({"secretRef": format!("{SECRET_REF_PREFIX}{id}")}),
                    None,
                ))
            }
            SecretDelivery::File => {
                let dir = vault.files_dir();
                let path = write_owner_only(&dir, id, secret.expose_secret().as_bytes())
                    .await
                    .map_err(|error| {
                        tracing::debug!(%error, "secret file write failed");
                        AgentRequestError::Unavailable
                    })?;
                let shown = path.display().to_string();
                Ok((json!({"file": shown}), Some(shown)))
            }
        }
    }

    async fn answer_link(
        &self,
        environment_id: Option<String>,
        url: Option<String>,
        pairing_code: Option<String>,
        access: PeerLinkAccess,
    ) -> std::result::Result<Value, AgentRequestError> {
        let peers = self.peers().map_err(|_| AgentRequestError::Unavailable)?;
        let link = match (environment_id, url, pairing_code) {
            (_, Some(url), Some(code)) => peers
                .link(LinkEnvironmentRequest {
                    url,
                    pairing_code: code,
                    access,
                })
                .await
                .map_err(|e| AgentRequestError::Invalid(e.to_string()))?,
            (Some(id), _, _) => peers
                .links()
                .await
                .map_err(|_| AgentRequestError::Unavailable)?
                .into_iter()
                .find(|l| l.environment_id == id)
                .ok_or_else(|| {
                    AgentRequestError::Invalid(
                        "That machine is not linked yet: paste a pairing code from it.".into(),
                    )
                })?,
            _ => {
                return Err(AgentRequestError::Invalid(
                    "Pick a linked machine, or give its address and a pairing code from it.".into(),
                ));
            }
        };
        Ok(json!({
            "environmentId": link.environment_id, "label": link.label,
            "access": link.access.as_str(),
        }))
    }

    /// Tick step: time out unanswered requests; remove day-old secret files.
    pub(crate) async fn expire_agent_requests(&self) -> Result<()> {
        let now = now_ms();
        for id in self.store.expired_agent_requests(now).await? {
            if self
                .store
                .close_agent_request(&id, AgentRequestStatus::TimedOut, None, None)
                .await?
            {
                self.wake(&id);
            }
        }
        for (id, path) in self
            .store
            .stale_secret_files(now - SECRET_FILE_TTL_MS)
            .await?
        {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::debug!(%error, "secret file removal failed; retrying next tick");
                    continue;
                }
            }
            self.store.mark_secret_file_removed(&id).await?;
        }
        Ok(())
    }
}

/// Write `bytes` to `<dir>/<id>` readable only by this user.
async fn write_owner_only(
    dir: &std::path::Path,
    id: &str,
    bytes: &[u8],
) -> std::io::Result<PathBuf> {
    let dir = dir.to_path_buf();
    let id = id.to_string();
    let bytes = bytes.to_vec();
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let path = dir.join(&id);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(path)
    })
    .await
    .map_err(std::io::Error::other)?
}

#[async_trait]
impl AgentRequestService for Engine {
    async fn pending_requests(&self) -> std::result::Result<AgentRequestList, AgentRequestError> {
        let rows = self
            .store
            .pending_agent_requests()
            .await
            .map_err(|_| AgentRequestError::Unavailable)?;
        let mut requests = Vec::new();
        for row in rows {
            let title = self
                .store
                .thread(&row.thread_id)
                .await
                .ok()
                .flatten()
                .and_then(|t| t.title);
            requests.push(view_of(row, title));
        }
        Ok(AgentRequestList { requests })
    }

    async fn answer_request(
        &self,
        request_id: &str,
        answer: AgentRequestAnswer,
    ) -> std::result::Result<AgentRequestView, AgentRequestError> {
        self.answer(request_id, answer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn secret_files_are_owner_only_and_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("zlo-secret-test-{}", uuid::Uuid::new_v4()));
        let path = write_owner_only(&dir, "sec_1", b"hunter2").await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hunter2");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(write_owner_only(&dir, "sec_1", b"other").await.is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn timeouts_are_bounded() {
        assert_eq!(timeout_of(None), DEFAULT_TIMEOUT_MS);
        assert_eq!(timeout_of(Some(5)), 1_000);
        assert_eq!(timeout_of(Some(i64::MAX)), MAX_TIMEOUT_MS);
    }
}
