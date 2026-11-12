use super::*;

pub(super) struct VisibleInvocationContextRequest<'a> {
    pub(super) base: &'a ExecutionContext,
    pub(super) run_context: &'a LoopRunContext,
    pub(super) activity_id: CapabilityActivityId,
    pub(super) capability_id: &'a CapabilityId,
    pub(super) capability: &'a RuntimeSurfaceCapabilitySnapshot,
    pub(super) trust: ironclaw_host_api::runtime::TrustClass,
    pub(super) allowed_effects: &'a [EffectKind],
    pub(super) execution_mounts: &'a MountView,
}

pub(super) fn invocation_context_from_visible(
    request: VisibleInvocationContextRequest<'_>,
) -> Result<ExecutionContext, AgentLoopHostError> {
    let mut context =
        auth_decline_context_from_visible(request.base, request.run_context, request.activity_id)?;
    let loop_driver_extension = context.extension_id.clone();
    context.runtime = request.capability.runtime;
    context.trust = request.trust;
    context.grants = invocation_grants_from_visible(
        request.base,
        request.capability_id,
        &loop_driver_extension,
        request.allowed_effects,
    )?;
    // Mount propagation is host-authority only: visible-request contexts must arrive with no
    // caller-supplied mounts, while this invocation context receives the execution mounts that the
    // authority resolver selected for the run and capability dispatch.
    context.mounts = request.execution_mounts.clone();
    context.validate().map_err(|_| {
        AgentLoopHostError::new(
            AgentLoopHostErrorKind::InvalidInvocation,
            "capability execution context is invalid",
        )
    })?;
    Ok(context)
}

/// Reconstruct only the host-sealed identity required to terminalize a denied
/// auth gate. This deliberately does not consult the current capability
/// surface, provider trust, grants, mounts, or input: the admitted invocation's
/// durable `BlockedAuth` record is the authority, and `CapabilityHost` validates
/// its exact scope, actor, activity, and capability before mutation.
pub(super) fn auth_decline_context_from_visible(
    base: &ExecutionContext,
    run_context: &LoopRunContext,
    activity_id: CapabilityActivityId,
) -> Result<ExecutionContext, AgentLoopHostError> {
    let mut context = base.clone();
    context.extension_id = loop_driver_execution_extension_id(run_context)?;
    let invocation_id = InvocationId::from_uuid(activity_id.as_uuid());
    context.invocation_id = invocation_id;
    context.correlation_id = CorrelationId::new();
    context.process_id = None;
    context.parent_process_id = None;
    context.resource_scope.invocation_id = invocation_id;
    // Prompt-visible run identity: tool calls within the same turn-run share
    // it, so run-scoped policy state (e.g. coding read-before-edit) carries
    // across tool calls of one run but never leaks into a later run.
    let run_id = ironclaw_host_api::ids::RunId::from_uuid(run_context.run_id.as_uuid());
    context.run_id = Some(run_id);
    // Authoritative origin (§5.2.1): a tool call inside an agent loop turn-run is
    // model-initiated, so the loop ingress seals `LoopRun`. The kernel would also
    // reconstruct this from `run_id`, but stamping `origin` explicitly makes the
    // loop the authoritative source rather than relying on the compat fallback.
    context.origin = Some(
        match run_context
            .product_context
            .as_ref()
            .map(|product_context| product_context.origin)
        {
            Some(ironclaw_turns::TurnOriginKind::ScheduledTrigger) => {
                InvocationOrigin::ScheduledLoopRun(run_id)
            }
            Some(ironclaw_turns::TurnOriginKind::WebUi)
            | Some(ironclaw_turns::TurnOriginKind::Inbound)
            | None => InvocationOrigin::LoopRun(run_id),
        },
    );
    context.authenticated_actor_user_id = run_context.actor().map(|actor| actor.user_id.clone());
    context.validate().map_err(|_| {
        AgentLoopHostError::new(
            AgentLoopHostErrorKind::InvalidInvocation,
            "capability execution context is invalid",
        )
    })?;
    Ok(context)
}

