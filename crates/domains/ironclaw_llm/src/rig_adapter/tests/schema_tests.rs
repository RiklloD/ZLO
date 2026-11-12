use super::*;

#[test]
fn test_normalize_schema_strict_passes_through_valid_object_schema() {
    let input = serde_json::json!({
        "type": "object",
        "properties": {
            "query": { "type": "string" }
        },
        "required": ["query"]
    });
    let mut description = "Search the index".to_string();
    let result = normalize_schema_strict(&input, &mut description);

    // Strict-mode normalization runs: additionalProperties forced false
    // and required is set to ALL keys, but the structural shape is
    // unchanged.
    assert_eq!(result["type"], "object");
    assert_eq!(result["additionalProperties"], false);
    assert_eq!(result["required"], serde_json::json!(["query"]));
    assert!(result["properties"]["query"].is_object());
    assert_eq!(
        description, "Search the index",
        "description must be untouched when no flatten happened"
    );
}

#[test]
fn test_normalize_schema_strict_flattens_top_level_oneof() {
    // Mirrors the GitHub Copilot MCP `github` tool shape that triggered
    // the original 400.
    let input = serde_json::json!({
        "type": "object",
        "oneOf": [
            { "properties": { "action": { "const": "create_issue" }, "title": { "type": "string" } } },
            { "properties": { "action": { "const": "list_issues" }, "repo":  { "type": "string" } } }
        ]
    });
    let mut description = "GitHub umbrella tool".to_string();
    let result = normalize_schema_strict(&input, &mut description);

    assert_eq!(result["type"], "object");
    assert!(
        result.get("oneOf").is_none(),
        "top-level oneOf must be removed"
    );
    assert_eq!(result["additionalProperties"], true);
    assert!(result["properties"].is_object());
    assert!(result["required"].as_array().unwrap().is_empty());
    assert!(
        description.contains("Upstream JSON schema"),
        "description must include the advisory hint"
    );
    assert!(
        description.contains("create_issue"),
        "original schema variants must survive in the hint"
    );
}

#[test]
fn test_normalize_schema_strict_flattens_anyof_allof_enum_not() {
    for forbidden in ["anyOf", "allOf", "enum", "not"] {
        let input = serde_json::json!({
            "type": "object",
            forbidden: ["whatever"]
        });
        let mut description = "tool".to_string();
        let result = normalize_schema_strict(&input, &mut description);
        assert!(
            result.get(forbidden).is_none(),
            "top-level {forbidden} must be stripped"
        );
        assert_eq!(result["type"], "object");
        assert_eq!(result["additionalProperties"], true);
    }
}

#[test]
fn test_normalize_schema_strict_hint_is_keyword_aware() {
    // The flatten hint must match the construct that triggered it. The
    // previous one-size-fits-all "pick one variant" hint was correct
    // for oneOf/anyOf but actively misleading for allOf (where the LLM
    // should pass fields from ALL variants), enum (one of the listed
    // values), and not (any object that doesn't match).
    let cases = [
        ("oneOf", "pick ONE variant"),
        ("anyOf", "pick ONE variant"),
        ("allOf", "pass fields from ALL variants"),
        ("enum", "pass one of the listed values"),
        ("not", "does NOT match the constraint"),
    ];
    for (keyword, expected_phrase) in cases {
        let input = serde_json::json!({
            "type": "object",
            keyword: ["whatever"]
        });
        let mut description = "tool".to_string();
        let _ = normalize_schema_strict(&input, &mut description);
        assert!(
            description.contains(expected_phrase),
            "hint for top-level {keyword} must contain `{expected_phrase}`, \
                 got: {description}"
        );
    }
}

#[test]
fn test_normalize_schema_strict_replaces_non_object_top_level_type() {
    // A schema like `{"type": "string"}` is not a valid OpenAI tool
    // parameters object — replace wholesale.
    let input = serde_json::json!({ "type": "string" });
    let mut description = "weird tool".to_string();
    let result = normalize_schema_strict(&input, &mut description);
    assert_eq!(result["type"], "object");
    assert!(result["properties"].is_object());
    assert_eq!(result["additionalProperties"], true);
}

#[test]
fn test_normalize_schema_strict_does_not_flatten_nullable_object_type() {
    // `"type": ["object", "null"]` is valid JSON Schema for a nullable
    // object. Some upstream providers and `make_nullable` produce this
    // form. The previous check only matched `JsonValue::String("object")`
    // and would have flattened this schema, discarding all properties.
    let input = serde_json::json!({
        "type": ["object", "null"],
        "properties": {
            "query": { "type": "string" }
        },
        "required": ["query"]
    });
    let mut description = "nullable tool".to_string();
    let result = normalize_schema_strict(&input, &mut description);

    // Should NOT flatten — the schema is a valid object type.
    assert!(
        result["properties"]["query"].is_object(),
        "properties must be preserved for nullable object type, got: {result}"
    );
    assert_eq!(
        description, "nullable tool",
        "description must be untouched (no flatten hint appended)"
    );
}

