//! `session_import.status` and `session_import.run`: the caller's terminal
//! session import (settings, last run, "Import now"). The work is
//! `ironclaw_cli_providers::session_import`; this answers for the caller's
//! own scope only, and a run is a background job the status reports on.

use std::sync::Arc;

use ironclaw_cli_providers::session_import::SessionImporter;
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::profiles::ProfileOwner;
use ironclaw_product_contracts::session_import::{
    SESSION_IMPORT_COMMAND_ID, SESSION_IMPORT_VIEW, SessionImportRequest,
};
use ironclaw_product_contracts::surface::ProductSurfaceCaller;
use serde_json::Value;

use super::surface::scope_of;

fn unavailable() -> OrchestratorError {
    OrchestratorError::request(
        "session_import_unavailable",
        "The backend has no home folder to import sessions from.",
    )
}

/// The view, when `view` is it.
pub(super) async fn view(
    importer: Option<&Arc<SessionImporter>>,
    engine: &Engine,
    caller: &ProductSurfaceCaller,
    view: &str,
) -> Option<Result<Value, OrchestratorError>> {
    if view != SESSION_IMPORT_VIEW.id {
        return None;
    }
    Some(
        async {
            let importer = importer.ok_or_else(unavailable)?;
            let owner = ProfileOwner::from(&scope_of(caller));
            Ok(serde_json::to_value(
                importer.status(engine, &owner).await?,
            )?)
        }
        .await,
    )
}

