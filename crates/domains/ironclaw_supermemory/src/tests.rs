//! Hermetic tests: a scripted in-process egress stands in for the Supermemory
//! API and an in-memory store holds a fake key. No real API, no real secrets.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_host_api::action::NetworkMethod;
use ironclaw_host_api::ids::{SecretHandle, TenantId, ThreadId, UserId};
use ironclaw_host_api::turn::{TurnActor, TurnScope};
use ironclaw_loop_contracts::{
    AgentLoopHostError, ContextProfileId, LoopContextSnippet, MemoryPromptContextLoad,
    MemoryPromptContextRequest, MemoryPromptContextService,
};
use ironclaw_memory::{
    MemoryInteractionMessage, MemoryInteractionRole, MemoryInvocation, MemoryService,
    MemoryServiceRecordRequest,
};
use ironclaw_network::{
    NetworkHttpEgress, NetworkHttpError, NetworkHttpRequest, NetworkHttpResponse,
};
use ironclaw_product_contracts::operator_secrets::{
    OperatorSecretValueStore, OperatorSecretValueStoreError,
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};

use crate::binding::StaticSupermemoryBindingSource;
use crate::capture;
use crate::recall;
use crate::*;

const FAKE_KEY: &str = "sm_test_key_not_real";

#[derive(Debug, Clone)]
struct Recorded {
    method: NetworkMethod,
    url: String,
    headers: Vec<(String, String)>,
    body: Value,
}

#[derive(Default)]
struct ScriptedEgress {
    responses: Mutex<HashMap<String, VecDeque<(u16, Value)>>>,
    requests: Mutex<Vec<Recorded>>,
}

impl ScriptedEgress {
    fn respond(&self, path: &str, status: u16, body: Value) {
        self.responses
            .lock()
            .unwrap()
            .entry(path.to_string())
            .or_default()
            .push_back((status, body));
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn requests_to(&self, path: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|request| request.url.ends_with(path))
            .collect()
    }
}

#[async_trait]
impl NetworkHttpEgress for ScriptedEgress {
    async fn execute(
        &self,
        request: NetworkHttpRequest,
    ) -> Result<NetworkHttpResponse, NetworkHttpError> {
        let path = request
            .url
            .strip_prefix("https://api.supermemory.test")
            .unwrap_or(&request.url)
            .to_string();
        let body = if request.body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&request.body).unwrap()
        };
        self.requests.lock().unwrap().push(Recorded {
            method: request.method,
            url: request.url.clone(),
            headers: request.headers.clone(),
            body,
        });
        let scripted = self
            .responses
            .lock()
            .unwrap()
            .get_mut(&path)
            .and_then(VecDeque::pop_front);
        let (status, body) = scripted.unwrap_or((404, json!({"error": "not scripted"})));
        Ok(NetworkHttpResponse {
            status,
            headers: Vec::new(),
            body: serde_json::to_vec(&body).unwrap(),
            usage: Default::default(),
        })
    }
}

#[derive(Default)]
struct MemoryKeys(Mutex<HashMap<String, String>>);

impl MemoryKeys {
    fn with_key() -> Arc<Self> {
        let keys = Self::default();
        keys.0
            .lock()
            .unwrap()
            .insert(SUPERMEMORY_API_KEY_SECRET.to_string(), FAKE_KEY.to_string());
        Arc::new(keys)
    }
}

#[async_trait]
impl OperatorSecretValueStore for MemoryKeys {
    async fn put(
        &self,
        handle: &SecretHandle,
        value: SecretString,
    ) -> Result<(), OperatorSecretValueStoreError> {
        self.0.lock().unwrap().insert(
            handle.as_str().to_string(),
            value.expose_secret().to_string(),
        );
        Ok(())
    }
    async fn contains(&self, handle: &SecretHandle) -> Result<bool, OperatorSecretValueStoreError> {
        Ok(self.0.lock().unwrap().contains_key(handle.as_str()))
    }
    async fn handles(&self) -> Result<Vec<SecretHandle>, OperatorSecretValueStoreError> {
        Ok(Vec::new())
    }
    async fn read(
        &self,
        handle: &SecretHandle,
    ) -> Result<Option<SecretString>, OperatorSecretValueStoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(handle.as_str())
            .map(|value| SecretString::from(value.clone())))
    }
    async fn delete(&self, handle: &SecretHandle) -> Result<bool, OperatorSecretValueStoreError> {
        Ok(self.0.lock().unwrap().remove(handle.as_str()).is_some())
    }
}

