//! What the driver keeps between turns: each thread's agent session id, the
//! account's model list (and thinking levels), and the background refresh
//! that renews that list without a turn.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use ironclaw_orchestrator::model::now_ms;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;

use super::launch::{self, APP_DATA_DIR};
use super::{AntigravityConfig, DEFAULT_MODEL, fold};
use crate::rpc::{self, RpcClient, RpcError};

pub(super) const INIT_TIMEOUT: Duration = Duration::from_secs(90);
pub(super) const SESSION_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the account's model list is trusted before a catalog read asks
/// the agent again, so models the vendor ships reach the picker without a turn.
pub(super) const MODELS_TTL_MS: i64 = 60 * 60 * 1000;
/// Closing the probe session is housekeeping; never wait long for it.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// The `initialize` request every launch sends: no file system, no terminal.
pub(super) fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientInfo": {"name": "ironclaw", "title": "IronClaw", "version": env!("CARGO_PKG_VERSION")},
        "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
    })
}

/// The agent session a thread resumes; `None` on its first turn.
pub(super) async fn read_session_id(path: &Path) -> Option<String> {
    tokio::fs::read_to_string(path)
        .await
        .ok() // silent-ok: no file means the first turn of the thread
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(super) async fn save_session_id(path: &Path, id: &str) {
    let written = async {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(path, id).await
    }
    .await;
    if let Err(error) = written {
        tracing::debug!(%error, "antigravity: session id not saved");
    }
}

/// Stop resuming a session the model can no longer take. The pointer is
/// kept beside the map (`<key>.retired-<ms>`), never deleted: the agent's
/// own conversation stays where it is.
pub(super) async fn retire_session(path: &Path) {
    let retired = path.with_extension(format!("retired-{}", now_ms()));
    if let Err(error) = tokio::fs::rename(path, &retired).await {
        tracing::debug!(%error, "antigravity: broken session not retired");
    }
}

/// Keep the account's model list and thinking levels for the catalog.
pub(super) async fn save_models(path: &Path, options: &Value) {
    let models = fold::model_options(options);
    if models.is_empty() {
        return;
    }
    let body = json!({
        "at": now_ms(),
        "current": fold::current_model(options),
        "models": models,
        "efforts": fold::effort_options(options),
    })
    .to_string();
    // A turn and a refresh may save at once: write a private file and rename
    // it over the list, so a reader never sees a torn or truncated one.
    let tmp = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
    let written = async {
        tokio::fs::write(&tmp, body).await?;
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if let Err(error) = written {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "best-effort cleanup of our own model-list temp file; the write error is traced below"
        )]
        let _ = tokio::fs::remove_file(&tmp).await;
        tracing::debug!(%error, "antigravity: model list not saved");
    }
}

/// Clears the refresh single-flight flag when the refresh task ends.
pub(super) struct RefreshFlag(pub(super) Arc<AtomicBool>);

impl Drop for RefreshFlag {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Whether our model list is missing, unreadable, from before lists carried
/// a timestamp, or older than [`MODELS_TTL_MS`].
pub(super) async fn models_stale(path: &Path) -> bool {
    let Ok(raw) = tokio::fs::read_to_string(path).await else {
        return true; // silent-ok: no list yet is exactly what a refresh fixes
    };
    let at = serde_json::from_str::<Value>(&raw)
        .ok() // silent-ok: a torn list is refreshed like a missing one
        .and_then(|doc| doc["at"].as_i64());
    at.is_none_or(|at| now_ms().saturating_sub(at) > MODELS_TTL_MS)
}

/// The account's models and thinking levels: our own list from the last
/// session, else the one Hermes saved in a shared profile, else the agent's
/// default alias.
pub(super) async fn cached_models(config: &AntigravityConfig) -> (Vec<String>, Vec<String>) {
    let hermes = config
        .profile_dir
        .join(APP_DATA_DIR)
        .join("hermes-models.json");
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m.as_str().or_else(|| m.get("id").and_then(Value::as_str)))
            .map(str::to_string)
            .collect()
    };
    for path in [config.models_file(), hermes] {
        let Ok(raw) = tokio::fs::read_to_string(&path).await else {
            continue; // silent-ok: a missing cache falls through to the next source
        };
        let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
            continue; // silent-ok: a torn cache falls through
        };
        let models = strings(&doc["models"]);
        if !models.is_empty() {
            return (models, strings(&doc["efforts"]));
        }
    }
    (vec![DEFAULT_MODEL.to_string()], Vec::new())
}

