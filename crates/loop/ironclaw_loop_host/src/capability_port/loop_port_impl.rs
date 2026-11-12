use super::*;

#[async_trait]
impl LoopCapabilityPort for HostRuntimeLoopCapabilityPort {
    fn requires_ordered_batch_invocation(&self, _invocations: &[LoopRequest]) -> bool {
        false
    }

    fn tool_definitions(&self) -> Result<Vec<ProviderToolDefinition>, AgentLoopHostError> {
        self.validate_visible_request_scope()?;
        let Some((_, snapshot)) = self.current_snapshot()? else {
            return Ok(Vec::new());
        };
        let mut definitions = Vec::new();
        for (capability_id, capability) in &snapshot.capabilities {
            if let Some(definition) = capability.tool_definition(capability_id)? {
                definitions.push(definition);
            }
        }
        definitions.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(definitions)
    }

    fn provider_tool_call_capability_ids(
        &self,
        tool_call: &ProviderToolCall,
    ) -> Result<ProviderToolCallCapabilityIds, AgentLoopHostError> {
        let prepared = self.prepare_provider_tool_call(tool_call)?;
        Ok(ProviderToolCallCapabilityIds {
            provider_capability_id: prepared.capability_id,
            effective_capability_ids: prepared.effective_capability_ids,
        })
    }

    fn validate_provider_tool_call(
        &self,
        tool_call: &ProviderToolCall,
    ) -> Result<(), AgentLoopHostError> {
        self.prepare_provider_tool_call(tool_call).map(|_| ())
    }

    async fn register_provider_tool_call(
        &self,
        request: RegisterProviderToolCallRequest,
    ) -> Result<ironclaw_loop_contracts::CapabilityCallCandidate, AgentLoopHostError> {
        self.register_provider_tool_call_with_activity(request.tool_call, request.activity_id)
            .await
    }

    async fn visible_capabilities(
        &self,
        _request: VisibleCapabilityRequest,
    ) -> Result<VisibleCapabilitySurface, AgentLoopHostError> {
        self.validate_visible_request_scope()?;
        let runtime_surface = self
            .runtime
            .visible_capabilities(self.visible_request.clone())
            .await
            .map_err(host_runtime_error)?;
        let version = loop_surface_version(runtime_surface.version.as_str())?;
        let mut snapshot = SurfaceSnapshot::with_synthetic_capabilities()?;
        let mut descriptors = runtime_surface
            .capabilities
            .into_iter()
            .map(|capability| {
                let capability_id = capability.descriptor.id.clone();
                if snapshot.capabilities.contains_key(&capability_id) {
                    return Err(AgentLoopHostError::new(
                        AgentLoopHostErrorKind::InvalidInvocation,
                        "host runtime capability id is reserved for a synthetic loop capability",
                    ));
                }
                let provider_tool_name =
                    provider_tool_name(&capability.descriptor.id, &snapshot.provider_names);
                snapshot
                    .provider_names
                    .insert(provider_tool_name.clone(), capability_id.clone());
                snapshot.capabilities.insert(
                    capability_id.clone(),
                    SurfaceCapabilitySnapshot::Runtime(Box::new(
                        RuntimeSurfaceCapabilitySnapshot {
                            provider: capability.descriptor.provider.clone(),
                            runtime: capability.descriptor.runtime,
                            estimate: capability.estimated_resources.clone(),
                            safe_description: capability.descriptor.description.clone(),
                            description_trust: capability.description_trust,
                            parameters_schema: capability.descriptor.parameters_schema.clone(),
                            effects: capability.descriptor.effects.clone(),
                            provider_tool_name,
                        },
                    )),
                );
                Ok(CapabilityDescriptorView {
                    capability_id,
                    provider: Some(capability.descriptor.provider),
                    runtime: capability.descriptor.runtime,
                    safe_name: capability.descriptor.id.as_str().to_string(),
                    safe_description: capability.descriptor.description,
                    description_trust: capability.description_trust,
                    parameters_schema: capability.descriptor.parameters_schema,
                })
            })
            .collect::<Result<Vec<_>, AgentLoopHostError>>()?;
        descriptors.extend(snapshot.synthetic_descriptor_views()?);

        let mut snapshots = lock_mut(&self.snapshots, "capability surface snapshot store")?;
        snapshots.clear();
        snapshots.insert(version.as_str().to_string(), snapshot);
        *lock_mut(
            &self.current_surface_version,
            "capability surface snapshot pointer",
        )? = Some(version.as_str().to_string());

        Ok(VisibleCapabilitySurface {
            version,
            descriptors,
            // Empty = "callable == advertised". A disclosure decorator that narrows
            // the advertised set populates this with the wider reachable catalog.
            callable_capability_ids: None,
        })
    }

    async fn invoke_capability(
        &self,
        request: LoopRequest,
    ) -> Result<Resolution, AgentLoopHostError> {
        // §5.3 Stage 2b (collapse complete): dispatch produces the host_api
        // `Resolution` directly, paired with the durable `GateRecord` its channel
        // renders from (a `GatedResolution`) — mapped ONCE, by construction, so
        // the returned resolution carries the SAME gate ref the record is
        // persisted under. `persist_gate_record_for_mapped` persists that record
        // and returns the resolution to hand back; on a concurrent duplicate it
        // is the OWNER's resolution (whose gate ref the record is under), returned
        // only AFTER its durable save completes (#6287). The idempotency key is
        // derived INSIDE `persist_gate_record_for_mapped`, after dispatch and only
        // for a
        // gate-bearing outcome — its `resume.input_ref` binding is the
        // STORE-derived one (same derivation the dispatch cache uses), so it stays
        // byte-stable and identical to dispatch's (§5.3 Stage 0). Deriving it there
        // (rather than up front) keeps dispatch's own resume identity/activity
        // validation the FIRST error a malformed resume surfaces — a missing/stale
        // resume payload must not pre-empt an `InvalidInvocation` activity mismatch.
        // Chain-boxing: each port delegation is boxed so the stacked
        // decorator chain never compiles into a single oversized poll
        // frame (see reborn_integration_model_recovery stack-overflow).
        let gated = Box::pin(self.invoke_capability_dispatch(request.clone())).await?;
        Box::pin(self.persist_gate_record_for_mapped(&request, gated)).await
    }

    async fn invoke_capability_batch(
        &self,
        request: LoopRequestBatch,
    ) -> Result<ResolutionBatch, AgentLoopHostError> {
        let mut resolutions = Vec::new();
        let mut stopped_on_suspension = false;
        for invocation in request.invocations {
            // `invoke_capability` (the trait method above) persists each gate
            // record at the seam, so the batch inherits per-outcome persistence.
            // Chain-boxing: each port delegation is boxed so the stacked
            // decorator chain never compiles into a single oversized poll
            // frame (see reborn_integration_model_recovery stack-overflow).
            let resolution = Box::pin(self.invoke_capability(invocation)).await?;
            // `parks()`, not `is_suspension()` (H1): a re-entrant gate (`Blocked`)
            // stops the batch too — nothing after a gated invocation can proceed
            // until it is resolved, exactly as parked work does.
            let parks = resolution.parks();
            resolutions.push(resolution);
            if request.stop_on_first_suspension && parks {
                stopped_on_suspension = true;
                break;
            }
        }
        Ok(ResolutionBatch {
            resolutions,
            stopped_on_suspension,
        })
    }
}
