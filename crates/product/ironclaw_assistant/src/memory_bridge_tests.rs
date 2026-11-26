//! The CLI memory tools through the bridge provider, answered by the real
//! Memory page surface over an in-memory document store.

use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use ironclaw_orchestrator::model::{RuntimeId, ThreadScope};

use super::*;
use crate::memory_page::MemoryPageSurface;
use crate::memory_page::tests::{FakeDocuments, Inner};

fn tools() -> MemoryBridgeTools {
    let docs = Arc::new(FakeDocuments::default());
    MemoryBridgeTools {
        memory: Arc::new(MemoryPageSurface::new(
            Arc::new(Inner),
            Some(docs as Arc<dyn MemoryDocumentsSource>),
            None,
            None,
        )),
    }
}

fn thread_of(user: &str) -> ThreadRecord {
    ThreadRecord::new(
        ThreadId::new(format!("t-{user}")).unwrap(),
        ThreadScope {
            tenant_id: TenantId::new("t1").unwrap(),
            user_id: UserId::new(user).unwrap(),
            agent_id: None,
            project_id: None,
        },
        RuntimeId::new("claude-cli").unwrap(),
    )
}

fn text(outcome: &Value) -> (&str, bool) {
    (
        outcome["content"][0]["text"].as_str().unwrap(),
        outcome["isError"].as_bool().unwrap(),
    )
}

#[tokio::test]
async fn lists_the_four_tools_and_handles_only_them() {
    let tools = tools();
    let names: Vec<String> = tools
        .tools()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, [TREE, READ, SEARCH, WRITE]);
    assert!(tools.handles(WRITE) && !tools.handles("history_search"));
}

#[tokio::test]
async fn an_agent_creates_reads_then_replaces_a_wiki_page_only_with_its_hash() {
    let tools = tools();
    let alice = thread_of("alice");

    let created = tools
        .call(
            &alice,
            WRITE,
            &json!({"path": "wiki/concepts/tea.md", "content": "# Tea\ngreen"}),
        )
        .await;
    let (body, is_error) = text(&created);
    assert!(!is_error, "{body}");
    assert!(body.contains("wiki/concepts/tea.md"));

    // A blind second create does not overwrite.
    let blind = tools
        .call(
            &alice,
            WRITE,
            &json!({"path": "wiki/concepts/tea.md", "content": "replaced"}),
        )
        .await;
    let (body, is_error) = text(&blind);
    assert!(is_error && body.contains("content_hash"), "{body}");

    let read = tools
        .call(&alice, READ, &json!({"path": "wiki/concepts/tea.md"}))
        .await;
    let (body, is_error) = text(&read);
    assert!(!is_error);
    let document: Value = serde_json::from_str(body).unwrap();
    assert_eq!(document["content"], "# Tea\ngreen");
    let hash = document["content_hash"].as_str().unwrap().to_string();

    let replaced = tools
        .call(
            &alice,
            WRITE,
            &json!({"path": "wiki/concepts/tea.md", "content": "# Tea\noolong", "content_hash": hash}),
        )
        .await;
    assert!(!text(&replaced).1);
    let reread = tools
        .call(&alice, READ, &json!({"path": "wiki/concepts/tea.md"}))
        .await;
    assert!(text(&reread).0.contains("oolong"));

    let tree = tools.call(&alice, TREE, &json!({"prefix": "wiki/"})).await;
    let listed: Value = serde_json::from_str(text(&tree).0).unwrap();
    assert_eq!(listed["paths"], json!(["wiki/concepts/tea.md"]));

    let hits = tools.call(&alice, SEARCH, &json!({"query": "oolong"})).await;
    assert!(text(&hits).0.contains("wiki/concepts/tea.md"));
}

#[tokio::test]
async fn calls_act_as_the_thread_owner_only() {
    let tools = tools();
    tools
        .call(
            &thread_of("alice"),
            WRITE,
            &json!({"path": "notes/secret.md", "content": "alice only"}),
        )
        .await;
    let bob = thread_of("bob");
    let read = tools
        .call(&bob, READ, &json!({"path": "notes/secret.md"}))
        .await;
    let (body, is_error) = text(&read);
    assert!(is_error && body.contains("no such document"), "{body}");
    let tree = tools.call(&bob, TREE, &json!({})).await;
    assert_eq!(
        serde_json::from_str::<Value>(text(&tree).0).unwrap()["paths"],
        json!([])
    );
}

#[tokio::test]
async fn bad_paths_and_arguments_are_model_visible_errors_without_echoing_content() {
    let tools = tools();
    let alice = thread_of("alice");
    for path in ["../etc/passwd", "/abs.md", "archive/x.md"] {
        let outcome = tools
            .call(&alice, WRITE, &json!({"path": path, "content": "x"}))
            .await;
        let (body, is_error) = text(&outcome);
        assert!(is_error, "{path}: {body}");
        assert!(body.contains("invalid path"), "{path}: {body}");
    }
    let outcome = tools
        .call(
            &alice,
            WRITE,
            &json!({"path": "a.md", "content": "TOP-SECRET-CONTENT", "mode": "append"}),
        )
        .await;
    let (body, is_error) = text(&outcome);
    assert!(is_error && body.starts_with("arguments are invalid"), "{body}");
    assert!(!body.contains("TOP-SECRET-CONTENT"));
}
