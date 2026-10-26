//! Tool arguments repaired against the tool's own input schema before they
//! reach the server. Weaker models send `"500"` for an integer, `"true"` for
//! a boolean or `"null"` for an optional field they meant to leave out; the
//! server rejects all three. Only those shapes are touched: a value the
//! schema already accepts passes unchanged.

use serde_json::{Map, Value};

/// The JSON types a property schema accepts (`type`, or each `anyOf` arm),
/// plus the string values its enums allow.
fn accepted(schema: &Value) -> (Vec<&str>, Vec<&str>) {
    let arms: Vec<&Value> = match schema.get("anyOf").and_then(Value::as_array) {
        Some(arms) => arms.iter().collect(),
        None => vec![schema],
    };
    let mut types = Vec::new();
    let mut enums = Vec::new();
    for arm in arms {
        match arm.get("type") {
            Some(Value::String(kind)) => types.push(kind.as_str()),
            Some(Value::Array(kinds)) => types.extend(kinds.iter().filter_map(Value::as_str)),
            _ => {}
        }
        if let Some(values) = arm.get("enum").and_then(Value::as_array) {
            enums.extend(values.iter().filter_map(Value::as_str));
        }
    }
    (types, enums)
}

/// A repaired value, or `None` to drop the field.
fn repair(text: &str, schema: &Value, required: bool) -> Option<Value> {
    let (types, enums) = accepted(schema);
    let trimmed = text.trim();
    let free_string = types.contains(&"string") && enums.is_empty();
    if enums.contains(&trimmed) || (free_string && trimmed != "null" && !trimmed.is_empty()) {
        return Some(Value::String(text.to_string()));
    }
    if types.contains(&"integer")
        && let Ok(number) = trimmed.parse::<i64>()
    {
        return Some(Value::from(number));
    }
    if types.contains(&"number")
        && let Ok(number) = trimmed.parse::<f64>()
        && let Some(number) = serde_json::Number::from_f64(number)
    {
        return Some(Value::Number(number));
    }
    if types.contains(&"boolean") {
        match trimmed.to_ascii_lowercase().as_str() {
            "true" => return Some(Value::Bool(true)),
            "false" => return Some(Value::Bool(false)),
            _ => {}
        }
    }
    if !required && (trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null")) {
        return None;
    }
    Some(Value::String(text.to_string()))
}

/// Whether `value` satisfies one schema arm's type, enum and bounds.
fn fits_arm(value: &Value, arm: &Value) -> bool {
    let type_ok = match arm.get("type") {
        Some(Value::String(kind)) => matches_type(value, kind),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .any(|kind| matches_type(value, kind)),
        _ => true,
    };
    let enum_ok = arm
        .get("enum")
        .and_then(Value::as_array)
        .is_none_or(|values| values.contains(value));
    let number = value.as_f64();
    let min_ok = match (number, arm.get("minimum").and_then(Value::as_f64)) {
        (Some(n), Some(min)) => n >= min,
        _ => true,
    };
    let max_ok = match (number, arm.get("maximum").and_then(Value::as_f64)) {
        (Some(n), Some(max)) => n <= max,
        _ => true,
    };
    type_ok && enum_ok && min_ok && max_ok
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

fn fits(value: &Value, schema: &Value) -> bool {
    match schema.get("anyOf").and_then(Value::as_array) {
        Some(arms) => arms.iter().any(|arm| fits_arm(value, arm)),
        None => fits_arm(value, schema),
    }
}

pub(crate) fn coerce(input_schema: &Value, arguments: &mut Value) {
    let Some(object) = arguments.as_object_mut() else {
        return;
    };
    let Some(properties) = input_schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    let required: Vec<&str> = input_schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let repaired: Map<String, Value> = std::mem::take(object)
        .into_iter()
        .filter_map(|(name, value)| {
            let Some(schema) = properties.get(&name) else {
                return Some((name, value));
            };
            let is_required = required.contains(&name.as_str());
            let value = match &value {
                Value::String(text) => repair(text, schema, is_required)?,
                _ => value,
            };
            // An optional field the model filled with a placeholder its own
            // schema rejects (`window_id: 0`) means "not set": leave it out.
            // A required one goes through so the server's error reaches the
            // model.
            (is_required || fits(&value, schema)).then_some((name, value))
        })
        .collect();
    *object = repaired;
}
