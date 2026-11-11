use super::*;

pub(super) fn is_process_sandbox_capability(capability_id: &CapabilityId) -> bool {
    capability_id.as_str() == ironclaw_host_api::capability::PROCESS_SANDBOX_CAPABILITY_ID
}

pub(super) fn provider_schema_is_resolved(schema: &serde_json::Value) -> bool {
    // The hot capability catalog owns canonical JSON Schema validation and the
    // selected LLM provider owns wire-format shaping (including top-level
    // `oneOf` flattening). The loop must not maintain a second, narrower schema
    // dialect: doing so silently delists valid, versioned extension schemas
    // before they reach the provider adapter. It only enforces the boundary the
    // provider cannot resolve safely on its own.
    !schema_contains_external_ref(schema, 0)
}

pub(super) fn provider_tool_name(
    capability_id: &CapabilityId,
    existing: &HashMap<ProviderToolName, CapabilityId>,
) -> ProviderToolName {
    let base = provider_tool_name_base(capability_id.as_str());
    if let Ok(name) = ProviderToolName::new(base.clone())
        && existing
            .get(&name)
            .is_none_or(|existing_id| existing_id == capability_id)
    {
        return name;
    }
    provider_tool_name_with_digest(&base, capability_id.as_str(), existing, 0)
}

pub(super) fn provider_tool_name_with_digest(
    base: &str,
    capability_id: &str,
    existing: &HashMap<ProviderToolName, CapabilityId>,
    attempt: u16,
) -> ProviderToolName {
    let digest_input = if attempt == 0 {
        capability_id.to_string()
    } else {
        format!("{capability_id}#{attempt}")
    };
    let digest = sha256_digest_token(digest_input.as_bytes());
    let suffix = digest.strip_prefix("sha256:").unwrap_or(&digest);
    let suffix = &suffix[..PROVIDER_TOOL_NAME_DIGEST_BYTES]; // safety: sha256 hex digest is ASCII and longer than the fixed suffix.
    let prefix_len = PROVIDER_TOOL_NAME_MAX_BYTES.saturating_sub("__".len() + suffix.len());
    let prefix = if base.len() <= prefix_len {
        base
    } else {
        let prefix_end = base
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= prefix_len)
            .last()
            .unwrap_or(0);
        &base[..prefix_end] // safety: prefix_end comes from char_indices(), so it is a UTF-8 boundary.
    };
    let candidate = format!("{prefix}__{suffix}");
    let candidate = ProviderToolName::new(candidate)
        .expect("provider tool name generator must produce provider-safe names"); // safety: `prefix` is sanitized and `suffix` is a fixed ASCII hex digest slice.
    if existing
        .get(&candidate)
        .is_none_or(|existing_id| existing_id.as_str() == capability_id)
        || attempt == u16::MAX
    {
        return candidate;
    }
    provider_tool_name_with_digest(base, capability_id, existing, attempt + 1)
}

pub(super) fn provider_tool_name_base(capability_id: &str) -> String {
    let mut name = String::with_capacity(capability_id.len());
    for character in capability_id.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
            name.push(character);
        } else if character == '.' {
            name.push_str("__");
        } else {
            name.push('_');
        }
    }
    if name.is_empty() {
        "tool".to_string()
    } else {
        name
    }
}

pub(super) fn should_retry_result_write(
    outcome: &RuntimeCapabilityOutcome,
    result: &Result<GatedResolution, AgentLoopHostError>,
) -> bool {
    matches!(outcome, RuntimeCapabilityOutcome::Completed(_))
        && matches!(
            result,
            Err(error)
                if matches!(
                    error.kind,
                    AgentLoopHostErrorKind::Unavailable
                        | AgentLoopHostErrorKind::TranscriptWriteFailed
                )
        )
}
