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
use super::paths::{self, Kind, Managed};
use super::{AntigravityConfig, DEFAULT_MODEL, fold, locked, ownership, signin};
use crate::rpc::{self, RpcClient, RpcError};
use crate::slot::GroupGuard;

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
/// model's (t3code #41699). A turn and a refresh may save at once: the save
/// merges into the list on disk under its lock ([`locked`]), so neither
/// drops the levels the other learned.
pub(super) async fn save_models(path: &Path, options: &Value) {
    let models = fold::model_options(options);
    if models.is_empty() {
        return;
    }
    let efforts = fold::effort_options(options);
    let current = fold::current_model(options);
    let saved = locked::update(path.to_path_buf(), move |existing| {
        let mut per_model = existing
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok()) // silent-ok: a torn list starts its per-model levels over
            .and_then(|doc| doc.get("model_efforts").cloned())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        if let Some(map) = per_model.as_object_mut() {
            if let Some(model) = &current {
                map.insert(model.clone(), json!(efforts));
            }
            map.retain(|m, _| models.contains(m));
        }
        Some(
            json!({
                "at": now_ms(),
                "current": current,
                "models": models,
                "efforts": efforts,
                "model_efforts": per_model,
            })
            .to_string(),
        )
    })
    .await;
    if let Err(error) = saved {
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

/// Ask the agent for the account's models without a turn: `initialize`,
/// `authenticate`, `session/new` (whose config options carry the model
/// list), save the list, `session/close`, then end the process group. The
/// session is bound to no thread and never prompted.
///
/// Passive: it runs only on sign-in evidence that makes `authenticate`
/// non-interactive ([`signin::status`], checked again here), since native
/// agy opens the system browser itself for a signed-out `authenticate`.
/// The session runs in a folder made for it alone, which is what proves
/// afterwards which session files are the probe's ([`remove_probe_session`]).
pub(super) async fn refresh_models(
    config: &AntigravityConfig,
    status: &signin::StatusCache,
) -> Result<(), String> {
    if !signin::status(config, now_ms()).await.may_probe {
        return Err("no sign-in evidence: the agent is not started".into());
    }
    launch::prepare_dirs(config)
        .await
        .map_err(|e| format!("dirs: {}", e.kind()))?;
    let probe_dir = config
        .state_dir
        .join("probes")
        .join(uuid::Uuid::new_v4().to_string());
    launch::private_dir_all(&probe_dir)
        .await
        .map_err(|e| format!("probe dir: {}", e.kind()))?;
    let probe = AntigravityConfig {
        work_dir: probe_dir.clone(),
        ..config.clone()
    };
    let asked = probe_models(&probe, status).await;
    if let Err(error) = tokio::fs::remove_dir_all(&probe_dir).await {
        tracing::debug!(%error, "antigravity: probe folder not removed");
    }
    asked
}

async fn probe_models(
    config: &AntigravityConfig,
    status: &signin::StatusCache,
) -> Result<(), String> {
    let mut child = launch::agent_command(config)
        .spawn()
        .map_err(|e| format!("spawn: {}", e.kind()))?;
    let pgid = rpc::group_of(&child).ok_or("exited at spawn")?;
    // A refresh dropped mid-probe (its runtime shut down) still ends the
    // agent and everything it started.
    let mut group = GroupGuard::new(pgid, "antigravity: dropped model-list host");
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
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
        let attempt = now_ms();
        let authed = client
            .request(
                "authenticate",
                json!({"methodId": config.auth_method}),
                config.auth_timeout,
            )
            .await;
        match &authed {
            Ok(_) => signin::note(&config.state_dir, attempt, true).await,
            Err(error) if fold::sign_in_failed(error, config.uses_browser()).1 => {
                signin::note(&config.state_dir, attempt, false).await
            }
            Err(_) => {}
        }
        status.invalidate();
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
    group.disarm();
    if let Some(id) = probe_session {
        remove_probe_session(&config.profile_dir, &id, &config.work_dir).await;
    }
    asked.map_err(|e| e.label())
}

/// The files agy keeps for the model-list probe's session, which was
/// never prompted and holds no conversation (t3code #39275): without this,
/// every hourly refresh leaves an empty session in the profile.
///
/// The session id is the agent's word, so it alone is never trusted: the
/// files go only when the session's own record names the probe's folder
/// (`probe_cwd`, unique to this probe) as its only workspace (agy 1.5's
/// `trajectory_metadata_blob`, the legacy runtime's `.meta` sidecar), as
/// t3code c8b214158e does. A session id that names any other conversation
/// keeps its files. Every path is checked part by part ([`paths::managed`]):
/// a linked `brain` or `conversations` folder sends nothing outside the
/// profile.
pub(super) async fn remove_probe_session(profile: &Path, id: &str, probe_cwd: &Path) {
    let valid = !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !valid {
        return;
    }
    if !ownership::owned_by(profile, id, probe_cwd).await {
        tracing::debug!("antigravity: probe session not proven ours; its files are kept");
        return;
    }
    let mut entries: Vec<([String; 3], Kind)> = vec![
        ([APP_DATA_DIR.into(), "brain".into(), id.into()], Kind::Dir),
        (
            [
                APP_DATA_DIR.into(),
                "annotations".into(),
                format!("{id}.pbtxt"),
            ],
            Kind::File,
        ),
    ];
    for suffix in ["db", "db-wal", "db-shm", "db-journal", "meta"] {
        entries.push((
            [
                APP_DATA_DIR.into(),
                "conversations".into(),
                format!("{id}.{suffix}"),
            ],
            Kind::File,
        ));
    }
    for (parts, kind) in entries {
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        let Some(Managed::Found(entry)) = paths::managed(profile, &parts, kind).await else {
            continue; // missing, or not a plain entry of ours
        };
        let removed = match kind {
            // `remove_dir_all` does not follow links inside the folder.
            Kind::Dir => tokio::fs::remove_dir_all(&entry).await,
            Kind::File => tokio::fs::remove_file(&entry).await,
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

    /// A session db whose record names `cwd` as its workspace.
    fn session_db(path: &Path, cwd: &Path, id: &str) {
        let blob =
            super::super::ownership::test_blob(&[&format!("file://{}", cwd.display())], Some(id));
        let hex: String = blob.iter().map(|b| format!("{b:02X}")).collect();
        let ok = std::process::Command::new(super::super::sqlite::tool().unwrap())
            .arg(path)
            .arg(format!(
                "CREATE TABLE trajectory_metadata_blob (data blob); \
                 INSERT INTO trajectory_metadata_blob VALUES (X'{hex}');"
            ))
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }

    /// Only the probe's own session goes: one whose record names the
    /// probe's unique folder. A probe answering with an existing
    /// conversation's id, a linked `brain`, or an id that leaves the folder
    /// removes nothing.
    #[tokio::test]
    async fn the_probe_session_leaves_nothing_but_others_stay() {
        if super::super::sqlite::tool().is_none() {
            return; // no sqlite3: ownership cannot be proven, nothing is removed
        }
        let dir = tempfile::tempdir().unwrap();
        let probe_cwd = dir.path().join("probe-cwd");
        let user_cwd = dir.path().join("project");
        std::fs::create_dir_all(&probe_cwd).unwrap();
        std::fs::create_dir_all(&user_cwd).unwrap();
        let acp = dir.path().join(APP_DATA_DIR);
        let conversations = acp.join("conversations");
        std::fs::create_dir_all(&conversations).unwrap();
        std::fs::create_dir_all(acp.join("brain/probe/.system_generated")).unwrap();
        std::fs::create_dir_all(acp.join("brain/kept")).unwrap();
        session_db(&conversations.join("probe.db"), &probe_cwd, "probe");
        session_db(&conversations.join("kept.db"), &user_cwd, "kept");
        std::fs::write(conversations.join("probe.db-wal"), "x").unwrap();
        remove_probe_session(dir.path(), "probe", &probe_cwd).await;
        assert!(!acp.join("brain/probe").exists());
        assert!(!conversations.join("probe.db").exists());
        assert!(!conversations.join("probe.db-wal").exists());
        // The agent's id names the user's conversation: kept.
        remove_probe_session(dir.path(), "kept", &probe_cwd).await;
        assert!(conversations.join("kept.db").exists());
        assert!(acp.join("brain/kept").exists());
        remove_probe_session(dir.path(), "../kept", &probe_cwd).await;
        assert!(acp.join("brain/kept").exists());

        // A linked `brain` folder is never followed out of the profile.
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(outside.path().join("probe2")).unwrap();
        std::fs::remove_dir_all(acp.join("brain")).unwrap();
        std::os::unix::fs::symlink(outside.path(), acp.join("brain")).unwrap();
        session_db(&conversations.join("probe2.db"), &probe_cwd, "probe2");
        remove_probe_session(dir.path(), "probe2", &probe_cwd).await;
        assert!(outside.path().join("probe2").exists());
        assert!(!conversations.join("probe2.db").exists());
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

    /// A turn and a refresh saving at once both keep the levels they
    /// learned: neither save drops the other's model.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn overlapping_saves_keep_every_models_levels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("models.json");
        let names: Vec<String> = (0..12).map(|n| format!("gemini-{n}")).collect();
        let saves: Vec<_> = names
            .iter()
            .map(|current| {
                let (path, names, current) = (path.clone(), names.clone(), current.clone());
                tokio::spawn(async move {
                    let options = json!([
                        {"id": "model", "type": "select", "currentValue": current,
                         "options": names.iter().map(|m| json!({"value": m})).collect::<Vec<_>>()},
                        {"id": "thought_level", "category": "thought_level", "type": "select",
                         "currentValue": "high",
                         "options": [{"value": format!("{current}-level")}]},
                    ]);
                    save_models(&path, &options).await;
                })
            })
            .collect();
        for save in saves {
            save.await.unwrap();
        }
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for name in &names {
            assert_eq!(
                doc["model_efforts"][name],
                json!([format!("{name}-level")]),
                "{name} lost its levels: {doc}"
            );
        }
    }

    /// A refresh dropped while the agent hangs (its runtime shut down)
    /// still ends the agent's whole process group.
    #[test]
    fn a_dropped_refresh_ends_the_agents_group() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let agy = dir.path().join("agy");
        std::fs::write(
            &agy,
            format!(
                "#!/bin/sh\nsleep 30 &\nprintf '%s' $! > '{}'\nwait\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).unwrap();
        let profile = dir.path().join("profile");
        std::fs::create_dir_all(profile.join(APP_DATA_DIR)).unwrap();
        std::fs::write(
            profile.join(APP_DATA_DIR).join("acp_token.json"),
            r#"{"refresh_token":"r"}"#,
        )
        .unwrap();
        let config = AntigravityConfig {
            runtime_id: "antigravity".into(),
            display_name: "Antigravity".into(),
            launch: launch::AgentLaunch::Cli { agy },
            profile_dir: profile,
            user_gemini_dir: None,
            auth_method: "oauth-personal".into(),
            auth_timeout: Duration::from_secs(5),
            work_dir: dir.path().join("work"),
            state_dir: dir.path().join("state"),
            background_hold: Duration::from_secs(30),
            prompt_silence: Duration::from_secs(120),
            keyring: None,
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.spawn(async move {
            let status = signin::StatusCache::default();
            refresh_models(&config, &status).await.unwrap_or_default();
        });
        let started = std::time::Instant::now();
        while !pid_file.exists() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
        runtime.shutdown_timeout(Duration::from_secs(1));
        let alive = || nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
        let started = std::time::Instant::now();
        while alive() && started.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(), "the probe's agent group outlived its refresh");
    }
}
