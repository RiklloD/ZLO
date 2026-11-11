//! Scenario: with an embedding provider configured, `memory.search` finds a
//! saved fact by MEANING — a query sharing no word with the stored sentence —
//! and a query that matches both ways is reported as a hybrid match. With no
//! embedding provider (the default) the same meaning-only query finds nothing,
//! exactly today's full-text behavior.
//!
//! Both groups run the production-shaped native provider on libSQL and reach
//! it only through the model-visible memory tools. The embedding provider is
//! the deterministic `ConceptEmbeddings` double ("car" and "automobile" share
//! a concept; full text sees two unrelated words) behind composition's real
//! memory-embedder adapter: the fake is at the provider seam, no network.

use std::sync::Arc;

use super::reborn_support::group::{HarnessResult, RebornIntegrationGroup};
use super::reborn_support::reply::RebornScriptedReply;
use ironclaw_host_runtime::{MEMORY_SEARCH_CAPABILITY_ID, MEMORY_WRITE_CAPABILITY_ID};
use ironclaw_llm::testing::embeddings::ConceptEmbeddings;
use serde_json::{Value, json};

const FACT: &str = "remember that my car is painted teal-quokka-9";

pub async fn run() -> HarnessResult<()> {
    let embedder = Arc::new(ConceptEmbeddings::new(16).with_concept(["car", "automobile"]));
    let semantic = RebornIntegrationGroup::builder()
        .builtin_tools_with_semantic_memory_libsql(embedder.clone())
        .await?;
    seed(&semantic, "conv-semantic-writer").await?;
    // Index time: the saved sentence's chunk was embedded.
    if !embedder
        .calls()
        .iter()
        .any(|batch| batch.iter().any(|text| text.contains("teal-quokka-9")))
    {
        return Err("the written memory chunk never reached the embedder".into());
    }

    // Meaning-only query: no word in common with the fact.
    let hits = search(&semantic, "conv-semantic-meaning", "automobile").await?;
    let hit = find_fact(&hits)
        .ok_or_else(|| format!("semantic search must find the fact by meaning; got {hits}"))?;
    if hit["is_hybrid_match"] != json!(false) {
        return Err(format!("a vector-only hit is not hybrid: {hit}").into());
    }

    // A query matching by word ("car") and by meaning ("automobile").
    let hits = search(&semantic, "conv-semantic-both", "car automobile").await?;
    let hit =
        find_fact(&hits).ok_or_else(|| format!("hybrid search must find the fact; got {hits}"))?;
    if hit["is_hybrid_match"] != json!(true) {
        return Err(format!("a word+meaning hit must be hybrid: {hit}").into());
    }

    // Default deployment (no embedding provider): the meaning-only query
    // stays a full-text miss — behavior unchanged.
    let plain = RebornIntegrationGroup::builtin_tools_with_native_memory_libsql().await?;
    seed(&plain, "conv-plain-writer").await?;
    let hits = search(&plain, "conv-plain-meaning", "automobile").await?;
    if find_fact(&hits).is_some() {
        return Err(
            format!("without embeddings, search must stay full-text only; got {hits}").into(),
        );
    }
    Ok(())
}

async fn seed(group: &RebornIntegrationGroup, conversation: &str) -> HarnessResult<()> {
    let writer = group
        .thread(conversation)
        .script([
            RebornScriptedReply::tool_call(
                MEMORY_WRITE_CAPABILITY_ID,
                json!({"target": "memory", "content": FACT, "append": false}),
            ),
            RebornScriptedReply::text("saved"),
        ])
        .build()
        .await?;
    writer.submit_turn("remember my car's colour").await?;
    writer.assert_tool_invoked(MEMORY_WRITE_CAPABILITY_ID).await
}

async fn search(
    group: &RebornIntegrationGroup,
    conversation: &str,
    query: &str,
) -> HarnessResult<Value> {
    let searcher = group
        .thread(conversation)
        .script([
            RebornScriptedReply::tool_call(
                MEMORY_SEARCH_CAPABILITY_ID,
                json!({"query": query, "limit": 5}),
            ),
            RebornScriptedReply::text("searched"),
        ])
        .build()
        .await?;
    searcher.submit_turn("what colour is it").await?;
    searcher
        .assert_tool_invoked(MEMORY_SEARCH_CAPABILITY_ID)
        .await?;
    searcher
        .tool_result_output(MEMORY_SEARCH_CAPABILITY_ID)
        .await
}

fn find_fact(output: &Value) -> Option<&Value> {
    output["results"].as_array()?.iter().find(|result| {
        result["content"]
            .as_str()
            .is_some_and(|content| content.contains("teal-quokka-9"))
    })
}
