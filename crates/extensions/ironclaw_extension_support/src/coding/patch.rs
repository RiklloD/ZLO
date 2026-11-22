use ironclaw_host_api::dispatch::RuntimeDispatchErrorKind;
use serde_json::Value;

use super::{
    CodingCapabilityError,
    hashline::{AnchoredEdit, AnchoredEditError, LineAnchor, fresh_anchors_around},
    input_error, operation_error_with_summary,
    text::ReplaceContentError,
};

const MAX_PATCH_EDITS: usize = 256;

#[derive(Debug, Clone)]
pub(super) struct PatchEdit {
    pub(super) old_string: String,
    pub(super) new_string: String,
}

/// The edits of one `apply_patch` call. The two kinds never mix: text edits
/// are matched by content, anchored edits by `LINE:HASH`, and a call that
/// combined them would have no single coherent "original file" view.
#[derive(Debug, Clone)]
pub(super) enum PatchEdits {
    Text(Vec<PatchEdit>),
    Anchored(Vec<AnchoredEdit>),
}

impl PatchEdits {
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Text(edits) => edits.len(),
            Self::Anchored(edits) => edits.len(),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct ApplyPatchInput {
    pub(super) edits: PatchEdits,
    pub(super) replace_all: bool,
}

/// Parse and validate the whole `apply_patch` input contract.
///
/// The published input schema is a flat object (no top-level
/// `oneOf`/`allOf`), so every cross-field rule it used to encode lives here and
/// fails with a summary naming the rule: single edit XOR `edits`, no active
/// `"null"` placeholder, `replace_all` only with exactly one text edit, and no
/// mixing of text and anchored edits.
pub(super) fn parse_apply_patch_input(
    input: &Value,
) -> Result<ApplyPatchInput, CodingCapabilityError> {
    let replace_all = match input.get("replace_all") {
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(patch_input_error("replace_all must be a boolean")),
        None => false,
    };

    let edits = if let Some(edits_value) = optional_edits_value(input) {
        if optional_sentinel_field(input, "old_string").is_some()
            || optional_sentinel_field(input, "new_string").is_some()
        {
            return Err(patch_input_error(
                "pass either old_string and new_string or edits, not both",
            ));
        }
        parse_patch_edits(edits_value)?
    } else {
        let (Some(old_raw), Some(new_raw)) = (
            input.get("old_string").and_then(Value::as_str),
            input.get("new_string").and_then(Value::as_str),
        ) else {
            return Err(patch_input_error(
                "provide old_string and new_string, or a non-empty edits array",
            ));
        };
        if old_raw == "null" || new_raw == "null" {
            return Err(patch_input_error(
                "old_string and new_string must not be the placeholder null",
            ));
        }
        let old_string = normalize_patch_text(old_raw);
        let new_string = normalize_patch_text(new_raw);
        PatchEdits::Text(vec![validated_patch_edit(old_string, new_string)?])
    };

    if edits.len() > MAX_PATCH_EDITS {
        return Err(patch_input_error(format!(
            "at most {MAX_PATCH_EDITS} edits per call"
        )));
    }
    if replace_all && !matches!(&edits, PatchEdits::Text(text) if text.len() == 1) {
        return Err(patch_input_error(
            "replace_all requires exactly one old_string edit",
        ));
    }

    Ok(ApplyPatchInput { edits, replace_all })
}

fn patch_input_error(rule: impl AsRef<str>) -> CodingCapabilityError {
    CodingCapabilityError::with_safe_summary(
        RuntimeDispatchErrorKind::InputEncode,
        format!("apply_patch input rejected: {}", rule.as_ref()),
    )
}

/// Map an anchored-edit rejection to a model-actionable error. A stale or
/// out-of-range anchor carries fresh anchors for the neighbourhood so the
/// model can retry without another read.
pub(super) fn anchored_edit_error(
    error: AnchoredEditError,
    content: &str,
    safe_path: String,
) -> CodingCapabilityError {
    match error {
        AnchoredEditError::Inverted { edit_index } => patch_input_error(format!(
            "edit {edit_index} has an end_anchor before its anchor"
        )),
        AnchoredEditError::NoChange => input_error(),
        AnchoredEditError::Overlap {
            previous_edit_index,
            current_edit_index,
        } => operation_error_with_summary(format!(
            "apply_patch failed for {safe_path}: anchored edits {previous_edit_index} and {current_edit_index} overlap; merge them into one edit"
        )),
        AnchoredEditError::OutOfRange {
            edit_index,
            anchor,
            total_lines,
        } => operation_error_with_summary(format!(
            "apply_patch failed for {safe_path}: edit {edit_index} anchor {anchor} is past the end of the file, which has {total_lines} lines; current last anchors: {}",
            fresh_anchors_around(content, total_lines)
        )),
        AnchoredEditError::Stale {
            edit_index,
            anchor,
            current,
        } => operation_error_with_summary(format!(
            "apply_patch failed for {safe_path}: edit {edit_index} anchor {anchor} is stale, line {} now hashes {current}; nothing was changed. Fresh anchors: {}",
            anchor.line,
            fresh_anchors_around(content, anchor.line)
        )),
    }
}

pub(super) fn replacement_error(
    error: ReplaceContentError,
    safe_path: String,
    edit_count: usize,
) -> CodingCapabilityError {
    match error {
        ReplaceContentError::EmptyOld
        | ReplaceContentError::InvalidEditCount
        | ReplaceContentError::NoChange => input_error(),
        ReplaceContentError::NotFound { edit_index } => operation_error_with_summary(format!(
            "apply_patch failed for {safe_path}: {} matched 0 times",
            edit_label(edit_index, edit_count)
        )),
        ReplaceContentError::Duplicate {
            edit_index,
            occurrences,
        } => operation_error_with_summary(format!(
            "apply_patch failed for {safe_path}: {} matched {occurrences} times; set replace_all=true or provide a unique old_string",
            edit_label(edit_index, edit_count)
        )),
        ReplaceContentError::Overlap {
            previous_edit_index,
            current_edit_index,
        } => operation_error_with_summary(format!(
            "apply_patch failed for {safe_path}: edits[{previous_edit_index}] and edits[{current_edit_index}] overlap; merge them into one edit"
        )),
    }
}

fn parse_patch_edits(edits_value: &Value) -> Result<PatchEdits, CodingCapabilityError> {
    let parsed;
    let edits_value = if let Some(edits_string) = edits_value.as_str() {
        parsed = serde_json::from_str::<Value>(edits_string).map_err(|error| {
            tracing::debug!(?error, "apply_patch edits string is not valid JSON");
            patch_input_error("edits must be an array")
        })?;
        &parsed
    } else {
        edits_value
    };

    let edits = edits_value
        .as_array()
        .ok_or_else(|| patch_input_error("edits must be an array"))?;
    if edits.is_empty() {
        return Err(patch_input_error("edits must not be empty"));
    }

    if edits.iter().any(|edit| edit.get("anchor").is_some()) {
        if !edits.iter().all(|edit| edit.get("anchor").is_some()) {
            return Err(patch_input_error(
                "do not mix anchored edits with old_string edits in one call",
            ));
        }
        return edits
            .iter()
            .enumerate()
            .map(|(index, edit)| parse_anchored_edit(index, edit))
            .collect::<Result<Vec<_>, _>>()
            .map(PatchEdits::Anchored);
    }

    edits
        .iter()
        .map(parse_patch_edit)
        .collect::<Result<Vec<_>, _>>()
        .map(PatchEdits::Text)
}

fn parse_anchored_edit(index: usize, edit: &Value) -> Result<AnchoredEdit, CodingCapabilityError> {
    if ["old_string", "oldText", "newText"]
        .iter()
        .any(|field| edit.get(*field).is_some())
    {
        return Err(patch_input_error(format!(
            "edit {index} mixes anchor with old_string"
        )));
    }
    let anchor = |field: &str| -> Result<Option<LineAnchor>, CodingCapabilityError> {
        edit.get(field)
            .map(|value| {
                value.as_str().and_then(LineAnchor::parse).ok_or_else(|| {
                    patch_input_error(format!(
                        "edit {index} {field} must be LINE:HASH as shown by read_file with hashline=true, e.g. 42:a3f0"
                    ))
                })
            })
            .transpose()
    };
    let Some(start) = anchor("anchor")? else {
        return Err(input_error());
    };
    let end = anchor("end_anchor")?.unwrap_or(start);
    let new_string = edit
        .get("new_string")
        .and_then(Value::as_str)
        .ok_or_else(|| patch_input_error(format!("edit {index} needs a new_string")))?;
    Ok(AnchoredEdit {
        start,
        end,
        new_string: new_string.to_string(),
    })
}

fn optional_edits_value(input: &Value) -> Option<&Value> {
    optional_sentinel_field(input, "edits")
}

fn optional_sentinel_field<'a>(input: &'a Value, field: &str) -> Option<&'a Value> {
    match input.get(field) {
        Some(Value::Null) => None,
        Some(Value::String(value)) if value == "null" => None,
        value => value,
    }
}

