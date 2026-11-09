//! The thread repository and console operations
//! (`ironclaw_product_contracts::thread_repo`) on the orchestration
//! decorator: operator callers only, on threads the caller owns, answered
//! by the engine's `repo` and `console` modules.
//!
//! The WebUI already classifies these routes as operator-only (mounted only
//! with an operator surface, pre-gated per token); the check is repeated
//! here so no other transport reaches host git or a host shell without it.

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::model::{ThreadRecord, ThreadScope};
use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceErrorCode,
};
use ironclaw_product_contracts::thread_repo::*;
use serde_json::Value;

use super::surface::{answer, decode, scope_of};

fn operator_only(caller: &ProductSurfaceCaller) -> Result<(), ProductSurfaceError> {
    if caller.operator_config {
        Ok(())
    } else {
        Err(ProductSurfaceError::from_status(
            ProductSurfaceErrorCode::Forbidden,
            403,
            false,
        ))
    }
}

async fn owned(
    engine: &Engine,
    raw: &str,
    scope: &ThreadScope,
) -> Result<ThreadRecord, OrchestratorError> {
    engine.owned_thread(&ThreadId::new(raw)?, scope).await
}

fn json<T: serde::Serialize>(value: T) -> Result<Value, OrchestratorError> {
    Ok(serde_json::to_value(value)?)
}

/// The views; `None` for any other view id.
pub(super) async fn view(
    engine: &Engine,
    caller: &ProductSurfaceCaller,
    view: &str,
    input: Value,
) -> Option<Result<Value, ProductSurfaceError>> {
    let scope = scope_of(caller);
    let result = if view == THREAD_GIT_VIEW.id {
        async {
            operator_only(caller)?;
            let req: ThreadGitQuery = decode(input)?;
            answer(
                async {
                    let thread = owned(engine, &req.thread_id, &scope).await?;
                    engine.thread_git(&thread, req.read).await
                }
                .await,
            )
        }
        .await
    } else if view == THREAD_CONSOLE_VIEW.id {
        async {
            operator_only(caller)?;
            let req: ThreadConsoleQuery = decode(input)?;
            answer(
                async {
                    let thread = owned(engine, &req.thread_id, &scope).await?;
                    json(engine.thread_console(&thread, req.after).await?)
                }
                .await,
            )
        }
        .await
    } else {
        return None;
    };
    Some(result)
}

/// The commands; `None` for any other operation id.
pub(super) async fn command(
    engine: &Engine,
    caller: &ProductSurfaceCaller,
    operation: &str,
    input: Value,
) -> Option<Result<Value, ProductSurfaceError>> {
    let scope = scope_of(caller);
    let result = match operation {
        THREAD_GIT_CHANGE_COMMAND_ID => {
            async {
                operator_only(caller)?;
                let req: ThreadGitChange = decode(input)?;
                answer(
                    async {
                        let thread = owned(engine, &req.thread_id, &scope).await?;
                        engine.thread_git_change(&thread, req.action).await
                    }
                    .await,
                )
            }
            .await
        }
        THREAD_CONSOLE_CONTROL_COMMAND_ID => {
            async {
                operator_only(caller)?;
                let req: ThreadConsoleControl = decode(input)?;
                answer(
                    async {
                        let thread = owned(engine, &req.thread_id, &scope).await?;
                        json(engine.thread_console_control(&thread, req.action).await?)
                    }
                    .await,
                )
            }
            .await
        }
        _ => return None,
    };
    Some(result)
}