fn config() -> SupermemoryConfig {
    SupermemoryConfig {
        base_url: "https://api.supermemory.test".to_string(),
        ..SupermemoryConfig::default()
    }
}

fn build(keys: Arc<MemoryKeys>, read: &str) -> (Arc<Supermemory>, Arc<ScriptedEgress>) {
    let egress = Arc::new(ScriptedEgress::default());
    let client = SupermemoryClient::new(&config(), egress.clone(), keys)
        .unwrap()
        .without_retry_delay();
    let bindings =
        StaticSupermemoryBindingSource::from_values(Some("ironclaw"), Some(read)).unwrap();
    let supermemory = Supermemory::new(client, Arc::new(bindings), config());
    (Arc::new(supermemory), egress)
}

fn caller() -> SupermemoryCaller {
    SupermemoryCaller::from_turn_scope(&scope(), UserId::new("user-1").unwrap())
}

fn scope() -> TurnScope {
    TurnScope::new(
        TenantId::new("tenant-1").unwrap(),
        None,
        None,
        ThreadId::new("thread-1").unwrap(),
    )
}

fn prompt_request(query: &str) -> MemoryPromptContextRequest {
    MemoryPromptContextRequest {
        scope: scope(),
        actor: TurnActor::new(UserId::new("user-1").unwrap()),
        query: query.to_string(),
        max_snippets: 8,
        context_profile_id: ContextProfileId::new("default").unwrap(),
    }
}

struct LocalContext;

#[async_trait]
impl MemoryPromptContextService for LocalContext {
    async fn load_memory_snippets(
        &self,
        _request: MemoryPromptContextRequest,
    ) -> Result<MemoryPromptContextLoad, AgentLoopHostError> {
        Ok(MemoryPromptContextLoad::healthy(vec![
            LoopContextSnippet::from_untrusted_memory(
                "local:1".to_string(),
                "local memory".to_string(),
            )?,
        ]))
    }
}

#[derive(Default)]
struct LocalWriter(Mutex<usize>);

#[async_trait]
impl MemoryService for LocalWriter {
    async fn record_interaction(
        &self,
        _invocation: MemoryInvocation,
        _request: MemoryServiceRecordRequest,
    ) -> Result<ironclaw_memory::MemoryServiceRecordResponse, ironclaw_memory::MemoryServiceError>
    {
        *self.0.lock().unwrap() += 1;
        Ok(ironclaw_memory::MemoryServiceRecordResponse { recorded: true })
    }
}

fn invocation() -> MemoryInvocation {
    MemoryInvocation {
        scope: caller().network_scope(),
        correlation_id: ironclaw_host_api::ids::CorrelationId::new(),
    }
}

fn record_request(user: &str, assistant: &str) -> MemoryServiceRecordRequest {
    let message = |role, content: &str| MemoryInteractionMessage {
        role,
        content: content.to_string(),
        name: None,
    };
    MemoryServiceRecordRequest {
        messages: vec![
            message(MemoryInteractionRole::User, user),
            message(MemoryInteractionRole::Tool, "tool output is not captured"),
            message(MemoryInteractionRole::Assistant, assistant),
        ],
        turn_run_id: None,
        metadata: json!({}),
    }
}

#[tokio::test]
async fn without_a_key_local_memory_is_used_and_nothing_is_sent() {
    let (supermemory, egress) = build(Arc::new(MemoryKeys::default()), "career");
    let context =
        SupermemoryPromptContext::new(Arc::clone(&supermemory), Some(Arc::new(LocalContext)));
    let load = context
        .load_memory_snippets(prompt_request("hello"))
        .await
        .unwrap();
    assert_eq!(load.snippets.len(), 1);
    assert_eq!(load.snippets[0].model_content, "local memory");

    let local = Arc::new(LocalWriter::default());
    let recorder =
        SupermemoryInteractionRecorder::new(Arc::clone(&supermemory), Some(local.clone()));
    let response = recorder
        .record_interaction(invocation(), record_request("hi", "hello"))
        .await
        .unwrap();
    assert!(response.recorded);
    assert_eq!(*local.0.lock().unwrap(), 1);

    let search = run_search(&supermemory, &caller(), &json!({"query": "x"})).await;
    assert_eq!(search["ok"], false);
    assert_eq!(search["error"], "disabled");
    assert!(!supermemory.is_active(&caller()).await);
    assert!(egress.requests().is_empty());
}