fn parse_patch_edit(edit: &Value) -> Result<PatchEdit, CodingCapabilityError> {
    let snake = patch_edit_from_fields(edit, "old_string", "new_string")?;
    let camel = patch_edit_from_fields(edit, "oldText", "newText")?;
    match (snake, camel) {
        (Some(_), Some(_)) => Err(patch_input_error(
            "an edit uses old_string and new_string or oldText and newText, not both",
        )),
        (Some(edit), None) | (None, Some(edit)) => validated_patch_edit(edit.0, edit.1),
        (None, None) => Err(patch_input_error(
            "every edit needs old_string and new_string",
        )),
    }
}

fn patch_edit_from_fields(
    edit: &Value,
    old_field: &str,
    new_field: &str,
) -> Result<Option<(String, String)>, CodingCapabilityError> {
    let old = edit.get(old_field);
    let new = edit.get(new_field);
    match (old, new) {
        (None, None) => Ok(None),
        (Some(old), Some(new)) => {
            let (Some(old), Some(new)) = (old.as_str(), new.as_str()) else {
                return Err(patch_input_error(format!(
                    "{old_field} and {new_field} must be strings"
                )));
            };
            Ok(Some((normalize_patch_text(old), normalize_patch_text(new))))
        }
        _ => Err(patch_input_error(format!(
            "{old_field} and {new_field} must be given together"
        ))),
    }
}