/// Derives the execution extension id for a loop driver.
///
/// Valid extension ids are preserved as-is. Other loop-driver ids are sanitized into a lowercase
/// slug, truncated to leave room for entropy, and suffixed with a digest fragment so separators,
/// case changes, non-ASCII input, and other slug collisions remain distinct.
pub fn loop_driver_execution_extension_id(
    run_context: &LoopRunContext,
) -> Result<ExtensionId, AgentLoopHostError> {
    let raw = run_context.loop_driver_id.as_str();
    if let Ok(extension_id) = ExtensionId::new(raw) {
        return Ok(extension_id);
    }

    let digest = sha256_digest_token(raw.as_bytes());
    let digest_hex = digest.strip_prefix("sha256:").unwrap_or(&digest);
    let slug = extension_id_slug(raw);
    let prefix_budget = 128usize
        .saturating_sub("loop-driver-".len())
        .saturating_sub("-".len())
        .saturating_sub(16);
    let mut candidate = slug.chars().take(prefix_budget).collect::<String>();
    if candidate.is_empty() {
        candidate.push_str("driver");
    }
    ExtensionId::new(format!("loop-driver-{candidate}-{}", &digest_hex[..16])).map_err(|_| {
        AgentLoopHostError::new(
            AgentLoopHostErrorKind::Internal,
            "loop driver id could not be represented as an execution extension",
        )
    })
}

pub(super) fn extension_id_slug(value: &str) -> String {
    let mut slug = String::new();
    let mut last_separator = false;
    for byte in value.bytes() {
        let next = match byte {
            b'a'..=b'z' | b'0'..=b'9' => {
                last_separator = false;
                byte as char
            }
            b'A'..=b'Z' => {
                last_separator = false;
                byte.to_ascii_lowercase() as char
            }
            b'_' | b'-' => {
                if last_separator {
                    continue;
                }
                last_separator = true;
                '-'
            }
            b'.' => {
                if slug.is_empty() || last_separator {
                    continue;
                }
                last_separator = true;
                '.'
            }
            _ => {
                if last_separator {
                    continue;
                }
                last_separator = true;
                '-'
            }
        };
        slug.push(next);
    }
    while slug.ends_with(['-', '.']) {
        slug.pop();
    }
    if slug
        .as_bytes()
        .first()
        .is_none_or(|first| !(first.is_ascii_lowercase() || first.is_ascii_digit()))
    {
        slug.insert_str(0, "driver");
    }
    slug
}

pub(super) fn invocation_grants_from_visible(
    base: &ExecutionContext,
    capability_id: &CapabilityId,
    loop_driver_extension: &ExtensionId,
    allowed_effects: &[EffectKind],
) -> Result<CapabilitySet, AgentLoopHostError> {
    let mut filtered = CapabilitySet::default();
    for grant in &base.grants.grants {
        if grant.capability != *capability_id {
            continue;
        }
        if !grant_principal_matches_visible_context(&grant.grantee, base, loop_driver_extension)
            || !matches!(grant.issued_by, Principal::HostRuntime)
            || !effects_are_covered(&grant.constraints.allowed_effects, allowed_effects)
        {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::Unauthorized,
                "capability execution context carries an untrusted grant",
            ));
        }
        filtered.grants.push(grant.clone());
    }
    Ok(filtered)
}

pub(super) fn grant_principal_matches_visible_context(
    principal: &Principal,
    context: &ExecutionContext,
    loop_driver_extension: &ExtensionId,
) -> bool {
    match principal {
        Principal::Tenant(id) => id == &context.tenant_id,
        Principal::User(id) => id == &context.user_id,
        Principal::Agent(id) => context.agent_id.as_ref() == Some(id),
        Principal::Project(id) => context.project_id.as_ref() == Some(id),
        Principal::Mission(id) => context.mission_id.as_ref() == Some(id),
        Principal::Thread(id) => context.thread_id.as_ref() == Some(id),
        Principal::Extension(id) => id == loop_driver_extension,
        Principal::HostRuntime | Principal::System(_) => false,
    }
}

pub(super) fn effects_are_covered(required: &[EffectKind], allowed: &[EffectKind]) -> bool {
    required.iter().all(|effect| allowed.contains(effect))
}

