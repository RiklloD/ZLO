//! Shared tool-call argument JSON parsing with an explicit error policy,
//! for provider files that decode raw wire bytes.

use serde_json::Value;

/// Parse error returned by [`parse_tool_call_args`].
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub(crate) struct ArgsParseError {
    pub reason: String,
}

/// Fail-loud variant. Returns `Err` on malformed JSON.
///
/// Policy: any valid JSON value (object, array, number, string, null) is
/// accepted — `Ok` is returned. Callers decide whether a non-object shape
/// should be rejected. Only a parse failure produces `Err`.
pub(crate) fn parse_tool_call_args(raw: &str) -> Result<Value, ArgsParseError> {
    serde_json::from_str(raw).map_err(|e| ArgsParseError {
        reason: format!("failed to parse tool-call arguments JSON: {e}"),
    })
}

/// Migration helper: silent-`{}` fallback with error capture.
///
/// Returns `(parsed, None)` on success and `(empty object, Some(reason))` on
/// failure. An empty string is treated as a parse failure.
///
/// **Doc-only deprecation notice:** This function is a migration bridge.
/// It preserves the current silent-`{}` behavior at existing call sites
/// while populating `ToolCall.arguments_parse_error` for future gateway
/// surfacing. Remove once every provider migrates to [`parse_tool_call_args`]
/// AND the gateway reads the `arguments_parse_error` field.
/// Do not rely on this function in new code.
pub(crate) fn parse_tool_call_args_lossy(raw: &str) -> (Value, Option<String>) {
    if raw.is_empty() {
        return (
            Value::Object(serde_json::Map::new()),
            Some("empty arguments string".to_owned()),
        );
    }
    match parse_tool_call_args(raw) {
        Ok(v) => (v, None),
        Err(ArgsParseError { reason }) => (Value::Object(serde_json::Map::new()), Some(reason)),
    }
}

/// Streaming-salvage variant of [`parse_tool_call_args_lossy`].
///
/// Some streaming providers (e.g. NearAI reasoning models like DeepSeek) append
/// a stray token after a complete arguments object (`{...}` + junk). This parses
/// the **first** complete JSON value and ignores any trailing content, then
/// falls back to the same silent-`{}` + captured-`reason` behavior as
/// [`parse_tool_call_args_lossy`] on empty or genuinely malformed input.
///
/// Kept as a separate, explicitly-named helper so this permissive "ignore
/// trailing junk" behavior stays scoped to the streaming path and does **not**
/// loosen the shared, fail-loud [`parse_tool_call_args`] boundary that other
/// providers rely on.
pub(crate) fn parse_tool_call_args_allow_trailing_lossy(raw: &str) -> (Value, Option<String>) {
    // A clean parse (no trailing junk) is handled by the shared lossy helper.
    let lossy = parse_tool_call_args_lossy(raw);
    if lossy.1.is_none() {
        return lossy;
    }
    // The strict parse rejected the input. If it is a valid JSON value followed
    // by a stray trailing token (streaming reasoning models do this), recover the
    // leading value; otherwise keep the lossy `{}` + captured reason.
    match serde_json::Deserializer::from_str(raw)
        .into_iter::<Value>()
        .next()
    {
        Some(Ok(value)) => (value, None),
        _ => lossy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_args_valid_object() {
        let result = parse_tool_call_args(r#"{"x": 1}"#).expect("should parse");
        assert_eq!(result["x"], 1);
    }

    #[test]
    fn parse_args_valid_array_accepted() {
        let result = parse_tool_call_args(r#"[1, 2, 3]"#).expect("arrays are valid JSON");
        assert!(result.is_array());
    }

    #[test]
    fn parse_args_invalid_json_returns_err() {
        let err = parse_tool_call_args(r#"{not valid"#).expect_err("should fail");
        assert!(
            err.reason
                .starts_with("failed to parse tool-call arguments JSON: ")
        );
    }

    #[test]
    fn parse_args_lossy_invalid_json_returns_empty_object_and_error() {
        let (val, err) = parse_tool_call_args_lossy(r#"{not valid"#);
        assert!(val.as_object().expect("should be object").is_empty());
        let reason = err.expect("should have error");
        assert!(reason.starts_with("failed to parse tool-call arguments JSON: "));
    }

    #[test]
    fn parse_args_lossy_empty_string() {
        let (val, err) = parse_tool_call_args_lossy("");
        assert!(val.as_object().expect("should be object").is_empty());
        assert_eq!(
            err.expect("error should be present"),
            "empty arguments string"
        );
    }

    #[test]
    fn parse_args_lossy_valid_returns_no_error() {
        let (val, err) = parse_tool_call_args_lossy(r#"{"a": 2}"#);
        assert_eq!(val["a"], 2);
        assert!(err.is_none());
    }

    #[test]
    fn allow_trailing_lossy_recovers_first_value() {
        // The streaming salvage helper recovers the leading object when a
        // reasoning model appends a stray token after it, instead of dropping
        // the arguments to `{}`.
        let (val, err) = parse_tool_call_args_allow_trailing_lossy(r#"{"query": "test"}trailing"#);
        assert_eq!(val["query"], "test");
        assert!(err.is_none());

        // Genuinely malformed input still fails closed to `{}` + a captured reason.
        let (val, err) = parse_tool_call_args_allow_trailing_lossy(r#"{not valid"#);
        assert!(val.as_object().expect("object").is_empty());
        assert!(err.is_some());
    }

    #[test]
    fn parse_tool_call_args_stays_strict_on_trailing_content() {
        // The shared, fail-loud primitive must NOT accept concatenated or
        // trailing output — only the streaming salvage helper tolerates it.
        // Guards against loosening the canonical boundary for other providers.
        assert!(parse_tool_call_args(r#"{"a": 2} x"#).is_err());
        assert!(parse_tool_call_args(r#"{}{}"#).is_err());
    }

    #[test]
    fn parse_args_empty_string_returns_err() {
        let err = parse_tool_call_args("").expect_err("empty string is not valid JSON");
        assert!(
            err.reason
                .starts_with("failed to parse tool-call arguments JSON: ")
        );
    }

    #[test]
    fn parse_args_valid_primitives_accepted() {
        let result = parse_tool_call_args("null").expect("null is valid JSON");
        assert!(result.is_null());

        let result = parse_tool_call_args("42").expect("number is valid JSON");
        assert_eq!(result, 42);

        let result = parse_tool_call_args(r#""hello""#).expect("string is valid JSON");
        assert_eq!(result, "hello");
    }

    #[test]
    fn parse_args_lossy_non_object_passthrough() {
        let result = parse_tool_call_args_lossy("null");
        assert_eq!(result.0, Value::Null);
        assert!(result.1.is_none());

        let result = parse_tool_call_args_lossy("42");
        assert_eq!(result.0, 42);
        assert!(result.1.is_none());

        let result = parse_tool_call_args_lossy("[1,2,3]");
        assert_eq!(result.0, json!([1, 2, 3]));
        assert!(result.1.is_none());
    }
}