#[tokio::test]
async fn recall_queries_primary_profile_and_read_containers_and_replaces_local_memory() {
    let (supermemory, egress) = build(MemoryKeys::with_key(), "career, thesis");
    egress.respond(
        "/v4/profile",
        200,
        json!({
            "profile": {"static": ["Lorik studies business"], "dynamic": ["Building IronClaw"]},
            "searchResults": {"results": [
                {"id": "m1", "memory": "Prefers short answers", "similarity": 0.874},
                {"id": "m2", "memory": "lorik studies business", "similarity": 0.5}
            ]}
        }),
    );
    egress.respond(
        "/v4/search",
        200,
        json!({"results": [{"id": "c1", "memory": "Siemens job", "similarity": 0.61}]}),
    );
    egress.respond("/v4/search", 500, json!({}));

    let long_message = "a".repeat(250);
    let context =
        SupermemoryPromptContext::new(Arc::clone(&supermemory), Some(Arc::new(LocalContext)));
    let load = context
        .load_memory_snippets(prompt_request(&long_message))
        .await
        .unwrap();

    let profile = egress.requests_to("/v4/profile");
    assert_eq!(profile.len(), 1);
    assert_eq!(profile[0].method, NetworkMethod::Post);
    assert_eq!(profile[0].body["containerTag"], "ironclaw");
    assert_eq!(
        profile[0].body["q"].as_str().unwrap().chars().count(),
        recall::RECALL_QUERY_CHARS
    );
    assert!(
        profile[0]
            .headers
            .contains(&("authorization".to_string(), format!("Bearer {FAKE_KEY}")))
    );
    assert!(
        profile[0]
            .headers
            .contains(&("x-sm-source".to_string(), "ironclaw".to_string()))
    );
    let searches = egress.requests_to("/v4/search");
    let mut searched: Vec<_> = searches
        .iter()
        .map(|request| request.body["containerTag"].as_str().unwrap().to_string())
        .collect();
    searched.sort();
    assert_eq!(searched, ["career", "thesis"]);
    assert_eq!(
        searches[0].body["limit"],
        recall::READ_CONTAINER_RECALL_LIMIT
    );

    assert!(load.degradations.is_empty());
    let all: Vec<&str> = load
        .snippets
        .iter()
        .map(|s| s.model_content.as_str())
        .collect();
    assert!(all[0].contains("Primary container: ironclaw"));
    assert!(all[0].contains("career, thesis"));
    let body = all[1..].join("\n");
    assert!(body.contains("Profile: Lorik studies business"));
    assert!(body.contains("Recent context: Building IronClaw"));
    assert!(body.contains("Memory [87%]: Prefers short answers"));
    assert!(body.contains("[career]") && body.contains("Siemens job"));
    // Deduplicated against the profile fact; local memory is not consulted.
    assert_eq!(body.matches("studies business").count(), 1);
    assert!(!body.contains("local memory"));
}

#[tokio::test]
async fn failed_primary_recall_degrades_without_falling_back_to_local_memory() {
    let (supermemory, egress) = build(MemoryKeys::with_key(), "");
    egress.respond("/v4/profile", 503, json!({}));
    let context =
        SupermemoryPromptContext::new(Arc::clone(&supermemory), Some(Arc::new(LocalContext)));
    let load = context
        .load_memory_snippets(prompt_request("hi"))
        .await
        .unwrap();
    assert_eq!(load.snippets.len(), 1, "guidance only");
    assert!(load.is_degraded());
}