/// Regression: OpenAI rejects `"items": true` and missing `items` on
/// array-typed properties with "array schema items is not an object".
/// Schema generators (schemars) produce this for `Vec<serde_json::Value>`.
/// The normalizer must ensure `items` is a JSON Schema object.
#[test]
fn test_normalize_schema_strict_fixes_array_items_not_object() {
    // Case 1: items missing entirely (Vec<Value> → {"type": "array"})
    let input = serde_json::json!({
        "type": "object",
        "properties": {
            "requests": { "type": "array" }
        },
        "required": ["requests"]
    });
    let mut description = "batch".to_string();
    let result = normalize_schema_strict(&input, &mut description);
    assert!(
        result["properties"]["requests"]["items"].is_object(),
        "missing items must be filled with an object: {}",
        result["properties"]["requests"]
    );

    // Case 2: items is boolean true (valid JSON Schema, rejected by OpenAI)
    let input = serde_json::json!({
        "type": "object",
        "properties": {
            "data": { "type": "array", "items": true }
        },
        "required": ["data"]
    });
    let mut description = "bool items".to_string();
    let result = normalize_schema_strict(&input, &mut description);
    assert!(
        result["properties"]["data"]["items"].is_object(),
        "boolean items must be replaced with an object: {}",
        result["properties"]["data"]
    );

    // Case 3: items is already an object (should not be clobbered)
    let input = serde_json::json!({
        "type": "object",
        "properties": {
            "tags": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["tags"]
    });
    let mut description = "ok".to_string();
    let result = normalize_schema_strict(&input, &mut description);
    assert_eq!(
        result["properties"]["tags"]["items"]["type"], "string",
        "well-formed items must be preserved"
    );
}

/// Regression for google_docs_tool: a tagged enum with a variant
/// containing `requests: Vec<serde_json::Value>` produces a top-level
/// `oneOf` (which we flatten) with a nested `{"type": "array"}` property
/// that has no `items`. The flatten path originally short-circuited
/// `normalize_schema_recursive`, so the merged `requests` property kept
/// its bare array schema and OpenAI rejected it with "array schema items
/// is not an object". After the fix, the flatten path normalizes each
/// merged property individually.
#[test]
fn test_normalize_schema_strict_flatten_normalizes_merged_array_items() {
    let input = serde_json::json!({
        "type": "object",
        "oneOf": [
            {
                "properties": {
                    "action": { "const": "batch_update" },
                    "document_id": { "type": "string" },
                    "requests": { "type": "array" }
                },
                "required": ["action", "document_id", "requests"]
            },
            {
                "properties": {
                    "action": { "const": "get" },
                    "document_id": { "type": "string" }
                },
                "required": ["action", "document_id"]
            }
        ]
    });
    let mut description = "docs".to_string();
    let result = normalize_schema_strict(&input, &mut description);

    // The flatten happened (oneOf removed, properties merged).
    assert!(result.get("oneOf").is_none());
    let props = result["properties"].as_object().expect("properties");
    assert!(props.contains_key("requests"));

    // CRITICAL: the merged `requests` array property must have
    // `items` as an object — not missing, not boolean, not null.
    let requests = &result["properties"]["requests"];
    assert_eq!(requests["type"], "array");
    assert!(
        requests["items"].is_object(),
        "merged array property must have items as a JSON Schema object \
             after flatten-path normalization; got: {requests}"
    );
}

#[test]
fn test_normalize_schema_strict_merges_variant_properties() {
    // Top-level oneOf flatten now merges all variants' properties into
    // the envelope so the LLM sees structured field hints instead of
    // an empty `{}`. Mirrors the GitHub Copilot `github` tool shape:
    // each variant declares its own subset of fields keyed by `action`.
    let input = serde_json::json!({
        "type": "object",
        "oneOf": [
            {
                "properties": {
                    "action": { "const": "create_issue" },
                    "title":  { "type": "string" },
                    "body":   { "type": "string" }
                },
                "required": ["action", "title"]
            },
            {
                "properties": {
                    "action": { "const": "list_issues" },
                    "repo":   { "type": "string" },
                    "state":  { "type": "string" }
                },
                "required": ["action", "repo"]
            }
        ]
    });
    let mut description = "github".to_string();
    let result = normalize_schema_strict(&input, &mut description);

    // The flatten happened.
    assert_eq!(result["type"], "object");
    assert!(result.get("oneOf").is_none());
    assert_eq!(result["additionalProperties"], true);
    // Strict-mode `required` is empty so the LLM can mix fields from
    // different variants without failing OpenAI validation.
    assert_eq!(result["required"], serde_json::json!([]));

    // CRITICAL: properties is no longer `{}` — every field from every
    // variant must appear so the LLM can pick what to send.
    let props = result["properties"].as_object().expect("merged properties");
    assert!(props.contains_key("action"), "discriminator must merge");
    assert!(props.contains_key("title"), "create_issue field must merge");
    assert!(props.contains_key("body"), "create_issue field must merge");
    assert!(props.contains_key("repo"), "list_issues field must merge");
    assert!(props.contains_key("state"), "list_issues field must merge");
    assert_eq!(props.len(), 5);
}

#[test]
fn test_normalize_schema_strict_merge_first_write_wins_on_conflict() {
    // If two variants declare the same field with different schemas,
    // first-write wins. Documented behaviour — the description hint
    // still has the full original schema for ambiguous cases.
    let input = serde_json::json!({
        "type": "object",
        "anyOf": [
            {
                "properties": {
                    "value": { "type": "string", "description": "first" }
                }
            },
            {
                "properties": {
                    "value": { "type": "integer", "description": "second" }
                }
            }
        ]
    });
    let mut description = "ambiguous".to_string();
    let result = normalize_schema_strict(&input, &mut description);

    let value_schema = &result["properties"]["value"];
    assert_eq!(value_schema["type"], "string");
    assert_eq!(value_schema["description"], "first");
}

#[test]
fn test_normalize_schema_strict_preserves_nested_oneof() {
    // Nested combinators inside `properties` are FINE for the API. Only
    // the top level is forbidden, so the nested oneOf must survive
    // (its variants get recursively strict-normalized but the union
    // itself is preserved). `filter` is marked required so strict mode
    // doesn't wrap it in an `anyOf` for nullability — that would move
    // the inner oneOf one level deeper and obscure what we're checking.
    let input = serde_json::json!({
        "type": "object",
        "properties": {
            "filter": {
                "oneOf": [
                    { "type": "string" },
                    { "type": "object", "properties": { "regex": { "type": "string" } } }
                ]
            }
        },
        "required": ["filter"]
    });
    let mut description = "search".to_string();
    let result = normalize_schema_strict(&input, &mut description);

    assert_eq!(result["type"], "object");
    // Nested oneOf survives untouched at the same path.
    let nested = &result["properties"]["filter"]["oneOf"];
    assert!(nested.is_array(), "nested oneOf must be preserved");
    assert_eq!(nested.as_array().unwrap().len(), 2);
    // The object variant inside the nested oneOf got strict-mode
    // normalized (additionalProperties: false, all keys required).
    let object_variant = &nested[1];
    assert_eq!(object_variant["type"], "object");
    assert_eq!(object_variant["additionalProperties"], false);
    assert_eq!(description, "search");
}

#[test]
fn test_normalize_schema_strict_truncates_huge_schema_on_char_boundary() {
    // 4KB blob with a multi-byte char near the truncation point. The
    // truncated hint must not panic and must end on a valid char
    // boundary.
    let big_string = "α".repeat(2000); // each `α` is 2 bytes in UTF-8 → 4000 bytes
    let input = serde_json::json!({
        "anyOf": [{ "description": big_string }]
    });
    let mut description = "tool".to_string();
    let result = normalize_schema_strict(&input, &mut description);
    assert!(description.contains("(truncated)"));
    assert_eq!(result["type"], "object");
    assert!(result.get("anyOf").is_none());
}

/// Size-capped serializer: verify the capped writer produces correct
/// output at boundary conditions.
#[test]
fn test_serialize_json_capped_boundary_conditions() {
    // Small schema under the cap: full output, no truncation.
    let small = serde_json::json!({"a": 1});
    let result = serialize_json_capped(&small, 1500).expect("should serialize");
    assert_eq!(result.text, r#"{"a":1}"#);
    assert!(!result.was_truncated);

    // Exactly at the cap: should produce exactly cap bytes (or fewer
    // if the serialized output happens to be shorter).
    let result = serialize_json_capped(&small, 7).expect("should serialize");
    assert_eq!(result.text.len(), 7); // {"a":1} is exactly 7 bytes
    assert!(!result.was_truncated);

    // Over the cap: output is truncated. The JSON will be malformed
    // (cut mid-stream) but that's OK — the caller adds "... (truncated)".
    let result = serialize_json_capped(&small, 4).expect("should serialize");
    assert_eq!(result.text.len(), 4);
    assert_eq!(result.text, r#"{"a""#);
    assert!(result.was_truncated);

    // Cap of 0: empty output.
    let result = serialize_json_capped(&small, 0).expect("should serialize");
    assert!(result.text.is_empty());
    assert!(result.was_truncated);
}

/// Size-capped serializer with multi-MB string values: the cap must
/// bound the allocation even when the schema has few nodes but large
/// string values (the gap the old node-counting approach missed).
#[test]
fn test_serialize_json_capped_large_string_values() {
    let big = serde_json::json!({
        "description": "x".repeat(100_000)
    });
    let result = serialize_json_capped(&big, 1500).expect("should serialize");
    assert!(
        result.text.len() <= 1500,
        "capped serializer must bound output to max_bytes; got {} bytes",
        result.text.len()
    );
    assert!(result.was_truncated);
    // The output should start with valid JSON structure.
    assert!(result.text.starts_with(r#"{"description":""#));
}

#[test]
fn test_serialize_json_capped_reports_multibyte_truncation() {
    let value = serde_json::json!({"description": "α"});
    let result = serialize_json_capped(&value, 17).expect("should serialize");

    assert_eq!(result.text, r#"{"description":""#);
    assert!(result.was_truncated);
}

/// Caller-level regression test: drives `convert_tools` (the rig-based
/// provider entry point) end to end with a GitHub-Copilot-shaped tool
/// definition and asserts the resulting `RigToolDefinition` has a clean
/// top level. This is the test that would have caught the OpenAI-via-rig
/// path regressing the same way the Codex path did.
#[test]
fn test_convert_tools_handles_top_level_oneof_dispatcher() {
    let tools = vec![IronToolDefinition {
        name: "github".to_string(),
        description: "GitHub MCP umbrella tool".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "oneOf": [
                {
                    "properties": {
                        "action": { "const": "create_issue" },
                        "title":  { "type": "string" }
                    },
                    "required": ["action", "title"]
                },
                {
                    "properties": {
                        "action": { "const": "list_issues" },
                        "repo":   { "type": "string" }
                    },
                    "required": ["action", "repo"]
                }
            ]
        }),
    }];
    let converted = convert_tools(&tools);
    assert_eq!(converted.len(), 1);
    let tool = &converted[0];

    assert_eq!(tool.name, "github");
    assert_eq!(tool.parameters["type"], "object");
    assert!(
        tool.parameters.get("oneOf").is_none(),
        "top-level oneOf must not survive into the rig-core ToolDefinition"
    );
    assert_eq!(tool.parameters["additionalProperties"], true);
    assert!(
        tool.description.starts_with("GitHub MCP umbrella tool"),
        "original description must come first"
    );
    assert!(
        tool.description.contains("Upstream JSON schema"),
        "advisory hint must be appended"
    );
    assert!(
        tool.description.contains("create_issue") && tool.description.contains("list_issues"),
        "variant info must be retained in the hint"
    );
}

/// End-to-end regression test using the google_docs_tool's actual schema
/// shape. This tool has a tagged enum (`oneOf`) with a `BatchUpdate`
/// variant containing `requests: Vec<serde_json::Value>` — which
/// produces a bare `{"type": "array"}` with no `items`. The flatten
/// path broke TWICE on this shape:
///
/// 1. The `return schema` short-circuit skipped `normalize_schema_recursive`,
///    so the merged `requests` property kept its bare array (no items).
/// 2. Even after the array-items fix was added to the recursive normalizer,
///    the flatten path still short-circuited before it ran.
///
/// This single test would have caught BOTH bugs. It drives the schema
/// through `normalize_schema_strict` (shared normalizer) AND both
/// consumer paths: `convert_tools` (rig-based providers) and
/// `convert_tool_definition` (codex provider). Asserts:
/// - top-level oneOf is flattened
/// - merged properties include fields from ALL variants
/// - array `items` is an object (not missing/boolean)
/// - nested object properties get strict-mode treatment
/// - output passes `validate_strict_schema` with zero violations
#[test]
fn test_realistic_wasm_schema_survives_normalize_flatten_pipeline() {
    // Actual shape from google_docs_tool: tagged enum with 4 variants.
    // BatchUpdate has `requests: Vec<Value>` (bare array, no items).
    // GetDocument/ReadContent have only string fields.
    // InsertText has a nested object (text_style).
    let wasm_schema = serde_json::json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "const": "get_document" },
                    "document_id": { "type": "string" }
                },
                "required": ["action", "document_id"]
            },
            {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "const": "batch_update" },
                    "document_id": { "type": "string" },
                    "requests": { "type": "array" }
                },
                "required": ["action", "document_id", "requests"]
            },
            {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "const": "insert_text" },
                    "document_id": { "type": "string" },
                    "text": { "type": "string" },
                    "text_style": {
                        "type": "object",
                        "properties": {
                            "bold": { "type": "boolean" },
                            "font_size": { "type": "integer" }
                        }
                    }
                },
                "required": ["action", "document_id", "text"]
            },
            {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "const": "read_content" },
                    "document_id": { "type": "string" }
                },
                "required": ["action", "document_id"]
            }
        ]
    });

    // Path 1: shared normalizer (used by both rig and codex paths)
    let mut description = "Google Docs tool".to_string();
    let normalized = normalize_schema_strict(&wasm_schema, &mut description);

    // Top-level oneOf must be flattened.
    assert!(normalized.get("oneOf").is_none(), "oneOf must be flattened");
    assert_eq!(normalized["type"], "object");
    assert_eq!(normalized["additionalProperties"], true);

    // Merged properties from ALL variants.
    let props = normalized["properties"]
        .as_object()
        .expect("merged properties");
    assert!(props.contains_key("action"), "discriminator");
    assert!(props.contains_key("document_id"), "shared field");
    assert!(props.contains_key("requests"), "BatchUpdate field");
    assert!(props.contains_key("text"), "InsertText field");
    assert!(props.contains_key("text_style"), "InsertText nested obj");

    // CRITICAL: array `items` must be an object (the bug that broke twice).
    let requests = &normalized["properties"]["requests"];
    assert!(
        requests["items"].is_object(),
        "requests array must have items as a JSON Schema object; got: {requests}"
    );

    // Nested object properties should get strict-mode treatment
    // (additionalProperties: false on the text_style sub-object).
    let text_style = &normalized["properties"]["text_style"];
    assert_eq!(text_style["additionalProperties"], false);
    assert!(text_style["properties"]["bold"].is_object());

    // Description hint should contain variant info.
    assert!(
        description.contains("batch_update") || description.contains("get_document"),
        "description must include variant info from the original schema"
    );

    // Path 2: rig-based provider entry point.
    let tools = convert_tools(&[IronToolDefinition {
        name: "google_docs_tool".to_string(),
        description: "Google Docs".to_string(),
        parameters: wasm_schema.clone(),
    }]);
    assert_eq!(tools.len(), 1);
    assert!(
        tools[0].parameters.get("oneOf").is_none(),
        "convert_tools output must not have oneOf"
    );
    assert!(
        tools[0].parameters["properties"]["requests"]["items"].is_object(),
        "convert_tools must normalize array items"
    );
}

