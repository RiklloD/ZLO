//! What the driver keeps between turns: each thread's agent session id, the
//! account's model list (and thinking levels), and the background refresh
//! that renews that list without a turn.

use std::collections::BTreeMap;
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

/// Keep the account's model list and thinking levels for the catalog. Each
/// model's own levels are kept from when it was the session's model, so a
/// model keeps its levels across restarts instead of showing another
/// model's (t3code #41699).
pub(super) async fn save_models(path: &Path, options: &Value) {
    let models = fold::model_options(options);
    if models.is_empty() {
        return;
    }
    let efforts = fold::effort_options(options);
    let current = fold::current_model(options);
    let mut per_model = match tokio::fs::read_to_string(path).await {
        Ok(raw) => serde_json::from_str::<Value>(&raw)
            .ok() // silent-ok: a torn list starts its per-model levels over
            .and_then(|doc| doc.get("model_efforts").cloned())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({})),
        Err(_) => json!({}), // silent-ok: no list yet
    };
    if let (Some(model), Some(map)) = (&current, per_model.as_object_mut()) {
        map.insert(model.clone(), json!(efforts));
        map.retain(|m, _| models.contains(m));
    }
    let body = json!({
        "at": now_ms(),
        "current": current,
        "models": models,
        "efforts": efforts,
        "model_efforts": per_model,
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

/// The account's models, thinking levels, and each model's own levels.
pub(super) struct CachedModels {
    pub(super) models: Vec<String>,
    pub(super) efforts: Vec<String>,
    pub(super) model_efforts: BTreeMap<String, Vec<String>>,
}

/// The account's models and thinking levels: our own list from the last
/// session, else the one Hermes saved in a shared profile, else the agent's
/// default alias.
pub(super) async fn cached_models(config: &AntigravityConfig) -> CachedModels {
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
            let model_efforts = doc["model_efforts"]
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(model, _)| models.contains(model))
                .map(|(model, levels)| (model.clone(), strings(levels)))
                .collect();
            return CachedModels {
                efforts: strings(&doc["efforts"]),
                models,
                model_efforts,
            };
        }
    }
    CachedModels {
        models: vec![DEFAULT_MODEL.to_string()],
        efforts: Vec::new(),
        model_efforts: BTreeMap::new(),
    }
}

/// Where the driver notes that `authenticate` last succeeded. agy 1.5
/// keeps a Google sign-in in the OS keyring and writes no token file, so a
/// successful sign-in is the only local evidence of one.
fn signed_in_file(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("signed-in")
}

/// Note how the last `authenticate` went: signed in, or signed out.
pub(super) async fn note_sign_in(state_dir: &Path, signed_in: bool) {
    let path = signed_in_file(state_dir);
    let noted = if signed_in {
        match tokio::fs::create_dir_all(state_dir).await {
            Ok(()) => tokio::fs::write(&path, now_ms().to_string()).await,
            Err(error) => Err(error),
        }
    } else {
        match tokio::fs::remove_file(&path).await {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    };
    if let Err(error) = noted {
        tracing::debug!(%error, "antigravity: sign-in state not noted");
    }
}

/// Whether the account reads as signed in: a token file in the profile
/// (the legacy runtime's `acp_token.json`, the CLI's file sign-in
/// `antigravity-oauth-token`), or a successful `authenticate` noted since
/// the last failed one.
pub(super) async fn signed_in(config: &AntigravityConfig) -> bool {
    let acp = config.profile_dir.join(APP_DATA_DIR);
    for path in [
        acp.join("acp_token.json"),
        acp.join("antigravity-oauth-token"),
        signed_in_file(&config.state_dir),
    ] {
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            // silent-ok: an unreadable file reads as signed out
            return true;
        }
    }
    false
}

