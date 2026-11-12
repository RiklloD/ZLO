use super::*;

#[test]
fn map_rig_error_auth_401_unauthorized() {
    let err = map_rig_error("openai", "HTTP error 401 Unauthorized: invalid api key");
    assert!(
        matches!(err, LlmError::AuthFailed { provider } if provider == "openai"),
        "401/invalid api key should map to AuthFailed, got a different variant",
    );
}

#[test]
fn map_rig_error_keeps_provider_and_model_identity_distinct() {
    let error = map_rig_error_for(
        "openai",
        "gpt-5-fixture",
        "HTTP 404: requested model does not exist",
    );
    assert!(matches!(
        error,
        LlmError::ModelNotAvailable { provider, model }
            if provider == "openai" && model == "gpt-5-fixture"
    ));
}

#[test]
fn map_rig_error_auth_invalid_api_key() {
    let err = map_rig_error("anthropic", "Incorrect API key provided");
    assert!(
        matches!(err, LlmError::AuthFailed { provider } if provider == "anthropic"),
        "invalid/incorrect api key should map to AuthFailed",
    );
}

#[test]
fn map_rig_error_context_length_still_wins_over_auth() {
    // A context-length error must never be misread as auth.
    let err = map_rig_error(
        "openai",
        "This model's maximum context length is 128000 tokens.",
    );
    assert!(
        matches!(err, LlmError::ContextLengthExceeded { .. }),
        "context-length error should map to ContextLengthExceeded",
    );
}

/// A number that merely *contains* 401 or 403 is not an auth failure.
///
/// `is_auth_error_message` matched `"401"`/`"403"` with a bare `contains`,
/// so a `Retry-After` of `4013 ms` — a rate limit, the most common
/// transient provider error there is — classified as an auth failure. The
/// run terminated immediately telling the user to fix their API key, for a
/// condition that would have cleared on its own. #6284 WS5.
#[test]
fn a_number_containing_401_is_not_an_auth_failure() {
    for message in [
        "rate limited, retry after 4013 ms",
        "rate limited, retry after 14030 ms",
        "request id req_1403f2a failed",
        "context window exceeded: 24019 tokens",
        "upstream returned 4010",
    ] {
        assert!(
            !is_auth_error_message(message),
            "{message:?} is not an auth failure, but was classified as one — \
                 the run would terminate telling the user to fix their API key"
        );
        // Through the caller, because the predicate is not what the run
        // acts on: `map_rig_error` turns it into the error variant, and
        // `retry::is_retryable` keys off that variant. A predicate fix
        // that failed to change the classification would leave the bug
        // exactly where it was.
        let error = map_rig_error("openai", message);
        assert!(
            !matches!(error, LlmError::AuthFailed { .. }),
            "{message:?} mapped to {error:?}; a transient condition must not \
                 become a terminal auth failure"
        );
        assert!(
            crate::retry::is_retryable(&error),
            "{message:?} mapped to {error:?}, which is not retried — this is \
                 the rate limit the fix exists for, and it would have cleared"
        );
    }
}

/// The real status codes must still be caught.
#[test]
fn a_standalone_401_or_403_is_still_an_auth_failure() {
    for message in [
        "server returned 401",
        "http 403 while calling the model",
        "401",
        "status: 403, body: {}",
        "(401)",
    ] {
        assert!(
            is_auth_error_message(message),
            "{message:?} is a genuine auth failure and must be classified as one"
        );
        // The contract the run depends on: a real 401/403 becomes
        // `AuthFailed`, which is neither retried nor allowed to trip the
        // circuit breaker.
        let error = map_rig_error("openai", message);
        assert!(
            matches!(error, LlmError::AuthFailed { ref provider } if provider == "openai"),
            "{message:?} mapped to {error:?}, not AuthFailed"
        );
        assert!(
            !crate::retry::is_retryable(&error),
            "{message:?} is a bad credential; retrying it cannot help"
        );
    }
}

/// A missing model must not be retried twelve times.
///
/// `LlmError::ModelNotAvailable` had **zero producers**, so a 404 /
/// model-not-found fell into `RequestFailed`, which `retry::is_retryable`
/// treats as retryable. A typo'd or decommissioned model id burned the full
/// retry budget before failing. #6284 WS5.
#[test]
fn a_missing_model_is_not_retried_as_a_transient_failure() {
    for message in [
        "The model `gpt-5-turbo` does not exist",
        "model not found",
        "404 model_not_found",
        "unknown model: claude-99",
    ] {
        let error = map_rig_error("gpt-5-turbo", message);
        assert!(
            matches!(error, LlmError::ModelNotAvailable { .. }),
            "{message:?} must map to ModelNotAvailable, got {error:?}"
        );
        assert!(
            !crate::retry::is_retryable(&error),
            "{message:?} must not be retried — the model will not appear between attempts"
        );
    }
}

/// An unrelated 404 is not a missing model.
#[test]
fn an_unrelated_404_still_falls_through() {
    let error = map_rig_error("gpt-5-turbo", "404 not found: /v1/healthz");
    assert!(
        matches!(error, LlmError::RequestFailed { .. }),
        "an unrelated 404 must not be read as a missing model, got {error:?}"
    );
}

#[test]
fn map_rig_error_unrelated_still_request_failed() {
    let err = map_rig_error("openai", "connection reset by peer");
    assert!(
        matches!(err, LlmError::RequestFailed { provider, .. } if provider == "openai"),
        "unrelated transient error should remain RequestFailed",
    );
}

