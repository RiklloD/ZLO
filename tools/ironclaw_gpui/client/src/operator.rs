//! Typed calls for the operator surface: setup, config keys, readiness
//! status, diagnostics, service lifecycle, logs, and the run inspector.
//!
//! Every route here is operator-gated by the gateway. Request and response
//! bodies are the contract types, except where noted below.

use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use ironclaw_product_contracts::inspector::{
    BoundedDiagnosticText, DiagnosticActivityEvent, ModelCallDiagnostic, PromptComponentDiagnostic,
    SessionDiagnosticStats, ToolExecutionDiagnostic,
};
use ironclaw_product_contracts::product_wire::{
    RebornLogQueryResponse, RebornOperatorCommandPlaneResponse, RebornOperatorConfigGetResponse,
    RebornOperatorConfigListResponse, RebornOperatorConfigSetRequest,
    RebornOperatorConfigValidateRequest, RebornOperatorConfigValidateResponse,
    RebornOperatorLogsQuery, RebornOperatorServiceLifecycleAction,
    RebornOperatorServiceLifecycleRequest, RebornOperatorSetupResponse,
};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::api::{Client, V2, seg};
use crate::backend::Backend;
use crate::error::ClientError;

/// Inspector contract types the app renders directly.
pub use ironclaw_product_contracts::inspector;

