//! `t3_delegate` through the loop's synthetic handler: the task records the
//! loop run that launched it, and the result is written under the call's
//! own activity id with a preview naming the task, so the parent's
//! transcript draws the subagent in place of that call.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_host_api::dispatch::CapabilityDisplayOutputPreview;
use ironclaw_host_api::ids::{CapabilityId, InvocationId, TenantId, ThreadId, UserId};
use ironclaw_host_api::turn::{RunProfileId, RunProfileVersion, TurnId, TurnRunId, TurnScope};
use ironclaw_loop_contracts::{
    CapabilityInputRef, CapabilitySurfaceVersion, LoopRequest, LoopRunContext, ResolvedRunProfile,
};
use ironclaw_loop_host::{
    CapabilityResultWrite, CapabilityWriteResult, LoopCapabilityResultWriter,
    SyntheticCapabilityHandler, SyntheticCapabilityInvocation,
};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::model::{RuntimeId, ThreadRecord};
use ironclaw_orchestrator::runtime::{
    CreateThread, CreatedThread, HistoryMessage, RuntimeCapabilities, RuntimeError, SteerHow,
    SteerOutcome, TurnAdmission, TurnInput, TurnRuntime,
};
use ironclaw_turns::{CapabilityActivityId, LoopResultRef};
use serde_json::{Value, json};

use super::OrchestratorToolHandler;

/// The loop runtime: children start, or (`starts: false`) cannot be made.
struct Loop {
    starts: bool,
}

#[async_trait]
impl TurnRuntime for Loop {
    fn id(&self) -> RuntimeId {
        RuntimeId::new("ironclaw").unwrap()
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::default()
    }
    async fn create_thread(&self, request: CreateThread) -> Result<CreatedThread, RuntimeError> {
        if !self.starts {
            return Err(RuntimeError::Unavailable("down".into()));
        }
        Ok(CreatedThread {
            thread_id: ThreadId::new(format!("child-{}", request.idempotency_key.len())).unwrap(),
            native_session_id: None,
            provider: request.provider,
            model: request.model,
        })
    }
    async fn start_turn(
        &self,
        _: &ThreadRecord,
        _: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        Ok(TurnAdmission::Started {
            run_id: Some("child-run".into()),
        })
    }
    async fn steer(
        &self,
        _: &ThreadRecord,
        _: &str,
        _: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        Ok(SteerOutcome::Steered)
    }
    async fn cancel(&self, _: &ThreadRecord, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn history(
        &self,
        _: &ThreadRecord,
        _: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        Ok(Vec::new())
    }
}

/// Everything the handler writes for one call.
#[derive(Default)]
struct Captured(Mutex<Vec<(InvocationId, Value, Option<CapabilityDisplayOutputPreview>)>>);

#[async_trait]
impl LoopCapabilityResultWriter for Captured {
    async fn write_capability_result(
        &self,
        write: CapabilityResultWrite<'_>,
    ) -> Result<CapabilityWriteResult, ironclaw_loop_contracts::AgentLoopHostError> {
        self.0.lock().unwrap().push((
            write.invocation_id,
            write.output.clone(),
            write.display_preview.clone(),
        ));
        Ok(CapabilityWriteResult::without_output_digest(
            LoopResultRef::new("result:delegate-test").unwrap(),
            0,
        ))
    }
}

async fn delegate(starts: bool) -> (Arc<Engine>, TurnRunId, CapabilityActivityId, Arc<Captured>) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    engine.register_runtime(Arc::new(Loop { starts }));
    // The database file outlives the test body's reads.
    std::mem::forget(dir);
    let handler = OrchestratorToolHandler {
        name: "t3_delegate",
        engine: Arc::clone(&engine),
        fallback_user_id: UserId::new("user-1").unwrap(),
    };
    let run = TurnRunId::new();
    let activity = CapabilityActivityId::new();
    let context = LoopRunContext::new(
        TurnScope::new(
            TenantId::new("tenant-1").unwrap(),
            None,
            None,
            ThreadId::new("parent-1").unwrap(),
        ),
        TurnId::new(),
        run,
        ResolvedRunProfile::legacy_compatibility(
            RunProfileId::interactive_default(),
            RunProfileVersion::new(1),
            true,
        ),
    );
    let writer = Arc::new(Captured::default());
    handler
        .invoke(SyntheticCapabilityInvocation {
            run_context: context,
            request: LoopRequest {
                activity_id: activity,
                surface_version: CapabilitySurfaceVersion::new("v1").unwrap(),
                capability_id: CapabilityId::new("builtin.t3_delegate").unwrap(),
                input_ref: CapabilityInputRef::new("input:delegate").unwrap(),
                approval_resume: None,
                auth_resume: None,
            },
            input: json!({"task": "look around", "title": "Look around"}),
            result_writer: Arc::clone(&writer) as Arc<dyn LoopCapabilityResultWriter>,
        })
        .await
        .unwrap();
    (engine, run, activity, writer)
}

#[tokio::test]
async fn a_loop_delegate_call_anchors_its_task_and_names_it_on_its_row() {
    let (engine, run, activity, writer) = delegate(true).await;
    let parent = ThreadId::new("parent-1").unwrap();
    let tasks = engine
        .store()
        .tasks_for_parent(&parent, false)
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].parent_run_id, Some(run.to_string()));
    let written = writer.0.lock().unwrap();
    let (invocation, _, preview) = &written[0];
    assert_eq!(
        invocation.to_string(),
        activity.as_uuid().to_string(),
        "written under the call's own id, as its tool row is keyed"
    );
    let preview = preview.as_ref().expect("a delegate call carries a preview");
    let shown: Value = serde_json::from_str(&preview.output_preview).unwrap();
    assert_eq!(shown["taskId"], tasks[0].id.as_str());
    assert_eq!(shown["ok"], true);
}

#[tokio::test]
async fn a_child_that_cannot_start_still_names_its_task() {
    let (engine, run, _, writer) = delegate(false).await;
    let parent = ThreadId::new("parent-1").unwrap();
    let tasks = engine
        .store()
        .tasks_for_parent(&parent, false)
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].parent_run_id, Some(run.to_string()));
    let written = writer.0.lock().unwrap();
    let (_, output, preview) = &written[0];
    assert_eq!(output["ok"], false, "{output}");
    let shown: Value =
        serde_json::from_str(&preview.as_ref().expect("a preview").output_preview).unwrap();
    assert_eq!(shown["taskId"], tasks[0].id.as_str());
    assert_eq!(shown["ok"], false);
}
