use super::*;

/// Whether a capability result write must be durably persisted, or the
/// content is already fully delivered to the model inline and only needs
/// best-effort in-memory staging for the current run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DurablePersistence {
    /// Durably persist the result content. The default, and correct choice
    /// for any capability result the model has not already seen in full.
    #[default]
    Persist,
    /// Skip durable persistence. Reserved for outputs that are already
    /// fully model-visible inline (e.g. a `result_read` continuation chunk,
    /// whose bytes are returned directly in the tool observation) — writing
    /// them durably again would mint a redundant record per chunk with no
    /// reader that needs it. Best-effort in-memory staging still happens,
    /// so an immediate re-read from cache can still succeed; a later durable
    /// read against this ref must fail gracefully as unavailable.
    InlineOnly,
}

pub struct CapabilityResultWrite<'a> {
    pub run_context: &'a LoopRunContext,
    pub input_ref: &'a CapabilityInputRef,
    pub invocation_id: InvocationId,
    pub capability_id: &'a CapabilityId,
    pub output: serde_json::Value,
    pub display_preview: Option<CapabilityDisplayOutputPreview>,
    pub durable_persistence: DurablePersistence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityWriteResult {
    pub result_ref: LoopResultRef,
    pub byte_len: u64,
    pub output_digest: Option<ContentDigest>,
    pub model_observation: Option<ModelVisibleToolObservation>,
}

impl CapabilityWriteResult {
    pub fn without_output_digest(result_ref: LoopResultRef, byte_len: u64) -> Self {
        Self {
            result_ref,
            byte_len,
            output_digest: None,
            model_observation: None,
        }
    }

    pub fn from_output(
        result_ref: LoopResultRef,
        byte_len: u64,
        output: &serde_json::Value,
    ) -> Self {
        // The output digest is a best-effort progress hint (consumed by output-aware
        // no-progress detection in a later change). A failure to compute it must NEVER
        // fail an otherwise-successful capability write — degrade to `None` instead.
        let output_digest = match ContentDigest::from_json_value(output) {
            Ok(digest) => Some(digest),
            Err(error) => {
                tracing::debug!(
                    %error,
                    "capability result output digest could not be built; recording result without it"
                );
                None
            }
        };
        Self {
            model_observation: None,
            result_ref,
            byte_len,
            output_digest,
        }
    }
}