/// Fields for `POST /operator/setup`. The contract's `RebornOperatorSetupRequest`
/// is deserialize-only (it carries secrets), so the body is built as JSON.
/// Empty fields are left out; the gateway rejects combinations it cannot use.
#[derive(Debug, Clone, Default)]
pub struct SetupInput {
    pub provider_id: String,
    pub adapter: String,
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

impl SetupInput {
    fn body(&self) -> serde_json::Value {
        let mut body = serde_json::Map::new();
        for (key, value) in [
            ("provider_id", &self.provider_id),
            ("adapter", &self.adapter),
            ("base_url", &self.base_url),
            ("model", &self.model),
            ("api_key", &self.api_key),
        ] {
            let value = value.trim();
            if !value.is_empty() {
                body.insert(key.to_string(), value.into());
            }
        }
        serde_json::Value::Object(body)
    }
}

// --- inspector decoding ---
//
// The gateway builds `DiagnosticSnapshot` / `PromptDiagnostic` server-side and
// they are serialize-only. Their leaves (model calls, tools, activity, stats,
// prompt components, bounded text) are deserializable contract types, so the
// client decodes only the fields it renders and keeps the leaves as-is.

/// What the inspector shows for one run.
#[derive(Debug, Clone, Deserialize)]
pub struct InspectorRun {
    #[serde(default)]
    pub prompt: Option<InspectorPrompt>,
    #[serde(default)]
    pub model_calls: Vec<ModelCallDiagnostic>,
    #[serde(default)]
    pub tool_executions: Vec<ToolExecutionDiagnostic>,
    #[serde(default)]
    pub activity: Vec<InspectorActivity>,
    #[serde(default)]
    pub stats: SessionDiagnosticStats,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InspectorActivity {
    pub sequence: u64,
    pub event: DiagnosticActivityEvent,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InspectorPrompt {
    #[serde(default)]
    pub components: Vec<PromptComponentDiagnostic>,
    #[serde(default)]
    pub components_truncated: bool,
    pub reconstructed_prompt: BoundedDiagnosticText,
    #[serde(default)]
    pub total_estimated_tokens: Option<u64>,
    #[serde(default)]
    pub message_count: u32,
    #[serde(default)]
    pub capability_count: u32,
    #[serde(default)]
    pub active_skills: Vec<BoundedDiagnosticText>,
    #[serde(default)]
    pub effective_model: Option<BoundedDiagnosticText>,
    #[serde(default)]
    pub context_limit: Option<u64>,
}

#[derive(Deserialize)]
struct SnapshotEnvelope {
    snapshot: Option<InspectorRun>,
}

#[derive(Deserialize)]
struct PromptEnvelope {
    prompt: Option<InspectorPrompt>,
}

#[derive(Deserialize)]
struct ToolEnvelope {
    tool: Option<ToolExecutionDiagnostic>,
}

// --- inspector live updates ---

const INSPECTOR_RECONNECT: Duration = Duration::from_secs(2);
const INSPECTOR_SILENCE: Duration = Duration::from_secs(45);

/// What the inspector update stream reports. Updates carry no payload: the
/// app re-reads the snapshot, which is the reconciled state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectorSignal {
    Live,
    Changed,
    Closed(String),
}

/// Owns the update task; dropping it closes the stream and frees the slot.
pub struct InspectorStream {
    task: JoinHandle<()>,
}

impl Drop for InspectorStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Backend {
    /// Follow one run's diagnostic updates (`operator.inspector_updates`).
    pub fn inspector_updates(
        &self,
        thread_id: &str,
        run_id: &str,
    ) -> (InspectorStream, mpsc::Receiver<InspectorSignal>) {
        let (tx, rx) = mpsc::channel(64);
        let (thread_id, run_id) = (thread_id.to_string(), run_id.to_string());
        let task =
            self.spawn(move |c| async move { follow_inspector(c, thread_id, run_id, tx).await });
        (InspectorStream { task }, rx)
    }
}

enum StreamEnd {
    Reconnect,
    Stop(String),
}

/// Reconnects with `Last-Event-ID` until the receiver goes away or the
/// gateway refuses for good. One `connection_id` with a rising generation, so
/// a reconnect replaces its own stream slot instead of taking another.
async fn follow_inspector(
    client: Client,
    thread_id: String,
    run_id: String,
    tx: mpsc::Sender<InspectorSignal>,
) {
    let connection_id = format!("desktop-inspector-{}", uuid::Uuid::new_v4().simple());
    let mut last_id: Option<String> = None;
    let mut generation: u64 = 0;
    loop {
        generation += 1;
        let path = format!(
            "{}/events?connection_id={}&connection_generation={generation}",
            run_path(&thread_id, &run_id),
            seg(&connection_id),
        );
        match inspector_once(&client, &path, &mut last_id, &tx).await {
            StreamEnd::Stop(reason) => {
                let _ = tx.send(InspectorSignal::Closed(reason)).await;
                return;
            }
            StreamEnd::Reconnect if tx.is_closed() => return,
            StreamEnd::Reconnect => tokio::time::sleep(INSPECTOR_RECONNECT).await,
        }
    }
}

async fn inspector_once(
    client: &Client,
    path: &str,
    last_id: &mut Option<String>,
    tx: &mpsc::Sender<InspectorSignal>,
) -> StreamEnd {
    let mut req = client
        .request(Method::GET, path)
        .header(reqwest::header::ACCEPT, "text/event-stream");
    if let Some(id) = last_id.as_deref() {
        req = req.header("Last-Event-ID", id);
    }
    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => return StreamEnd::Stop(format!("cannot reach the gateway: {e}")),
    };
    match resp.status() {
        StatusCode::OK => {}
        StatusCode::NO_CONTENT => return StreamEnd::Reconnect,
        StatusCode::TOO_MANY_REQUESTS => {
            return StreamEnd::Stop("all live stream slots are in use".into());
        }
        status => {
            let text = resp.text().await.unwrap_or_default();
            return StreamEnd::Stop(ClientError::from_status(status.as_u16(), &text).to_string());
        }
    }
    let mut events = resp.bytes_stream().eventsource();
    loop {
        let event = match tokio::time::timeout(INSPECTOR_SILENCE, events.next()).await {
            Ok(Some(Ok(event))) => event,
            // Silence, a read error, or the server's lifetime cap: resume.
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return StreamEnd::Reconnect,
        };
        if !event.id.is_empty() {
            *last_id = Some(event.id.clone());
        }
        let signal = match event.event.as_str() {
            "diagnostic_connected" => InspectorSignal::Live,
            "diagnostic_update" | "diagnostic_rebase" => InspectorSignal::Changed,
            "stream_error" => return StreamEnd::Stop("the gateway ended the stream".into()),
            _ => continue,
        };
        if tx.send(signal).await.is_err() {
            return StreamEnd::Stop(String::new());
        }
    }
}

fn run_path(thread_id: &str, run_id: &str) -> String {
    format!(
        "{V2}/operator/inspector/threads/{}/runs/{}",
        seg(thread_id),
        seg(run_id)
    )
}

impl Client {
    // --- setup ---

