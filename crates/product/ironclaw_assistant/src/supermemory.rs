//! `supermemory.search` and `supermemory.write` as synthetic loop
//! capabilities, offered only to runs Supermemory is on for (key set and a
//! container binding). Those runs also lose the local `ironclaw.memory.*`
//! tools, so Supermemory is their only memory.
//!
//! Search is read-only and ungated, like `ironclaw.memory.search`. Write is
//! declared mutating, so the shared synthetic approval gate decides each call
//! before its handler runs (unless tool settings allow it, the call raises a
//! durable approval request and resumes only with a matching, claimed lease).

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::{
    capability::CapabilityGrant,
    ids::{InvocationId, UserId},
    resolution::Resolution,
};
use ironclaw_loop_contracts::{
    AgentLoopHostError, AgentLoopHostErrorKind, CapabilityProgress, LoopRunContext, resolution,
};
use ironclaw_loop_host::{
    CapabilityResultWrite, DurablePersistence, SyntheticApproval, SyntheticCapability,
    SyntheticCapabilityDescriptor, SyntheticCapabilityEffect, SyntheticCapabilityHandler,
    SyntheticCapabilityInvocation,
};
use ironclaw_supermemory::{
    SUPERMEMORY_SEARCH_CAPABILITY_ID, SUPERMEMORY_SEARCH_DESCRIPTION, SUPERMEMORY_SEARCH_TOOL_NAME,
    SUPERMEMORY_WRITE_CAPABILITY_ID, SUPERMEMORY_WRITE_DESCRIPTION, SUPERMEMORY_WRITE_TOOL_NAME,
    Supermemory, SupermemoryCaller, hides_local_memory_capability,
};
use serde_json::Value;

/// Fixed, interpolation-free approval text shared by the gate record and the
/// loop outcome.
const WRITE_APPROVAL_SUMMARY: &str = "saving to Supermemory requires approval";

fn caller_for_run(run: &LoopRunContext, fallback_user_id: &UserId) -> SupermemoryCaller {
    SupermemoryCaller::from_turn_scope(&run.scope, run.acting_user_id(fallback_user_id))
}

/// The Supermemory service when this run uses it instead of local memory,
/// with the local `ironclaw.memory.*` grants removed from `grants`. `None`
/// (grants untouched) when no service is wired or it is off for the caller.
pub async fn active_for_run<'a>(
    supermemory: Option<&'a Arc<Supermemory>>,
    run: &LoopRunContext,
    fallback_user_id: &UserId,
    grants: &mut Vec<CapabilityGrant>,
) -> Option<&'a Arc<Supermemory>> {
    let supermemory = supermemory?;
    if !supermemory
        .is_active(&caller_for_run(run, fallback_user_id))
        .await
    {
        return None;
    }
    grants.retain(|grant| !hides_local_memory_capability(grant.capability.as_str()));
    Some(supermemory)
}

/// The two Supermemory tools for one run's capability surface.
pub fn supermemory_capabilities(
    supermemory: Arc<Supermemory>,
    fallback_user_id: UserId,
) -> Result<Vec<SyntheticCapability>, AgentLoopHostError> {
    let search = SyntheticCapability::new(
        SyntheticCapabilityDescriptor::new(
            SUPERMEMORY_SEARCH_CAPABILITY_ID,
            SUPERMEMORY_SEARCH_TOOL_NAME,
            SUPERMEMORY_SEARCH_DESCRIPTION,
            ironclaw_supermemory::search_schema(),
            SyntheticCapabilityEffect::ReadOnly,
        )?,
        Arc::new(SearchHandler {
            supermemory: Arc::clone(&supermemory),
            fallback_user_id: fallback_user_id.clone(),
        }),
    );
    let write = SyntheticCapability::new(
        SyntheticCapabilityDescriptor::new(
            SUPERMEMORY_WRITE_CAPABILITY_ID,
            SUPERMEMORY_WRITE_TOOL_NAME,
            SUPERMEMORY_WRITE_DESCRIPTION,
            ironclaw_supermemory::write_schema(),
            SyntheticCapabilityEffect::Mutating(SyntheticApproval::new(
                WRITE_APPROVAL_SUMMARY,
                "Store a memory in Supermemory (primary container)",
            )),
        )?,
        Arc::new(WriteHandler {
            supermemory,
            fallback_user_id,
        }),
    );
    Ok(vec![search, write])
}

fn validate(capability_id: &str, arguments: &Value) -> Result<(), AgentLoopHostError> {
    ironclaw_supermemory::validate_arguments(capability_id, arguments).map_err(|error| {
        AgentLoopHostError::new(AgentLoopHostErrorKind::InvalidInvocation, error.to_string())
    })
}

async fn complete(
    invocation: &SyntheticCapabilityInvocation,
    output: Value,
    name: &str,
) -> Result<Resolution, AgentLoopHostError> {
    let summary = match output.get("ok") {
        Some(Value::Bool(false)) => format!(
            "{name} failed: {}",
            output
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("error")
        ),
        _ => format!("{name} done"),
    };
    let input_ref = invocation
        .request
        .approval_resume
        .as_ref()
        .map(|resume| &resume.input_ref)
        .unwrap_or(&invocation.request.input_ref);
    let write = invocation
        .result_writer
        .write_capability_result(CapabilityResultWrite {
            run_context: &invocation.run_context,
            input_ref,
            invocation_id: InvocationId::new(),
            capability_id: &invocation.request.capability_id,
            output,
            display_preview: None,
            durable_persistence: DurablePersistence::Persist,
        })
        .await?;
    Ok(resolution::completed(
        write.result_ref,
        summary,
        CapabilityProgress::MadeProgress,
        false,
        write.byte_len,
        write.output_digest,
        write.model_observation,
    ))
}

