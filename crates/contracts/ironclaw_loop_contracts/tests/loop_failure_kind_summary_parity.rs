//! Drift guard: every failure category `LoopFailureKind::as_str` mints must have a dedicated
//! user-facing summary in `ironclaw_host_api::failure::summary`.
//!
//! Moved here from `ironclaw_assistant`'s failure-explanation tests, which
//! scanned this crate's source through a cross-crate `include_str!` (§11.2.7,
//! `reborn_cross_crate_include_scan.rs`). The scan reads the owning crate's own
//! source now, so the include no longer escapes a crate root.

use ironclaw_loop_contracts::LoopFailureKind;

const GENERIC_FAILURE_SUMMARY: &str = "The run failed before producing a reply. Retry the run, and contact support if it keeps happening.";

#[test]
fn failure_summary_covers_every_loop_failure_kind_category() {
    let expected = [
        (
            LoopFailureKind::ModelError.as_str(),
            "The run failed while calling the model. Check the selected model provider and try again.",
        ),
        (
            LoopFailureKind::ContextBuildFailed.as_str(),
            "The run failed while building the model context. Retry the run, and contact support if it keeps happening.",
        ),
        (
            LoopFailureKind::CapabilityProtocolError.as_str(),
            "The run failed because a capability returned an invalid protocol response. Retry the run, and contact support if it keeps happening.",
        ),
        (
            LoopFailureKind::IterationLimit.as_str(),
            "The run stopped after reaching its iteration limit before producing a reply. Retry with a narrower request or increase the limit.",
        ),
        (
            LoopFailureKind::InvalidModelOutput.as_str(),
            "The run failed because the model returned output the runner could not use. Retry the run or choose a different model.",
        ),
        (
            LoopFailureKind::CheckpointRejected.as_str(),
            "The host rejected a checkpoint, so the run stopped before continuing. No model or capability ran from the rejected state. Start a new run. If this repeats, ask an operator to inspect checkpoint storage and run-profile compatibility.",
        ),
        (
            LoopFailureKind::CheckpointUnavailable.as_str(),
            "The run failed because the checkpoint could not be loaded. Retry the run, and contact support if the checkpoint remains unavailable.",
        ),
        (
            LoopFailureKind::TranscriptWriteFailed.as_str(),
            "The run failed while saving transcript output. Retry the run, and contact support if saving still fails.",
        ),
        (
            LoopFailureKind::DriverBug.as_str(),
            "The agent runtime reported an internal error. Retry the run, and contact support if it happens again.",
        ),
        (
            LoopFailureKind::InterruptedUnexpectedly.as_str(),
            "The run stopped unexpectedly before it could finish. Retry the run.",
        ),
        (
            LoopFailureKind::NoProgressDetected.as_str(),
            "The run stopped because it repeated work without making progress. Retry with a clearer instruction or narrower scope.",
        ),
        (
            LoopFailureKind::PolicyDenied.as_str(),
            "The run stopped because a policy denied the requested action. Change the request or permissions and try again.",
        ),
        (
            LoopFailureKind::CompactionUnavailable.as_str(),
            "The run failed because context compaction was unavailable. Retry with a shorter request or start a new thread.",
        ),
        (
            LoopFailureKind::GateNotSupported.as_str(),
            "The run stopped because it needed an approval, sign-in, or resource this run profile cannot surface. Run it from an interactive surface or adjust permissions first.",
        ),
        (
            LoopFailureKind::WallClockLimit.as_str(),
            "The run stopped because it reached its time limit. Retry with a narrower request or a higher time limit.",
        ),
        (
            LoopFailureKind::ModelCallLimit.as_str(),
            "The run stopped because it used up its model-call budget. Retry with a narrower request or a higher budget.",
        ),
        (
            LoopFailureKind::CapabilityInvocationLimit.as_str(),
            "The run stopped because it used up its tool-call budget. Retry with a narrower request or a higher budget.",
        ),
    ];

    let source_values = loop_failure_kind_as_str_values_from_source();
    let expected_values = expected
        .iter()
        .map(|(category, _)| *category)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        source_values, expected_values,
        "LoopFailureKind::as_str gained or lost a category; update the Tier-2 summary table"
    );

    for (category, expected_summary) in expected {
        let summary = ironclaw_host_api::failure::summary::reborn_failure_summary_for_category(
            Some(category),
        );
        assert_eq!(summary, expected_summary, "category {category}");
        assert_ne!(summary, GENERIC_FAILURE_SUMMARY, "category {category}");
        assert!(
            !summary.trim().eq(category),
            "summary must not be a raw failure category"
        );
    }
}

fn loop_failure_kind_as_str_values_from_source() -> std::collections::BTreeSet<&'static str> {
    const SOURCE: &str = include_str!("../src/loop_exit.rs");
    source_match_string_values(SOURCE, "impl LoopFailureKind")
}

fn source_match_string_values(
    source: &'static str,
    impl_header: &str,
) -> std::collections::BTreeSet<&'static str> {
    let mut in_impl = false;
    let mut values = std::collections::BTreeSet::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(impl_header) {
            in_impl = true;
            continue;
        }
        if in_impl && trimmed.starts_with("fn to_sanitized_failure") {
            break;
        }
        if in_impl
            && trimmed.starts_with("Self::")
            && trimmed.contains("=>")
            && let Some(value) = quoted_value(trimmed)
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