    pub async fn operator_setup(&self) -> Result<RebornOperatorSetupResponse, ClientError> {
        self.get(&format!("{V2}/operator/setup")).await
    }

    pub async fn run_operator_setup(
        &self,
        input: &SetupInput,
    ) -> Result<RebornOperatorSetupResponse, ClientError> {
        self.post(&format!("{V2}/operator/setup"), &input.body())
            .await
    }

    // --- config keys ---

    pub async fn operator_config(&self) -> Result<RebornOperatorConfigListResponse, ClientError> {
        self.get(&format!("{V2}/operator/config")).await
    }

    pub async fn operator_config_key(
        &self,
        key: &str,
    ) -> Result<RebornOperatorConfigGetResponse, ClientError> {
        self.get(&format!("{V2}/operator/config/{}", seg(key)))
            .await
    }

    pub async fn set_operator_config_key(
        &self,
        key: &str,
        value: serde_json::Value,
    ) -> Result<RebornOperatorConfigGetResponse, ClientError> {
        let body = RebornOperatorConfigSetRequest { value };
        self.post(&format!("{V2}/operator/config/{}", seg(key)), &body)
            .await
    }

    /// An empty key list validates every key.
    pub async fn validate_operator_config(
        &self,
        keys: Vec<String>,
    ) -> Result<RebornOperatorConfigValidateResponse, ClientError> {
        let body = RebornOperatorConfigValidateRequest { keys };
        self.post(&format!("{V2}/operator/config/validate"), &body)
            .await
    }

    // --- status, diagnostics, service ---

    pub async fn operator_status(&self) -> Result<RebornOperatorCommandPlaneResponse, ClientError> {
        self.get(&format!("{V2}/operator/status")).await
    }

    pub async fn operator_diagnostics(
        &self,
    ) -> Result<RebornOperatorCommandPlaneResponse, ClientError> {
        self.get(&format!("{V2}/operator/diagnostics")).await
    }

    /// `Stop` takes the gateway down, so its response may never arrive.
    pub async fn operator_service(
        &self,
        action: RebornOperatorServiceLifecycleAction,
    ) -> Result<RebornOperatorCommandPlaneResponse, ClientError> {
        let body = RebornOperatorServiceLifecycleRequest { action };
        self.post(&format!("{V2}/operator/service"), &body).await
    }

    // --- logs ---

    /// The operator-wide log ring (`operator.logs`).
    pub async fn operator_logs(
        &self,
        query: &RebornOperatorLogsQuery,
    ) -> Result<RebornOperatorCommandPlaneResponse, ClientError> {
        let req = self
            .request(Method::GET, &format!("{V2}/operator/logs"))
            .query(query);
        self.send(req).await
    }

    /// Caller-scoped logs (`logs`); the gateway requires `thread_id`.
    pub async fn thread_logs(
        &self,
        query: &RebornOperatorLogsQuery,
    ) -> Result<RebornLogQueryResponse, ClientError> {
        let req = self
            .request(Method::GET, &format!("{V2}/logs"))
            .query(query);
        self.send(req).await
    }

    // --- inspector ---

    /// `None` when the gateway kept no diagnostics for the run (it retains a
    /// few recent runs per session, in memory).
    pub async fn inspector_snapshot(
        &self,
        thread_id: &str,
        run_id: &str,
    ) -> Result<Option<InspectorRun>, ClientError> {
        let envelope: SnapshotEnvelope = self.get(&run_path(thread_id, run_id)).await?;
        Ok(envelope.snapshot)
    }

    pub async fn inspector_prompt(
        &self,
        thread_id: &str,
        run_id: &str,
    ) -> Result<Option<InspectorPrompt>, ClientError> {
        let path = format!("{}/prompt", run_path(thread_id, run_id));
        let envelope: PromptEnvelope = self.get(&path).await?;
        Ok(envelope.prompt)
    }