pub(super) fn invocation_idempotency_key(
    run_context: &LoopRunContext,
    request: &LoopRequest,
    input_ref: &CapabilityInputRef,
) -> Result<IdempotencyKey, AgentLoopHostError> {
    // Each mode must hash to a distinct key: a colliding key would replay the
    // prior mode's recorded outcome (e.g. an auth re-dispatch receiving the
    // original cached ApprovalRequired gate) instead of dispatching.
    let resume_scope = match (
        request.approval_resume.as_ref(),
        request.auth_resume.as_ref(),
    ) {
        (Some(resume), _) => format!(
            "resume:{}:{}",
            resume.approval_request_id, resume.resume_token
        ),
        (None, Some(auth_resume))
            if matches!(
                auth_resume.disposition,
                Some(ironclaw_turns::GateResumeDisposition::Denied { .. })
            ) =>
        {
            "auth-denied".to_string()
        }
        (None, Some(auth_resume)) => {
            let resume_token = auth_resume
                .resume_token
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "missing".to_string());
            format!(
                "auth-resume:{}:{}",
                auth_resume
                    .prior_approval
                    .as_ref()
                    .map(|pa| pa.approval_request_id.to_string())
                    .unwrap_or_else(|| "none".to_string()),
                resume_token
            )
        }
        (None, None) => "dispatch".to_string(),
    };
    let payload = format!(
        "loop-capability\nrun={}\nsurface={}\ncapability={}\ninput={}\nmode={}",
        run_context.run_id,
        request.surface_version.as_str(),
        request.capability_id.as_str(),
        input_ref.as_str(),
        resume_scope
    );
    IdempotencyKey::new(format!(
        "loop-capability:{}",
        sha256_digest_token(payload.as_bytes())
    ))
    .map_err(host_runtime_error)
}

pub(super) fn auth_decline_idempotency_key(
    run_context: &LoopRunContext,
    activity_id: CapabilityActivityId,
    invocation_id: InvocationId,
    capability_id: &CapabilityId,
) -> Result<IdempotencyKey, AgentLoopHostError> {
    // Auth denial terminalizes an already-admitted durable invocation. Its
    // replay identity must therefore remain stable when the current surface or
    // input reference changes after the invocation entered BlockedAuth.
    let payload = format!(
        "loop-capability-auth-decline\nrun={}\nactivity={}\ninvocation={}\ncapability={}\nmode=auth-denied",
        run_context.run_id,
        activity_id,
        invocation_id,
        capability_id.as_str(),
    );
    IdempotencyKey::new(format!(
        "loop-capability:{}",
        sha256_digest_token(payload.as_bytes())
    ))
    .map_err(host_runtime_error)
}

pub(super) fn provider_tool_call_input_ref(
    run_context: &LoopRunContext,
    tool_call: &ProviderToolCall,
) -> Result<CapabilityInputRef, AgentLoopHostError> {
    let turn_id = tool_call.turn_id.as_deref().ok_or_else(|| {
        AgentLoopHostError::new(
            AgentLoopHostErrorKind::InvalidInvocation,
            "provider tool call is missing a provider turn id",
        )
    })?;
    let arguments = serde_json::to_string(&tool_call.arguments).map_err(|error| {
        let safe_summary = error.to_string();
        crate::raw_agent_loop_host_error(
            "capability_provider_tool_call",
            "serialize_arguments",
            AgentLoopHostErrorKind::InvalidInvocation,
            safe_summary,
            error,
        )
    })?;
    let payload = format!(
        "provider-tool-input\nrun={}\nprovider={}\nmodel={}\nturn={}\ncall={}\ntool={}\narguments={}",
        run_context.run_id,
        tool_call.provider_id,
        tool_call.provider_model_id,
        turn_id,
        tool_call.id,
        tool_call.name,
        arguments
    );
    let digest = sha256_digest_token(payload.as_bytes());
    let digest = digest.strip_prefix("sha256:").unwrap_or(&digest);
    CapabilityInputRef::new(format!("{PROVIDER_TOOL_CALL_INPUT_REF_PREFIX}{digest}")).map_err(
        |_| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::Internal,
                "provider tool-call input ref could not be represented",
            )
        },
    )
}

pub(super) fn is_provider_tool_call_input_ref(input_ref: &CapabilityInputRef) -> bool {
    input_ref
        .as_str()
        .starts_with(PROVIDER_TOOL_CALL_INPUT_REF_PREFIX)
}

pub(super) fn loop_surface_version(
    version: &str,
) -> Result<ironclaw_loop_contracts::CapabilitySurfaceVersion, AgentLoopHostError> {
    ironclaw_loop_contracts::CapabilitySurfaceVersion::new(version).map_err(|_| {
        AgentLoopHostError::new(
            AgentLoopHostErrorKind::Internal,
            "host runtime capability surface version could not be represented",
        )
    })
}
