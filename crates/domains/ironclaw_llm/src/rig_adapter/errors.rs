use super::*;

/// Map a rig-core completion error to an appropriate `LlmError` variant.
///
/// Detects context-length / payload-size errors in the error message and maps
/// them to `ContextLengthExceeded` so the dispatcher can trigger compaction
/// instead of retrying the same oversized payload.
#[cfg(test)]
pub(super) fn map_rig_error(model_name: &str, e: impl std::fmt::Display) -> LlmError {
    map_rig_error_for(model_name, model_name, e)
}

pub(super) fn map_rig_error_for(
    provider_id: &str,
    model_name: &str,
    error: impl std::fmt::Display,
) -> LlmError {
    crate::error::map_provider_message_error(provider_id, model_name, error.to_string())
}

/// Detect authentication/authorization failures in a lowercased provider
/// error message.
///
/// Mirrors the shape of [`crate::error::is_context_length_error_message`].
/// Deliberately conservative: matches robust indicators (HTTP 401/403,
/// explicit "invalid api key"/"unauthorized"/"authentication" phrasing) and
/// avoids unrelated substrings such as a bare `"key"`.
/// Whether `code` appears in `lower` as a standalone number rather than as a
/// run of digits inside a larger one.
///
/// `"401"`/`"403"` used to be matched with a bare `contains`, so **any** number
/// containing those digits classified the failure as an auth error: a
/// `Retry-After` of `4013 ms`, a request id ending `1403`, a token count of
/// `24019`. The run then terminated telling the user to fix their API key —
/// for what was usually a rate limit, the single most common transient
/// provider error. #6284 WS5.
#[cfg(test)]
pub(super) fn is_auth_error_message(lower: &str) -> bool {
    crate::error::is_auth_error_message(lower)
}