/// Ask the agent for the account's models without a turn: `initialize`,
/// `authenticate`, `session/new` (whose config options carry the model
/// list), save the list, `session/close`, then end the process group. The
/// session is bound to no thread and never prompted.
pub(super) async fn refresh_models(config: &AntigravityConfig) -> Result<(), String> {
    ask_agent(config, config.auth_timeout, true).await
}

/// Whether the agent signs in on its own, without a browser: `initialize`
/// and `authenticate` only, no session. Finds a sign-in `agy` keeps in the
/// system keychain (its own first launch makes one), which writes no
/// `acp_token.json`. A signed-out agent waits on a browser nobody opens, so
/// the wait is short.
pub(super) async fn check_sign_in(config: &AntigravityConfig) -> Result<(), String> {
    ask_agent(
        config,
        config.auth_timeout.min(SIGN_IN_CHECK_TIMEOUT),
        false,
    )
    .await
}

/// How long [`check_sign_in`] waits for `authenticate`.
const SIGN_IN_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Record what the agent last said about its sign-in: an `authenticate`
/// that answered marks the account signed in; one that waited for a browser
/// clears the mark. The token file, when the agent writes one, counts too
/// ([`signed_in`]).
pub(super) async fn mark_signed_in(config: &AntigravityConfig, on: bool) {
    let file = config.signed_in_file();
    let done = if on {
        async {
            tokio::fs::create_dir_all(&config.state_dir).await?;
            tokio::fs::write(&file, now_ms().to_string()).await
        }
        .await
    } else {
        match tokio::fs::remove_file(&file).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    };
    if let Err(error) = done {
        tracing::debug!(%error, on, "antigravity: sign-in mark not saved");
    }
}

/// Whether the account is signed in: the agent's token file, or the mark
/// of an `authenticate` that answered ([`mark_signed_in`]).
pub(super) async fn signed_in(config: &AntigravityConfig) -> bool {
    for file in [config.token_file(), config.signed_in_file()] {
        // silent-ok: an unreadable file reads as signed out
        if tokio::fs::try_exists(&file).await.unwrap_or(false) {
            return true;
        }
    }
    false
}

/// One background conversation with the agent, never prompted: start it,
/// `initialize`, `authenticate` within `auth_timeout` (the outcome is
/// recorded with [`mark_signed_in`]), and with `models` also open a session
/// to save the account's model list and close it again. The process group
/// always ends.
async fn ask_agent(
    config: &AntigravityConfig,
    auth_timeout: Duration,
    models: bool,
) -> Result<(), String> {
    launch::prepare_dirs(config)
        .await
        .map_err(|e| format!("dirs: {}", e.kind()))?;
    let mut child = launch::agent_command(config)
        .spawn()
        .map_err(|e| format!("spawn: {}", e.kind()))?;
    let pgid = rpc::group_of(&child).ok_or("exited at spawn")?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        rpc::kill_group(pgid, "antigravity: half-spawned model-list host");
        return Err("stdio unavailable".into());
    };
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "antigravity", "refresh stderr: {line}");
        }
    });
    let (client, inbound) = RpcClient::start(stdin, stdout, "antigravity");
    let asked = async {
        client
            .request("initialize", initialize_params(), INIT_TIMEOUT)
            .await?;
        let auth = client
            .request(
                "authenticate",
                json!({"methodId": config.auth_method}),
                auth_timeout,
            )
            .await;
        match &auth {
            Ok(_) => mark_signed_in(config, true).await,
            // Waiting on a browser sign-in: signed out.
            Err(RpcError::Timeout) if config.uses_browser() => mark_signed_in(config, false).await,
            Err(_) => {}
        }
        auth?;
        if !models {
            return Ok(());
        }
        let cwd = config.work_dir.display().to_string();
        let result = client
            .request(
                "session/new",
                json!({"cwd": cwd, "mcpServers": []}),
                SESSION_TIMEOUT,
            )
            .await?;
        save_models(
            &config.models_file(),
            result.get("configOptions").unwrap_or(&Value::Null),
        )
        .await;
        if let Some(id) = result.get("sessionId").and_then(Value::as_str) {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "closing is housekeeping, the list is already saved"
            )]
            let _ = client
                .request("session/close", json!({"sessionId": id}), CLOSE_TIMEOUT)
                .await;
        }
        Ok::<(), RpcError>(())
    }
    .await;
    drop(client);
    drop(inbound);
    close_host(&mut child, pgid).await;
    asked.map_err(|e| e.label())
}

/// After the answer: let the agent exit on stdin EOF, then make sure the
/// whole group is gone.
pub(super) async fn close_host(child: &mut Child, pgid: i32) {
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        rpc::end_group(child, pgid, rpc::KILL_GRACE).await;
    } else {
        rpc::kill_group(pgid, "antigravity: agent host group");
    }
}