#[tokio::test]
async fn capture_writes_one_cleaned_turn_document_and_retries_failures() {
    let (supermemory, egress) = build(MemoryKeys::with_key(), "career");
    let recorder = SupermemoryInteractionRecorder::new(
        Arc::clone(&supermemory),
        Some(Arc::new(LocalWriter::default())),
    );
    egress.respond("/v3/documents", 500, json!({}));
    let user = "Remember I like tea <supermemory-context>old recall</supermemory-context> see data:image/png;base64,iVBORw0KGgo=";
    let first = recorder
        .record_interaction(invocation(), record_request(user, "Noted."))
        .await
        .unwrap();
    assert!(!first.recorded);
    assert_eq!(supermemory.pending_len(), 1);

    egress.respond(
        "/v3/documents",
        200,
        json!({"id": "doc-1", "status": "queued"}),
    );
    let second = recorder
        .record_interaction(invocation(), record_request("And coffee", "Got it."))
        .await
        .unwrap();
    assert!(second.recorded);
    assert_eq!(supermemory.pending_len(), 0);

    let writes = egress.requests_to("/v3/documents");
    assert_eq!(writes.len(), 2);
    let body = &writes[1].body;
    assert_eq!(body["containerTags"], json!(["ironclaw"]));
    let custom_id = body["customId"].as_str().unwrap();
    assert!(custom_id.starts_with("thread_1_"), "{custom_id}");
    assert!(custom_id.contains("_b"));
    assert_eq!(body["metadata"]["type"], "conversation");
    assert_eq!(body["metadata"]["sm_source"], "ironclaw");
    let content = body["content"].as_str().unwrap();
    assert_eq!(
        content,
        "[role: user]\nRemember I like tea see [image]\n[user:end]\n[role: assistant]\nNoted.\n[assistant:end]\n\n\
         [role: user]\nAnd coffee\n[user:end]\n[role: assistant]\nGot it.\n[assistant:end]"
    );
    assert!(!content.contains("tool output"));
    assert!(
        body["entityContext"]
            .as_str()
            .unwrap()
            .starts_with("User-assistant conversation.")
    );
}

#[test]
fn pending_captures_are_bounded_oldest_first() {
    let mut pending: Vec<CapturedTurn> = (0..60)
        .map(|index| CapturedTurn::new("t", &format!("turn {index}"), "").unwrap())
        .collect();
    assert_eq!(capture::bound_pending(&mut pending), 10);
    assert_eq!(pending.len(), capture::MAX_PENDING_TURNS);
    assert_eq!(pending[0].user, "turn 10");
    assert!(CapturedTurn::new("t", "  ", "<supermemory-context>x</supermemory-context>").is_none());
}

#[tokio::test]
async fn search_targets_only_bound_containers() {
    let (supermemory, egress) = build(MemoryKeys::with_key(), "career");
    let denied = run_search(
        &supermemory,
        &caller(),
        &json!({"query": "job", "container": "finance"}),
    )
    .await;
    assert_eq!(denied["ok"], false);
    assert_eq!(denied["error"], "invalid_input");
    assert!(
        denied["message"]
            .as_str()
            .unwrap()
            .contains("ironclaw, career")
    );
    assert!(egress.requests().is_empty());

    egress.respond(
        "/v4/search",
        200,
        json!([{"id": "c1", "chunk": "Siemens job", "score": "0.5"}]),
    );
    let found = run_search(
        &supermemory,
        &caller(),
        &json!({"query": "job", "container": "career", "limit": 99}),
    )
    .await;
    assert_eq!(found["ok"], true);
    assert_eq!(found["count"], 1);
    assert_eq!(found["results"][0]["container"], "career");
    assert_eq!(found["results"][0]["similarity"], 50);
    assert_eq!(egress.requests_to("/v4/search")[0].body["limit"], 20);
}

