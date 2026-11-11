use super::*;

/// Maximum number of input issues rendered into a failure display preview.
pub(super) const CAPABILITY_FAILURE_PREVIEW_MAX_ISSUES: usize = 5;
/// Byte budget for the rendered failure summary. Stays well under the display
/// preview's own `CAPABILITY_DISPLAY_SUMMARY_MAX_BYTES` (2 KiB) cap.
pub(super) const CAPABILITY_FAILURE_PREVIEW_MAX_BYTES: usize = 1024;

/// Generic placeholder summaries assigned when a failure carries no
/// host-authored message. Surfacing these adds nothing over the bare error
/// kind, so they are filtered out (`runtime_failure_to_loop` /
/// `runtime_model_visible_failure_to_loop`).
pub(super) const GENERIC_CAPABILITY_FAILURE_SUMMARIES: [&str; 2] = [
    "capability invocation failed",
    "capability authorization denied",
];

/// Render a bounded, host-authored display summary for a failed capability so
/// the per-tool UI preview shows the actual reason instead of the bare error
/// kind.
///
/// Preference order:
/// 1. Structured `InvalidInput` field issues, when present — these carry the
///    most actionable per-field detail. Only schema-derived fields (`path`,
///    `code`, `expected`) are interpolated; `received` echoes raw tool input
///    and is deliberately omitted from any display surface.
/// 2. Otherwise the failure's host-authored `safe_summary` (e.g. a builtin's
///    `"invalid JSON: ..."` message), unless it is one of the generic
///    placeholders that say nothing the kind doesn't.
///
/// Returns `None` when neither is available, so the projection keeps its
/// existing `tool failed: <kind>` fallback.
pub(super) fn failure_display_summary(
    safe_summary: &str,
    detail: &CapabilityFailureDetail,
) -> Option<String> {
    if let CapabilityFailureDetail::InvalidInput { issues } = detail
        && !issues.is_empty()
    {
        let rendered = issues
            .iter()
            .take(CAPABILITY_FAILURE_PREVIEW_MAX_ISSUES)
            .filter_map(render_capability_input_issue)
            .collect::<Vec<_>>()
            .join("; ");
        if !rendered.is_empty() {
            let mut summary = format!("Invalid input: {rendered}");
            if issues.len() > CAPABILITY_FAILURE_PREVIEW_MAX_ISSUES {
                let extra = issues.len() - CAPABILITY_FAILURE_PREVIEW_MAX_ISSUES;
                summary.push_str(&format!(" (+{extra} more)"));
            }
            return Some(
                ironclaw_host_api::dispatch::truncate_capability_display_text(
                    &summary,
                    CAPABILITY_FAILURE_PREVIEW_MAX_BYTES,
                )
                .text,
            );
        }
    }

    let summary = safe_summary.trim();
    if summary.is_empty() || GENERIC_CAPABILITY_FAILURE_SUMMARIES.contains(&summary) {
        return None;
    }
    Some(
        ironclaw_host_api::dispatch::truncate_capability_display_text(
            summary,
            CAPABILITY_FAILURE_PREVIEW_MAX_BYTES,
        )
        .text,
    )
}

pub(super) const CAPABILITY_INPUT_ISSUE_FIELD_MAX_BYTES: usize = 160;

pub(super) fn render_capability_input_issue(issue: &CapabilityInputIssue) -> Option<String> {
    let code = match issue.code {
        DispatchInputIssueCode::MissingRequired => "missing required field",
        DispatchInputIssueCode::UnexpectedField => "unexpected field",
        DispatchInputIssueCode::TypeMismatch => "type mismatch",
        DispatchInputIssueCode::InvalidValue => "invalid value",
    };
    let path = capability_input_issue_display_text(&issue.path)?;
    match issue
        .expected
        .as_deref()
        .and_then(capability_input_issue_display_text)
    {
        Some(expected) if !expected.is_empty() => {
            Some(format!("{path} — {code} (expected {expected})"))
        }
        _ => Some(format!("{path} — {code}")),
    }
}

pub(super) fn capability_input_issue_display_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.chars().any(|character| {
            character == '\0'
                || character.is_control()
                || !character.is_ascii()
                || matches!(
                    character,
                    '{' | '}' | '[' | ']' | '`' | '<' | '>' | '/' | '\\'
                )
        })
        || contains_capability_input_issue_sensitive_marker(trimmed)
    {
        return None;
    }
    Some(
        ironclaw_host_api::dispatch::truncate_capability_display_text(
            trimmed,
            CAPABILITY_INPUT_ISSUE_FIELD_MAX_BYTES,
        )
        .text,
    )
}

pub(super) fn contains_capability_input_issue_sensitive_marker(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let normalized = lower
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>();
    for forbidden in [
        "accesstoken",
        "apikey",
        "authtoken",
        "authorization",
        "bearer",
        "password",
        "passwd",
        "secret",
        "toolinput",
    ] {
        if normalized.contains(forbidden) {
            return true;
        }
    }
    for forbidden in [
        "access token",
        "access_token",
        "api key",
        "api_key",
        "apikey",
        "authorization",
        "bearer",
        "password",
        "passwd",
        "secret",
        "tool input",
        "tool_input",
    ] {
        if lower.contains(forbidden) {
            return true;
        }
    }
    lower
        .split(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '.')
        })
        .any(|token| {
            [
                "sk-",
                "sk-ant-",
                "ghp_",
                "github_pat_",
                "gho_",
                "ghu_",
                "ghs_",
                "ghr_",
                "glpat-",
                "gcp-",
                "ya29.",
                "aiza",
            ]
            .iter()
            .any(|prefix| token.starts_with(prefix))
                || (token.len() >= 16 && (token.starts_with("akia") || token.starts_with("asia")))
        })
}