/// Deeply nested schema: object → array → object → array (no items).
/// Verifies the recursive normalizer walks the full depth and fixes
/// every array `items` and every nested object's strict-mode fields.
#[test]
fn test_normalize_schema_strict_fixes_deeply_nested_array_items() {
    // All fields marked required so `make_nullable` doesn't wrap
    // types as `["array", "null"]`, keeping the assertions focused
    // on "items get fixed at every nesting depth".
    let input = serde_json::json!({
        "type": "object",
        "properties": {
            "data": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "tags": { "type": "array" },
                        "metadata": {
                            "type": "object",
                            "properties": {
                                "values": { "type": "array" }
                            },
                            "required": ["values"]
                        }
                    },
                    "required": ["tags", "metadata"]
                }
            }
        },
        "required": ["data"]
    });
    let mut description = "nested".to_string();
    let result = normalize_schema_strict(&input, &mut description);

    // Level 1: data.items is an object (was already, should be preserved)
    let data_items = &result["properties"]["data"]["items"];
    assert!(data_items.is_object());

    // Level 2: data.items.properties.tags must get items added
    let tags = &data_items["properties"]["tags"];
    assert_eq!(tags["type"], "array");
    assert!(
        tags["items"].is_object(),
        "deeply nested array must get items: {tags}"
    );

    // Level 3: data.items.properties.metadata.properties.values
    let values = &data_items["properties"]["metadata"]["properties"]["values"];
    assert_eq!(values["type"], "array");
    assert!(
        values["items"].is_object(),
        "3-level deep array must get items: {values}"
    );

    // Nested objects should have additionalProperties: false
    assert_eq!(data_items["additionalProperties"], false);
    assert_eq!(
        data_items["properties"]["metadata"]["additionalProperties"],
        false
    );
}