#[tokio::test]
async fn write_goes_to_primary_and_reads_back_after_a_rate_limit() {
    let (supermemory, egress) = build(MemoryKeys::with_key(), "career");
    egress.respond("/v3/documents", 429, json!({}));
    egress.respond(
        "/v3/documents",
        200,
        json!({"id": "doc-9", "status": "queued"}),
    );
    egress.respond(
        "/v3/documents/doc-9",
        200,
        json!({"id": "doc-9", "status": "done"}),
    );
    let input = json!({"content": "I prefer Rust", "metadata": {"sm_source": "evil", "topic": "lang", "nested": {"x": 1}}});
    let result = run_write(&supermemory, &caller(), &input).await;
    assert_eq!(result["ok"], true);
    assert_eq!(result["id"], "doc-9");
    assert_eq!(result["container"], "ironclaw");
    assert_eq!(result["status"], "done");
    assert_eq!(result["verified"], true);

    let writes = egress.requests_to("/v3/documents");
    assert_eq!(writes.len(), 2, "429 retried once");
    let body = &writes[1].body;
    assert_eq!(body["containerTags"], json!(["ironclaw"]));
    assert_eq!(body["metadata"]["sm_source"], "ironclaw");
    assert_eq!(body["metadata"]["topic"], "lang");
    assert_eq!(body["metadata"]["type"], "preference");
    assert!(body["metadata"].get("nested").is_none());
    assert_eq!(
        egress.requests_to("/v3/documents/doc-9")[0].method,
        NetworkMethod::Get
    );
}

#[tokio::test]
async fn write_without_read_back_is_reported_unverified() {
    let (supermemory, egress) = build(MemoryKeys::with_key(), "");
    egress.respond("/v3/documents", 200, json!({"id": "doc-2"}));
    let result = run_write(&supermemory, &caller(), &json!({"content": "fact"})).await;
    assert_eq!(result["ok"], true);
    assert_eq!(result["verified"], false);
}

#[tokio::test]
async fn unauthorized_key_is_a_model_visible_failure_without_the_key() {
    let (supermemory, egress) = build(MemoryKeys::with_key(), "");
    egress.respond("/v4/search", 401, json!({"error": FAKE_KEY}));
    let result = run_search(&supermemory, &caller(), &json!({"query": "x"})).await;
    assert_eq!(result["error"], "unauthorized");
    assert!(!result.to_string().contains(FAKE_KEY));
}

#[test]
fn local_memory_tools_are_the_ones_hidden() {
    assert!(hides_local_memory_capability("ironclaw.memory.write"));
    assert!(hides_local_memory_capability("ironclaw.memory.search"));
    assert!(!hides_local_memory_capability(
        SUPERMEMORY_WRITE_CAPABILITY_ID
    ));
    assert!(!hides_local_memory_capability("builtin.t3_delegate"));
    assert!(validate_arguments(SUPERMEMORY_WRITE_CAPABILITY_ID, &json!({"content": " "})).is_err());
    assert!(validate_arguments(SUPERMEMORY_SEARCH_CAPABILITY_ID, &json!({"query": "q"})).is_ok());
}

#[test]
fn bindings_and_config_parse_like_supermemory_json() {
    let binding =
        StaticSupermemoryBindingSource::from_values(None, Some("career, ironclaw,career,"));
    assert!(binding.is_ok());
    assert!(StaticSupermemoryBindingSource::from_values(Some("bad tag!"), None).is_err());
    let parsed = SupermemoryConfig::from_values(
        Some("https://sm.example.com/"),
        Some("off"),
        None,
        Some("50"),
    )
    .unwrap();
    assert_eq!(parsed.base_url, "https://sm.example.com");
    assert!(!parsed.auto_recall && parsed.auto_capture);
    assert_eq!(parsed.max_recall_results, 20);
    assert!(SupermemoryConfig::from_values(None, Some("maybe"), None, None).is_err());
}

#[test]
fn relative_time_matches_hermes_buckets() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-23T12:00:00Z")
        .unwrap()
        .to_utc();
    assert_eq!(
        recall::relative_time("2026-10-23T11:50:00Z", now).as_deref(),
        Some("just now")
    );
    assert_eq!(
        recall::relative_time("2026-10-23T09:00:00Z", now).as_deref(),
        Some("3h ago")
    );
    assert_eq!(
        recall::relative_time("2026-10-21T12:00:00Z", now).as_deref(),
        Some("2d ago")
    );
    assert_eq!(
        recall::relative_time("2025-03-07T12:00:00Z", now).as_deref(),
        Some("07 Mar 2025")
    );
    assert_eq!(recall::relative_time("garbage", now), None);
}