struct SearchHandler {
    supermemory: Arc<Supermemory>,
    fallback_user_id: UserId,
}

#[async_trait]
impl SyntheticCapabilityHandler for SearchHandler {
    fn validate_provider_arguments(&self, arguments: &Value) -> Result<(), AgentLoopHostError> {
        validate(SUPERMEMORY_SEARCH_CAPABILITY_ID, arguments)
    }

    async fn invoke(
        &self,
        invocation: SyntheticCapabilityInvocation,
    ) -> Result<Resolution, AgentLoopHostError> {
        let caller = caller_for_run(&invocation.run_context, &self.fallback_user_id);
        let output =
            ironclaw_supermemory::run_search(&self.supermemory, &caller, &invocation.input).await;
        complete(&invocation, output, SUPERMEMORY_SEARCH_TOOL_NAME).await
    }
}

struct WriteHandler {
    supermemory: Arc<Supermemory>,
    fallback_user_id: UserId,
}

#[async_trait]
impl SyntheticCapabilityHandler for WriteHandler {
    fn validate_provider_arguments(&self, arguments: &Value) -> Result<(), AgentLoopHostError> {
        validate(SUPERMEMORY_WRITE_CAPABILITY_ID, arguments)
    }

    async fn invoke(
        &self,
        invocation: SyntheticCapabilityInvocation,
    ) -> Result<Resolution, AgentLoopHostError> {
        let caller = caller_for_run(&invocation.run_context, &self.fallback_user_id);
        let output =
            ironclaw_supermemory::run_write(&self.supermemory, &caller, &invocation.input).await;
        complete(&invocation, output, SUPERMEMORY_WRITE_TOOL_NAME).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ironclaw_host_api::capability::GrantConstraints;
    use ironclaw_host_api::ids::{CapabilityGrantId, CapabilityId};
    use ironclaw_host_api::ids::{SecretHandle, TenantId, ThreadId};
    use ironclaw_host_api::scope::Principal;
    use ironclaw_loop_contracts::{
        InMemoryRunProfileResolver, RunProfileResolutionRequest, RunProfileResolver,
    };
    use ironclaw_product_contracts::operator_secrets::OperatorSecretValueStore;
    use ironclaw_product_contracts::test_support::fakes::FakeOperatorSecretValueStore;
    use ironclaw_supermemory::{
        SUPERMEMORY_API_KEY_SECRET, StaticSupermemoryBindingSource, SupermemoryClient,
        SupermemoryConfig,
    };
    use ironclaw_turns::{TurnId, TurnRunId, TurnScope};

    async fn supermemory(with_key: bool) -> Arc<Supermemory> {
        let keys = Arc::new(FakeOperatorSecretValueStore::default());
        if with_key {
            keys.put(
                &SecretHandle::new(SUPERMEMORY_API_KEY_SECRET).unwrap(),
                secrecy::SecretString::from("sm_test_key_not_real".to_string()),
            )
            .await
            .unwrap();
        }
        // The activity check reads only the key and binding; it sends nothing.
        let config = SupermemoryConfig::default();
        let client = SupermemoryClient::production(&config, keys).unwrap();
        let bindings = Arc::new(StaticSupermemoryBindingSource::new(Some(
            ironclaw_supermemory::SupermemoryBinding {
                primary_container: "ironclaw".to_string(),
                read_containers: vec!["career".to_string()],
                instructions: None,
            },
        )));
        Arc::new(Supermemory::new(client, bindings, config))
    }

    async fn run() -> LoopRunContext {
        let profile = InMemoryRunProfileResolver::default()
            .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
            .await
            .unwrap();
        LoopRunContext::new(
            TurnScope::new(
                TenantId::new("tenant-sm").unwrap(),
                None,
                None,
                ThreadId::new("thread-sm").unwrap(),
            ),
            TurnId::new(),
            TurnRunId::new(),
            profile,
        )
    }

    fn grant(id: &str) -> CapabilityGrant {
        CapabilityGrant {
            id: CapabilityGrantId::new(),
            capability: CapabilityId::new(id).unwrap(),
            grantee: Principal::HostRuntime,
            issued_by: Principal::HostRuntime,
            constraints: GrantConstraints {
                allowed_effects: Vec::new(),
                mounts: Default::default(),
                network: Default::default(),
                secrets: Vec::new(),
                resource_ceiling: None,
                expires_at: None,
                max_invocations: None,
            },
        }
    }

    fn grants() -> Vec<CapabilityGrant> {
        [
            "ironclaw.memory.write",
            "ironclaw.memory.search",
            "builtin.project_create",
        ]
        .into_iter()
        .map(grant)
        .collect()
    }

    #[tokio::test]
    async fn key_and_binding_swap_local_memory_tools_for_supermemory() {
        let user = UserId::new("user-sm").unwrap();
        let run = run().await;

        let off = supermemory(false).await;
        let mut untouched = grants();
        assert!(
            active_for_run(Some(&off), &run, &user, &mut untouched)
                .await
                .is_none()
        );
        assert_eq!(untouched.len(), 3, "no key: local memory tools stay");
        assert!(
            active_for_run(None, &run, &user, &mut untouched)
                .await
                .is_none()
        );

        let on = supermemory(true).await;
        let mut swapped = grants();
        let active = active_for_run(Some(&on), &run, &user, &mut swapped).await;
        assert!(active.is_some());
        let left: Vec<&str> = swapped
            .iter()
            .map(|grant| grant.capability.as_str())
            .collect();
        assert_eq!(left, ["builtin.project_create"]);
    }
}