#[test]
fn is_auth_error_message_conservative() {
    // Positive indicators
    assert!(is_auth_error_message("401 unauthorized"));
    assert!(is_auth_error_message("403 forbidden"));
    assert!(is_auth_error_message("invalid api key"));
    assert!(is_auth_error_message("incorrect api key"));
    assert!(is_auth_error_message("error code: invalid_api_key"));
    assert!(is_auth_error_message("authentication failed"));
    assert!(is_auth_error_message("permission denied"));
    assert!(is_auth_error_message("missing api key"));
    assert!(is_auth_error_message("no api key was provided"));
    // Conservative: a bare "key" or unrelated message must not match.
    assert!(!is_auth_error_message("could not find the key in the map"));
    assert!(!is_auth_error_message("connection reset by peer"));
    assert!(!is_auth_error_message("rate limit exceeded"));
}

#[test]
fn test_map_rig_error_detects_context_length_exceeded() {
    let err = map_rig_error("openai", "Error: context_length_exceeded");
    assert!(
        matches!(err, LlmError::ContextLengthExceeded { .. }),
        "Should detect context_length_exceeded: {err:?}"
    );
}

#[test]
fn test_map_rig_error_detects_maximum_context_length() {
    let err = map_rig_error(
        "openai",
        "This model's maximum context length is 128000 tokens",
    );
    assert!(
        matches!(err, LlmError::ContextLengthExceeded { .. }),
        "Should detect maximum context length: {err:?}"
    );
}

#[test]
fn test_map_rig_error_detects_too_many_tokens() {
    let err = map_rig_error("anthropic", "Request has too many tokens (150000)");
    assert!(
        matches!(err, LlmError::ContextLengthExceeded { .. }),
        "Should detect too many tokens: {err:?}"
    );
}

#[test]
fn test_map_rig_error_detects_payload_too_large() {
    let err = map_rig_error("nearai", "HTTP 413: Payload Too Large");
    assert!(
        matches!(err, LlmError::ContextLengthExceeded { .. }),
        "Should detect payload too large: {err:?}"
    );
}

#[test]
fn test_map_rig_error_detects_longer_than_model_context_length() {
    let err = map_rig_error(
        "nearai",
        "HTTP 400 Bad Request: The input (314325 tokens) is longer than the model's context length (262144 tokens).",
    );
    match err {
        LlmError::ContextLengthExceeded { used, limit } => {
            assert_eq!(used, 314325);
            assert_eq!(limit, 262144);
        }
        other => panic!("Expected ContextLengthExceeded, got: {other:?}"),
    }
}

#[tokio::test]
async fn complete_maps_longer_than_context_error_to_context_length_exceeded() {
    let adapter = RigAdapter::new(FailingCompletionModel, "nearai");
    let err = adapter
        .complete(CompletionRequest::new(vec![ChatMessage::user(
            "read my email",
        )]))
        .await
        .expect_err("context overflow should fail the completion");

    match err {
        LlmError::ContextLengthExceeded { used, limit } => {
            assert_eq!(used, 314325);
            assert_eq!(limit, 262144);
        }
        other => panic!("expected context-length error, got {other:?}"),
    }
}

#[test]
fn test_map_rig_error_bare_413_no_false_positive() {
    // Bare "413" should NOT trigger ContextLengthExceeded — avoids false
    // positives on timestamps ("2026-04-13"), token counts ("used 1413"),
    // and request IDs.
    let err = map_rig_error("nearai", "Rate limit: 413 requests per minute exceeded");
    assert!(
        matches!(err, LlmError::RateLimited { .. }),
        "rate-limit wording should stay typed without treating bare 413 as payload status: {err:?}"
    );

    let err = map_rig_error("nearai", "Error at 2026-04-13T10:00:00Z");
    assert!(
        matches!(err, LlmError::RequestFailed { .. }),
        "413 in timestamp should not be ContextLengthExceeded: {err:?}"
    );
}

#[test]
fn test_map_rig_error_generic_error_is_request_failed() {
    let err = map_rig_error("openai", "Connection refused");
    assert!(
        matches!(err, LlmError::RequestFailed { .. }),
        "Generic error should be RequestFailed: {err:?}"
    );
}

#[test]
fn test_parse_token_counts_openai_format() {
    let msg = "this model's maximum context length is 128000 tokens. however, your messages resulted in 150000 tokens.";
    let (used, limit) = crate::error::parse_context_token_counts(msg);
    assert_eq!(limit, 128000);
    assert_eq!(used, 150000);
}

#[test]
fn test_parse_token_counts_unparseable_returns_zero() {
    let msg = "context_length_exceeded";
    let (used, limit) = crate::error::parse_context_token_counts(msg);
    assert_eq!(used, 0);
    assert_eq!(limit, 0);
}

#[test]
fn test_map_rig_error_extracts_token_counts() {
    let err = map_rig_error(
        "openai",
        "This model's maximum context length is 128000 tokens. However, your messages resulted in 150000 tokens.",
    );
    match err {
        LlmError::ContextLengthExceeded { used, limit } => {
            assert_eq!(limit, 128000);
            assert_eq!(used, 150000);
        }
        other => panic!("Expected ContextLengthExceeded, got: {other:?}"),
    }
}
