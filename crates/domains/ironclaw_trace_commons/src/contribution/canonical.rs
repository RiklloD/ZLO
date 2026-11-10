//! Content and redaction hashing.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use super::*;

pub(crate) fn canonical_hash(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    format!("sha256:{}", hex::encode(digest))
}

/// Integrity/dedupe digest over the redacted events and their counts.
///
/// Fallible on purpose. `serde_json::to_vec(...).unwrap_or_default()` hashed
/// *zero bytes* on a serialization failure, so every failing trace produced the
/// same well-formed `sha256:…` — a silent collision in the value used for
/// dedupe and integrity, and one that still satisfies the `starts_with("sha256:")`
/// dependability check downstream (#7144). Failing is the only honest answer;
/// both callers can carry it.
pub(crate) fn redaction_hash(
    events: &[TraceContributionEvent],
    counts: &BTreeMap<String, u32>,
) -> Result<String, TraceContributionError> {
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(events).map_err(|error| {
        TraceContributionError::RedactionFailed {
            reason: format!("redacted trace events could not be serialized for hashing: {error}"),
        }
    })?);
    hasher.update(serde_json::to_vec(counts).map_err(|error| {
        TraceContributionError::RedactionFailed {
            reason: format!("redaction counts could not be serialized for hashing: {error}"),
        }
    })?);
    Ok(format!("sha256:{}", hex::encode(hasher.finalize())))
}