/// The command, when `operation` is it. `Err(None)` for a malformed body.
pub(super) async fn command(
    importer: Option<&Arc<SessionImporter>>,
    engine: &Arc<Engine>,
    caller: &ProductSurfaceCaller,
    operation: &str,
    input: Value,
) -> Option<Result<Value, OrchestratorError>> {
    if operation != SESSION_IMPORT_COMMAND_ID {
        return None;
    }
    Some(
        async {
            let importer = importer.ok_or_else(unavailable)?;
            let request: SessionImportRequest = serde_json::from_value(input)
                .map_err(|e| OrchestratorError::invalid(format!("invalid import request: {e}")))?;
            let status = importer
                .request(engine, &scope_of(caller), &request)
                .await?;
            Ok(serde_json::to_value(status)?)
        }
        .await,
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use async_trait::async_trait;
    use ironclaw_cli_providers::claude::{ClaudeCliConfig, ClaudeCliRuntime};
    use ironclaw_cli_providers::session_import::{CLAUDE_CODE, CODEX, ImportPaths};
    use ironclaw_host_api::ids::{ActivityId, CapabilityId, TenantId, UserId};
    use ironclaw_orchestrator::runtime::TurnEventSink;
    use ironclaw_product_contracts::inbox::{INBOX_VIEW, InboxListResponse, InboxState};
    use ironclaw_product_contracts::session_import::SessionImportStatus;
    use ironclaw_product_contracts::surface::{
        ProductSurface, ProductSurfaceError, ProductSurfaceInvokeRequest,
        ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
        ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
    };
    use serde_json::json;

    use super::*;
    use crate::orchestration::OrchestratedProductSurface;

    /// The undecorated surface: no session threads, nothing else.
    struct Empty;

    #[async_trait]
    impl ProductSurface for Empty {
        async fn invoke(
            &self,
            _: ProductSurfaceCaller,
            _: ProductSurfaceInvokeRequest,
        ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
            Err(ProductSurfaceError::not_found())
        }
        async fn query(
            &self,
            _: ProductSurfaceCaller,
            request: ProductSurfaceQueryRequest,
        ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
            let item = match request.view_id.as_str() {
                "threads" => json!({"threads": []}),
                _ => return Err(ProductSurfaceError::not_found()),
            };
            Ok(ProductSurfaceQueryPage {
                items: vec![item],
                next_cursor: None,
            })
        }
        async fn stream_events(
            &self,
            _: ProductSurfaceCaller,
            _: ProductSurfaceStreamRequest,
        ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
            Err(ProductSurfaceError::not_found())
        }
    }

    fn caller() -> ProductSurfaceCaller {
        ProductSurfaceCaller::new(
            TenantId::new("t1").unwrap(),
            UserId::new("u1").unwrap(),
            None,
            None,
        )
    }

    async fn status(surface: &OrchestratedProductSurface) -> SessionImportStatus {
        let page = surface
            .query(
                caller(),
                ProductSurfaceQueryRequest {
                    view_id: SESSION_IMPORT_VIEW.id.into(),
                    input: json!({}),
                    cursor: None,
                    limit: None,
                },
            )
            .await
            .unwrap();
        serde_json::from_value(page.items[0].clone()).unwrap()
    }

    async fn run(surface: &OrchestratedProductSurface, input: Value) -> Value {
        surface
            .invoke(
                caller(),
                ProductSurfaceInvokeRequest {
                    operation_id: CapabilityId::new(SESSION_IMPORT_COMMAND_ID).unwrap(),
                    input,
                    activity_id: ActivityId::new(),
                },
            )
            .await
            .unwrap()
            .output
    }

    fn transcript(projects: &Path, cwd: &Path, session: &str) {
        let dir = projects.join(claude_codes_dir(cwd));
        std::fs::create_dir_all(&dir).unwrap();
        let row = |kind: &str, at: &str, text: &str| {
            json!({"type": kind, "sessionId": session, "cwd": cwd, "timestamp": at,
                   "message": {"role": kind, "content": text}})
            .to_string()
        };
        let body = format!(
            "{}\n{}\n",
            row("user", "2026-10-01T10:00:00Z", "Tidy the README"),
            row("assistant", "2026-10-01T10:00:20Z", "Done.")
        );
        std::fs::write(dir.join(format!("{session}.jsonl")), body).unwrap();
    }

    /// The CLI's folder name for a working directory (`/` and `.` → `-`).
    fn claude_codes_dir(cwd: &Path) -> String {
        cwd.to_string_lossy()
            .chars()
            .map(|c| if c == '/' || c == '.' { '-' } else { c })
            .collect()
    }

    #[tokio::test]
    async fn import_now_lands_a_terminal_session_settled_and_marked_on_the_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let db = Arc::new(
            libsql::Builder::new_local(root.join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
        let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
        let projects = root.join(".claude").join("projects");
        let sink = Arc::downgrade(&engine) as std::sync::Weak<dyn TurnEventSink>;
        let claude = ClaudeCliRuntime::new(
            ClaudeCliConfig {
                runtime_id: "claude-cli".into(),
                display_name: "Claude CLI".into(),
                binary: root.join("claude"),
                config_dir: None,
                work_dir: root.join("work"),
                login_file: root.join(".claude.json"),
                projects_dir: projects.clone(),
                usage_file: None,
            },
            sink,
            None,
        )
        .unwrap();
        engine.register_runtime(Arc::new(claude));
        let app = root.join("app");
        std::fs::create_dir_all(&app).unwrap();
        transcript(&projects, &app, "6c1e0b4a-0000-4000-8000-000000000001");

        let mut surface = OrchestratedProductSurface::new(Arc::new(Empty), engine, None);
        super::surface::set_importer(
            &mut surface,
            Arc::new(SessionImporter::new(ImportPaths {
                claude_projects: projects,
                codex_sessions: root.join(".codex").join("sessions"),
                codex_state: root.join(".ironclaw").join("codex"),
                claude_imports: ironclaw_cli_providers::claude::imports_dir(&root.join("work")),
                own_roots: vec![root.join(".ironclaw")],
            })),
        );

        let started: SessionImportStatus =
            serde_json::from_value(run(&surface, json!({"run": "manual"})).await).unwrap();
        assert!(started.running, "a run is a background job");
        let mut done = started;
        for _ in 0..200 {
            if !done.running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
            done = status(&surface).await;
        }
        assert!(!done.running && done.last_run_at.is_some());
        let claude = done
            .sources
            .iter()
            .find(|s| s.source == CLAUDE_CODE)
            .unwrap();
        assert_eq!(claude.counts.unwrap().imported, 1);
        let codex = done.sources.iter().find(|s| s.source == CODEX).unwrap();
        assert!(!codex.available, "no codex runtime in this backend");

        let page = surface
            .query(
                caller(),
                ProductSurfaceQueryRequest {
                    view_id: INBOX_VIEW.id.into(),
                    input: json!({}),
                    cursor: None,
                    limit: None,
                },
            )
            .await
            .unwrap();
        let list: InboxListResponse = serde_json::from_value(page.items[0].clone()).unwrap();
        let row = list
            .rows
            .iter()
            .find(|r| r.imported.as_deref() == Some(CLAUDE_CODE))
            .expect("the imported thread is on the inbox, marked");
        assert_eq!(row.state, InboxState::Settled);
        assert_eq!(row.status, None, "no badge, nothing unseen");
        assert_eq!(row.title.as_deref(), Some("Tidy the README"));

        // A malformed request is a correctable error, not a 5xx.
        let bad = run(&surface, json!({"wipe": true})).await;
        assert_eq!(bad["ok"], json!(false), "{bad}");
    }
}
