//! Drift guard: every failure category the agent loop (safe-summary categories) mints must have a dedicated
//! user-facing summary in `ironclaw_host_api::failure::summary`.
//!
//! Moved here from `ironclaw_assistant`'s failure-explanation tests, which
//! scanned this crate's source through a cross-crate `include_str!` (§11.2.7,
//! `reborn_cross_crate_include_scan.rs`). The scan reads the owning crate's own
//! source now, so the include no longer escapes a crate root.

use ironclaw_host_api::failure::categories::MODEL_CREDENTIALS_UNAVAILABLE_CATEGORY;

const GENERIC_FAILURE_SUMMARY: &str = "The run failed before producing a reply. Retry the run, and contact support if it keeps happening.";

#[test]
fn failure_summary_covers_agent_loop_safe_summary_categories() {
    let expected = [
        (
            "model_transient",
            "The run failed after a temporary model error. Retry the run.",
        ),
        (
            "model_context_overflow",
            "The run failed because the model context was too large. Retry with a shorter request or start a new thread.",
        ),
        (
            "model_content_filtered",
            "The run failed because the model provider filtered the response. Change the request and try again.",
        ),
        (
            "model_unavailable",
            "The run failed because the model provider was unavailable. Check the selected provider and retry the run.",
        ),
        (
            "model_internal",
            "The run failed because the model provider returned an internal error. Retry the run or choose a different provider.",
        ),
        (
            "model_invalid_output",
            "The run failed because the model returned output the runner could not use. Retry the run or choose a different model.",
        ),
        (
            "model_output_truncated",
            "The run failed because the model repeatedly reached its output limit. Retry with a request for a shorter answer or increase the output limit.",
        ),
        (
            "model_stale_request",
            "The run failed because the available tools changed while a model request was in flight. Retry the run.",
        ),
        (
            MODEL_CREDENTIALS_UNAVAILABLE_CATEGORY,
            "The run failed because model credentials or provider configuration are invalid. Check the selected provider's API key and base URL, then try again.",
        ),
        (
            "checkpoint_rejected",
            "The host rejected a checkpoint, so the run stopped before continuing. No model or capability ran from the rejected state. Start a new run. If this repeats, ask an operator to inspect checkpoint storage and run-profile compatibility.",
        ),
        (
            "transcript_write_failed",
            "The run failed while saving transcript output. Retry the run, and contact support if saving still fails.",
        ),
        (
            "capability_transient",
            "The run failed after a temporary tool error. Retry the run.",
        ),
        (
            "capability_permanent",
            "The run failed because a tool reported a permanent error. Change the request or tool configuration and try again.",
        ),
        (
            "capability_input_invalid",
            "The run failed because a tool rejected its input. Retry with a clearer or narrower request.",
        ),
        (
            "capability_operation_failed",
            "The run failed because a tool operation did not complete. Retry the run, and check the tool integration if it keeps happening.",
        ),
        (
            "capability_policy_denied",
            "The run failed because a tool policy denied the requested action. Change the request or permissions and try again.",
        ),
        (
            "capability_unavailable",
            "The run failed because a required tool was unavailable. Retry the run, and check the tool integration if it keeps happening.",
        ),
        (
            "capability_internal",
            "The run failed because a tool returned an internal error. Retry the run, and check the tool integration if it keeps happening.",
        ),
        // The granular `compaction_*` categories are deliberately absent: the
        // agent loop no longer mints them since non-cancellation compaction
        // failures became a deferred-continue path instead of a terminal exit
        // (#5838). `host_api::failure::summary` keeps display support so
        // historical records still render.
    ];

    // Parity guard: the hardcoded table above must stay exhaustive against the
    // agent-loop source that mints these safe-summary categories. Without this,
    // a newly added category would silently fall through to
    // GENERIC_FAILURE_SUMMARY and this test would still pass.
    let source_values = agent_loop_safe_summary_category_values_from_source();
    let expected_values = expected
        .iter()
        .map(|(category, _)| *category)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        source_values, expected_values,
        "agent-loop gained or lost a safe-summary category; update the expected table and its user-facing summary"
    );

    for (category, expected_summary) in expected {
        let summary = ironclaw_host_api::failure::summary::reborn_failure_summary_for_category(
            Some(category),
        );
        assert_eq!(summary, expected_summary, "category {category}");
        assert_ne!(summary, GENERIC_FAILURE_SUMMARY, "category {category}");
    }
}

fn agent_loop_safe_summary_category_values_from_source() -> std::collections::BTreeSet<&'static str>
{
    const MAPPING: &str = include_str!("../src/executor/mapping.rs");
    const PROMPT: &str = include_str!("../src/executor/prompt.rs");
    let mut values = std::collections::BTreeSet::new();
    values.extend(fn_match_arm_string_values(
        MAPPING,
        "capability_error_failure_category",
    ));
    values.extend(fn_match_arm_string_values(
        MAPPING,
        "model_error_failure_category",
    ));
    values.extend(fn_match_arm_string_values(
        PROMPT,
        "compaction_failure_category",
    ));
    values
}

fn fn_match_arm_string_values(
    source: &'static str,
    fn_marker: &str,
) -> std::collections::BTreeSet<&'static str> {
    let mut values = std::collections::BTreeSet::new();
    let mut in_fn = false;
    for line in source.lines() {
        let trimmed = line.trim();
        let is_fn_header = trimmed.starts_with("fn ")
            || trimmed.starts_with("pub fn ")
            || trimmed.starts_with("pub(super) fn ")
            || trimmed.starts_with("pub(crate) fn ");
        if !in_fn {
            if is_fn_header && trimmed.contains(fn_marker) {
                in_fn = true;
            }
            continue;
        }
        if is_fn_header {
            break;
        }
        if trimmed.contains("=> \"")
            && let Some(value) = quoted_value(line)
        {
            values.insert(value);
        }
    }
    values
}

fn quoted_value(line: &'static str) -> Option<&'static str> {
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    Some(&line[start..end])
}
