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
