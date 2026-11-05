//! @-mentions through the orchestrating surface's `turn.submit`: the turn
//! receives the typed text with its mentions expanded inside the granted
//! project folder, a thread mention reads only the sender's own threads,
//! and `mention_roots` never reaches the inner surface.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_host_api::ids::{ActivityId, CapabilityId, TenantId, UserId};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_product_contracts::mention::{
    MENTION_CONTEXT_OPEN, MentionUri, strip_mention_context,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use serde_json::{Value, json};

use super::surface::OrchestratedProductSurface;
use crate::reborn_services::{SUBMIT_TURN_COMMAND_ID, TIMELINE_VIEW};

/// IronClaw's own surface: records each submit, and serves `t-mine`'s
/// timeline to its owner only.
#[derive(Default)]
struct Recording {
    submits: Mutex<Vec<Value>>,
}

#[async_trait]
impl ProductSurface for Recording {
    async fn invoke(
        &self,
        _: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        if request.operation_id.as_str() == SUBMIT_TURN_COMMAND_ID {
            self.submits.lock().unwrap().push(request.input);
        }
        Ok(ProductSurfaceInvokeResponse { output: json!({}) })
    }
    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        let mine = request.view_id == TIMELINE_VIEW.id
            && request.input["thread_id"] == "t-mine"
            && caller.user_id.as_str() == "owner";
        if !mine {
            return Err(ProductSurfaceError::not_found());
        }
        let page = json!({
            "messages": [
                {"kind": "user", "content": "how do cookies expire?"},
                {"kind": "tool_result_reference", "content": "raw tool row"},
                {"kind": "assistant", "content": "After the max-age."},
            ],
            "summary_artifacts": [{"content": "Login cookie work."}],
        });
        Ok(serde_json::from_value(json!({"items": [page], "next_cursor": null})).unwrap())
    }
    async fn stream_events(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        Ok(ProductSurfaceStreamResponse {
            events: Vec::new(),
            next_cursor: None,
            subscription: None,
        })
    }
}

fn caller(user: &str) -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new(user).unwrap(),
        None,
        None,
    )
}

#[tokio::test]
async fn submit_expands_mentions_inside_the_root_and_the_senders_threads_only() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let inner = Arc::new(Recording::default());
    let surface = OrchestratedProductSurface::new(inner.clone(), engine, None);

    let project = dir.path().join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("notes.md"), "PROJECT-NOTES").unwrap();
    std::fs::write(dir.path().join("outside.md"), "OUTSIDE-SECRET").unwrap();
    let file = |path: std::path::PathBuf| {
        MentionUri::File {
            path: path.to_string_lossy().into_owned(),
            line_range: None,
        }
        .to_link("f")
    };
    let typed = format!(
        "compare {} {} with {}",
        file(project.join("notes.md")),
        file(dir.path().join("outside.md")),
        MentionUri::Thread {
            id: "t-mine".into()
        }
        .to_link("Cookies"),
    );

    let send = |who: ProductSurfaceCaller| {
        let typed = typed.clone();
        let roots = vec![project.to_string_lossy().into_owned()];
        let surface = &surface;
        async move {
            surface
                .invoke(
                    who,
                    ProductSurfaceInvokeRequest {
                        operation_id: CapabilityId::new(SUBMIT_TURN_COMMAND_ID).unwrap(),
                        input: json!({
                            "thread_id": "t-current",
                            "client_action_id": "a1",
                            "content": typed,
                            "mention_roots": roots,
                        }),
                        activity_id: ActivityId::new(),
                    },
                )
                .await
                .unwrap();
        }
    };

    send(caller("owner")).await;
    let sent = inner.submits.lock().unwrap()[0].clone();
    assert!(sent.get("mention_roots").is_none(), "{sent}");
    let content = sent["content"].as_str().unwrap();
    assert_eq!(strip_mention_context(content), typed);
    let context = &content[content.find(MENTION_CONTEXT_OPEN).unwrap()..];
    assert!(context.contains("PROJECT-NOTES"), "{context}");
    assert!(!context.contains("OUTSIDE-SECRET"), "{context}");
    assert!(context.contains("outside the project folder"), "{context}");
    assert!(context.contains("Login cookie work."), "{context}");
    assert!(
        context.contains("User: how do cookies expire?"),
        "{context}"
    );
    assert!(
        context.contains("Assistant: After the max-age."),
        "{context}"
    );
    assert!(!context.contains("raw tool row"), "{context}");

    // Another user naming the same thread reads nothing from it.
    send(caller("intruder")).await;
    let sent = inner.submits.lock().unwrap()[1].clone();
    let content = sent["content"].as_str().unwrap();
    assert!(!content.contains("cookies expire"), "{content}");
    assert!(
        content.contains("@Cookies: thread not available."),
        "{content}"
    );
}