    pub async fn inspector_tool(
        &self,
        thread_id: &str,
        run_id: &str,
        activity_id: &str,
    ) -> Result<Option<ToolExecutionDiagnostic>, ClientError> {
        let path = format!("{}/tools/{}", run_path(thread_id, run_id), seg(activity_id));
        let envelope: ToolEnvelope = self.get(&path).await?;
        Ok(envelope.tool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_body_keeps_only_filled_fields() {
        let input = SetupInput {
            provider_id: " glm ".into(),
            model: "glm-5.3".into(),
            api_key: "  ".into(),
            ..Default::default()
        };
        assert_eq!(
            input.body(),
            serde_json::json!({ "provider_id": "glm", "model": "glm-5.3" })
        );
    }

    #[test]
    fn snapshot_decodes_from_gateway_shape() {
        let body = serde_json::json!({ "snapshot": {
            "activity": [{ "sequence": 1, "event": {
                "activity_id": null, "iteration": null, "kind": "turn_started",
                "model_call_id": null, "occurred_at": "2026-10-23T11:48:25.722839Z",
                "summary": { "content": "Turn started", "original_bytes": 12, "truncated": false }
            }}],
            "latest_sequence": 1,
            "model_calls": [],
            "tool_executions": [],
            "prompt": null,
            "scope": { "tenant_id": "t", "user_id": "u", "thread_id": "x", "run_id": "y" },
            "stream_id": "10f1199c-82ce-4c3c-8bb3-3d1ac2c7402d",
            "stats": {
                "total_model_calls": 0, "calls_per_model": [], "calls_per_model_truncated": false,
                "input_tokens": { "known_total": 0, "unavailable_samples": 0 },
                "output_tokens": { "known_total": 0, "unavailable_samples": 0 },
                "cache_read_input_tokens": { "known_total": 0, "unavailable_samples": 0 },
                "cache_creation_input_tokens": { "known_total": 0, "unavailable_samples": 0 },
                "total_latency_ms": { "known_total": 0, "unavailable_samples": 0 },
                "total_tool_calls": 0, "successful_tool_calls": 0, "failed_tool_calls": 0
            }
        }});
        let decoded: SnapshotEnvelope = serde_json::from_value(body).expect("decode snapshot");
        let run = decoded.snapshot.expect("snapshot present");
        assert_eq!(run.activity.len(), 1);
        assert_eq!(
            run.activity[0].event.summary.as_ref().map(|s| s.content()),
            Some("Turn started")
        );
    }
}

// --- extension deployment configuration ---
//
// The gateway serves these as untyped JSON; the wire types live in
// `ironclaw_assistant` (`RebornAdminConfiguration*`), which the client does
// not depend on. These decode the fields the app shows.

#[derive(Debug, Clone, Deserialize)]
pub struct AdminConfigGroups {
    pub groups: Vec<AdminConfigGroup>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminConfigGroup {
    pub group_id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    pub revision: u64,
    pub complete: bool,
    pub fields: Vec<AdminConfigField>,
    #[serde(default)]
    pub used_by: Vec<AdminConfigUse>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminConfigField {
    pub handle: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    pub secret: bool,
    pub required: bool,
    pub provided: bool,
    /// Present for non-secret fields that have a value.
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdminConfigUse {
    pub display_name: String,
    pub installed: bool,
}

impl Client {
    pub async fn extension_configuration(&self) -> Result<AdminConfigGroups, ClientError> {
        self.get(&format!("{V2}/operator/extension-configuration"))
            .await
    }

    /// Replace one group's values. A blank secret keeps the stored one;
    /// `revision` guards against overwriting a newer save (409).
    pub async fn replace_extension_configuration(
        &self,
        group_id: &str,
        revision: u64,
        values: Vec<(String, String)>,
    ) -> Result<serde_json::Value, ClientError> {
        let values: Vec<_> = values
            .into_iter()
            .map(|(handle, value)| serde_json::json!({ "handle": handle, "value": value }))
            .collect();
        let body = serde_json::json!({
            "values": values,
            "expected_revision": revision,
            "idempotency_key": crate::api::client_action_id(),
        });
        self.send(
            self.request(
                Method::PUT,
                &format!("{V2}/operator/extension-configuration/{}", seg(group_id)),
            )
            .json(&body),
        )
        .await
    }
}
