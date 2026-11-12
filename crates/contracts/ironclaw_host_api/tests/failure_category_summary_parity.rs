//! Drift guard: every failure category `ironclaw_host_api::failure::categories` declares mints must have a dedicated
//! user-facing summary in `ironclaw_host_api::failure::summary`.
//!
//! Moved here from `ironclaw_assistant`'s failure-explanation tests, which
//! scanned this crate's source through a cross-crate `include_str!` (§11.2.7,
//! `reborn_cross_crate_include_scan.rs`). The scan reads the owning crate's own
//! source now, so the include no longer escapes a crate root.

use ironclaw_host_api::failure::categories::{
    BUDGET_ACCOUNTING_FAILED_CATEGORY, CHECKPOINT_REJECTED_CATEGORY,
    HOST_STAGE_UNAVAILABLE_CAPABILITY_CATEGORY, HOST_STAGE_UNAVAILABLE_CHECKPOINT_CATEGORY,
    HOST_STAGE_UNAVAILABLE_INPUT_CATEGORY, HOST_STAGE_UNAVAILABLE_MODEL_CATEGORY,
    HOST_STAGE_UNAVAILABLE_PROMPT_CATEGORY, HOST_STAGE_UNAVAILABLE_TRANSCRIPT_CATEGORY,
    HOST_STAGE_UNAVAILABLE_UNKNOWN_CATEGORY, MODEL_CREDENTIALS_UNAVAILABLE_CATEGORY,
    MODEL_CREDITS_EXHAUSTED_CATEGORY, MODEL_SPEND_BUDGET_EXHAUSTED_CATEGORY,
    MODEL_STAGE_POLICY_DENIED_CATEGORY, MODEL_STAGE_REQUEST_INVALID_CATEGORY,
    MODEL_STAGE_SCOPE_MISMATCH_CATEGORY, TRANSCRIPT_WRITE_FAILED_CATEGORY,
};

#[test]
fn failure_summary_covers_reborn_failure_category_constants() {
    let expected = [
        (
            MODEL_CREDITS_EXHAUSTED_CATEGORY,
            "The AI provider account is out of credits. Add credits or switch providers and try again.",
        ),
        (
            MODEL_CREDENTIALS_UNAVAILABLE_CATEGORY,
            "The run failed because model credentials or provider configuration are invalid. Check the selected provider's API key and base URL, then try again.",
        ),
        (
            MODEL_SPEND_BUDGET_EXHAUSTED_CATEGORY,
            "The run stopped because its configured model spend budget was exhausted. Increase the budget or start a new run.",
        ),
        (
            BUDGET_ACCOUNTING_FAILED_CATEGORY,
            "The run failed because resource accounting was temporarily unavailable. Retry the run, and contact support if it keeps happening.",
        ),
        (
            TRANSCRIPT_WRITE_FAILED_CATEGORY,
            "The run failed while saving transcript output. Retry the run, and contact support if saving still fails.",
        ),
        (
            CHECKPOINT_REJECTED_CATEGORY,
            "The host rejected a checkpoint, so the run stopped before continuing. No model or capability ran from the rejected state. Start a new run. If this repeats, ask an operator to inspect checkpoint storage and run-profile compatibility.",
        ),
        (
            HOST_STAGE_UNAVAILABLE_PROMPT_CATEGORY,
            "The run failed because the host prompt stage was unavailable. Retry the run, and contact support if it keeps happening.",
        ),
        (
            HOST_STAGE_UNAVAILABLE_MODEL_CATEGORY,
            "The run failed because the host model stage was unavailable. Check the model provider and try again.",
        ),
        (
            HOST_STAGE_UNAVAILABLE_CAPABILITY_CATEGORY,
            "The run failed because the host capability stage was unavailable. Retry the run, and check the tool integration if it keeps happening.",
        ),
        (
            HOST_STAGE_UNAVAILABLE_TRANSCRIPT_CATEGORY,
            "The run failed because the host transcript stage was unavailable. Retry the run, and contact support if saving still fails.",
        ),
        (
            HOST_STAGE_UNAVAILABLE_CHECKPOINT_CATEGORY,
            "The run failed because the host checkpoint stage was unavailable. Retry the run, and contact support if checkpoints remain unavailable.",
        ),
        (
            HOST_STAGE_UNAVAILABLE_INPUT_CATEGORY,
            "The run failed because the host input stage was unavailable. Check the submitted message and try again.",
        ),
        (
            HOST_STAGE_UNAVAILABLE_UNKNOWN_CATEGORY,
            "The run failed because a required host stage was unavailable. Retry the run, and contact support if it keeps happening.",
        ),
        // Permanent model-stage failures. Each says retrying will not help,
        // because these previously inherited the generic "Retry the run"
        // summary — advice that can never work for a refused or malformed
        // request.
        (
            MODEL_STAGE_REQUEST_INVALID_CATEGORY,
            "The model request was rejected as invalid. Retrying it unchanged will not help; the request itself needs to change.",
        ),
        (
            MODEL_STAGE_POLICY_DENIED_CATEGORY,
            "Policy does not permit this model request. Retrying will not help; the policy or the model profile needs to change.",
        ),
        (
            MODEL_STAGE_SCOPE_MISMATCH_CATEGORY,
            "The model request fell outside the granted scope. Retrying will not help; the scope or configuration needs to change.",
        ),
    ];
    let source_values = reborn_failure_category_constant_values_from_source();
    let expected_values = expected
        .iter()
        .map(|(category, _)| *category)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        source_values, expected_values,
        "host_api::failure::categories gained or lost a public category constant; update the Tier-2 summary table"
    );

    for (category, expected_summary) in expected {
        assert_eq!(
            ironclaw_host_api::failure::summary::reborn_failure_summary_for_category(Some(
                category
            )),
            expected_summary,
            "category {category}"
        );
    }
}

fn reborn_failure_category_constant_values_from_source() -> std::collections::BTreeSet<&'static str>
{
    const SOURCE: &str = include_str!("../src/failure/categories.rs");
    SOURCE
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with("pub const ") || !trimmed.contains("_CATEGORY") {
                return None;
            }
            quoted_value(trimmed)
        })
        .collect()
}

fn quoted_value(line: &'static str) -> Option<&'static str> {
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    Some(&line[start..end])
}