/// Ask the agent for the account's models without a turn: `initialize`,
/// `authenticate`, `session/new` (whose config options carry the model
/// list), save the list, `session/close`, then end the process group. The
/// session is bound to no thread and never prompted.
pub(super) async fn refresh_models(config: &AntigravityConfig) -> Result<(), String> {
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
    let mut probe_session = None;
    let asked = async {
        client
            .request("initialize", initialize_params(), INIT_TIMEOUT)
            .await?;
        let authed = client
            .request(
                "authenticate",
                json!({"methodId": config.auth_method}),
                config.auth_timeout,
            )
            .await;
        match &authed {
            Ok(_) => note_sign_in(&config.state_dir, true).await,
            Err(error) if fold::sign_in_failed(error, config.uses_browser()).1 => {
                note_sign_in(&config.state_dir, false).await
            }
            Err(_) => {}
        }
        authed?;
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
            probe_session = Some(id.to_string());
        }
        Ok::<(), RpcError>(())
    }
    .await;
    drop(client);
    drop(inbound);
    close_host(&mut child, pgid).await;
    if let Some(id) = probe_session {
        remove_probe_session(&config.profile_dir, &id).await;
    }
    asked.map_err(|e| e.label())
}

/// The files agy 1.5 keeps for the model-list probe's session, which was
/// never prompted and holds no conversation (t3code #39275): without this,
/// every hourly refresh leaves an empty session in the profile. Only that
/// session's own entries go, never through a link.
pub(super) async fn remove_probe_session(profile: &Path, id: &str) {
    let valid = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !valid {
        return;
    }
    let acp = profile.join(APP_DATA_DIR);
    let conversations = acp.join("conversations");
    let mut entries = vec![acp.join("brain").join(id)];
    for suffix in ["db", "db-wal", "db-shm"] {
        entries.push(conversations.join(format!("{id}.{suffix}")));
    }
    for entry in entries {
        let removed = match tokio::fs::symlink_metadata(&entry).await {
            // `remove_dir_all` does not follow links inside the folder.
            Ok(meta) if meta.file_type().is_dir() => tokio::fs::remove_dir_all(&entry).await,
            Ok(meta) if meta.file_type().is_file() => tokio::fs::remove_file(&entry).await,
            Ok(_) => Ok(()), // a link is not ours to remove
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        };
        if let Err(error) = removed {
            tracing::debug!(%error, "antigravity: probe session file not removed");
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_probe_session_leaves_nothing_but_others_stay() {
        let dir = tempfile::tempdir().unwrap();
        let acp = dir.path().join(APP_DATA_DIR);
        std::fs::create_dir_all(acp.join("conversations")).unwrap();
        std::fs::create_dir_all(acp.join("brain/probe/.system_generated")).unwrap();
        std::fs::create_dir_all(acp.join("brain/kept")).unwrap();
        for name in ["probe.db", "probe.db-wal", "kept.db"] {
            std::fs::write(acp.join("conversations").join(name), "x").unwrap();
        }
        remove_probe_session(dir.path(), "probe").await;
        assert!(!acp.join("brain/probe").exists());
        assert!(!acp.join("conversations/probe.db").exists());
        assert!(!acp.join("conversations/probe.db-wal").exists());
        assert!(acp.join("conversations/kept.db").exists());
        assert!(acp.join("brain/kept").exists());
        remove_probe_session(dir.path(), "../kept").await;
        assert!(acp.join("brain/kept").exists());
    }

    #[tokio::test]
    async fn each_model_keeps_its_own_thinking_levels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        let options = |current: &str, levels: &[&str]| {
            json!([
                {"id": "model", "type": "select", "currentValue": current,
                 "options": [{"value": "gemini-a"}, {"value": "gemini-b"}]},
                {"id": "thought_level", "category": "thought_level", "type": "select",
                 "currentValue": levels[0],
                 "options": levels.iter().map(|l| json!({"value": l})).collect::<Vec<_>>()},
            ])
        };
        save_models(&path, &options("gemini-a", &["low", "high"])).await;
        save_models(&path, &options("gemini-b", &["minimal", "xhigh"])).await;
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(doc["model_efforts"]["gemini-a"], json!(["low", "high"]));
        assert_eq!(
            doc["model_efforts"]["gemini-b"],
            json!(["minimal", "xhigh"])
        );
        assert_eq!(doc["efforts"], json!(["minimal", "xhigh"]));
    }
}