fn validated_patch_edit(
    old_string: String,
    new_string: String,
) -> Result<PatchEdit, CodingCapabilityError> {
    if old_string.is_empty() {
        return Err(patch_input_error("old_string must not be empty"));
    }
    if old_string == new_string {
        return Err(patch_input_error(
            "old_string and new_string are identical, so the edit changes nothing",
        ));
    }
    Ok(PatchEdit {
        old_string,
        new_string,
    })
}

fn normalize_patch_text(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn edit_label(edit_index: usize, edit_count: usize) -> String {
    if edit_count == 1 {
        "old_string".to_string()
    } else {
        format!("edits[{edit_index}].old_string")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn text_edits(input: &ApplyPatchInput) -> &[PatchEdit] {
        match &input.edits {
            PatchEdits::Text(edits) => edits,
            PatchEdits::Anchored(_) => panic!("expected text edits"),
        }
    }

    /// The flat schema no longer rejects these shapes, so the parser is the
    /// contract: every one fails with an InputEncode summary naming the rule.
    #[test]
    fn parse_apply_patch_input_enforces_the_retired_schema_combinators() {
        let rejected = [
            json!({"path": "/workspace/a", "old_string": "old", "new_string": "new",
                   "edits": [{"old_string": "a", "new_string": "b"}]}),
            json!({"path": "/workspace/a", "old_string": "old",
                   "edits": [{"old_string": "a", "new_string": "b"}]}),
            json!({"path": "/workspace/a", "edits": []}),
            json!({"path": "/workspace/a"}),
            json!({"path": "/workspace/a", "old_string": "old"}),
            json!({"path": "/workspace/a", "replace_all": true, "edits": [
                {"old_string": "a", "new_string": "b"}, {"old_string": "c", "new_string": "d"}]}),
            json!({"path": "/workspace/a", "old_string": "null", "new_string": null,
                   "replace_all": true, "edits": [
                {"old_string": "a", "new_string": "b"}, {"old_string": "c", "new_string": "d"}]}),
            json!({"path": "/workspace/a", "replace_all": true,
                   "edits": [{"anchor": "1:1cd9", "new_string": "x"}]}),
            json!({"path": "/workspace/a", "edits": [
                {"anchor": "1:1cd9", "new_string": "x"}, {"old_string": "c", "new_string": "d"}]}),
            json!({"path": "/workspace/a", "edits": [
                {"anchor": "1:1cd9", "old_string": "c", "new_string": "d"}]}),
            json!({"path": "/workspace/a", "edits": [{"anchor": "line one", "new_string": "x"}]}),
            json!({"path": "/workspace/a", "edits": [{"anchor": "1:1cd9"}]}),
        ];
        for input in rejected {
            let error = parse_apply_patch_input(&input).expect_err("must be rejected");
            assert_eq!(
                error.kind(),
                RuntimeDispatchErrorKind::InputEncode,
                "{input} must be an input error"
            );
            assert!(
                error
                    .safe_summary()
                    .is_some_and(|summary| summary.starts_with("apply_patch input rejected")),
                "{input} must name the violated rule: {:?}",
                error.safe_summary()
            );
        }
    }

    #[test]
    fn parse_apply_patch_input_accepts_anchored_edits() {
        let input = json!({
            "path": "/workspace/a",
            "edits": [
                {"anchor": "3:00ff", "end_anchor": " 5:ABCD ", "new_string": "x"},
                {"anchor": "9:1cd9", "new_string": ""}
            ]
        });
        let parsed = parse_apply_patch_input(&input).expect("anchored edits parse");
        let PatchEdits::Anchored(edits) = parsed.edits else {
            panic!("expected anchored edits");
        };
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].start.to_string(), "3:00ff");
        assert_eq!(edits[0].end.to_string(), "5:abcd");
        assert_eq!(edits[1].start, edits[1].end);
        assert_eq!(edits[1].new_string, "");
    }

    #[test]
    fn parse_apply_patch_input_treats_null_edits_as_absent() {
        for edits in [Value::Null, Value::String("null".to_string())] {
            let input = json!({
                "path": "/workspace/main.txt",
                "old_string": "old",
                "new_string": "new",
                "edits": edits
            });

            let parsed = parse_apply_patch_input(&input).expect("single edit");

            let edits = text_edits(&parsed);
            assert_eq!(edits.len(), 1);
            assert_eq!(edits[0].old_string, "old");
            assert_eq!(edits[0].new_string, "new");
        }
    }

    #[test]
    fn parse_apply_patch_input_rejects_non_boolean_replace_all() {
        let input = json!({
            "path": "/workspace/main.txt",
            "old_string": "old",
            "new_string": "new",
            "replace_all": "true"
        });

        assert!(parse_apply_patch_input(&input).is_err());
    }

    #[test]
    fn parse_apply_patch_input_rejects_active_null_string_placeholders() {
        for (old_string, new_string) in [("null", "new"), ("old", "null")] {
            let input = json!({
                "path": "/workspace/main.txt",
                "old_string": old_string,
                "new_string": new_string
            });

            assert!(parse_apply_patch_input(&input).is_err());
        }
    }

    #[test]
    fn parse_apply_patch_input_rejects_too_many_edits() {
        let edits = (0..=MAX_PATCH_EDITS)
            .map(|index| json!({"old_string": format!("old {index}"), "new_string": "new"}))
            .collect::<Vec<_>>();
        let input = json!({
            "path": "/workspace/main.txt",
            "edits": edits
        });

        assert!(parse_apply_patch_input(&input).is_err());
    }

    #[test]
    fn parse_apply_patch_input_treats_top_level_null_strings_as_absent_for_edits() {
        let input = json!({
            "path": "/workspace/main.txt",
            "old_string": "null",
            "new_string": Value::Null,
            "edits": [
                {"old_string": "old", "new_string": "new"}
            ]
        });

        let parsed = parse_apply_patch_input(&input).expect("multi edit");

        let edits = text_edits(&parsed);
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].old_string, "old");
        assert_eq!(edits[0].new_string, "new");
    }
}
