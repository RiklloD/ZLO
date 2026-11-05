//! Reject-with-feedback ("no, do X instead") wire contract: the feedback is
//! bounded and validated on the wire, and every row persisted before it
//! existed still loads.

use ironclaw_host_api::{
    audit::{ApprovalDecisionKind, DecisionSummary},
    turn::{DENIAL_FEEDBACK_MAX_CHARS, DenialFeedback, DenialFeedbackError, GateResumeDisposition},
};
use serde_json::json;

#[test]
fn legacy_denied_disposition_still_loads_and_serializes_unchanged() {
    let legacy: GateResumeDisposition = serde_json::from_value(json!("denied")).unwrap();
    assert_eq!(legacy, GateResumeDisposition::denied());
    assert_eq!(legacy.denial_feedback(), None);
    // A denial without feedback keeps the exact old wire value, so older
    // readers of new rows are unaffected too.
    assert_eq!(serde_json::to_value(&legacy).unwrap(), json!("denied"));
    assert_eq!(legacy.as_str(), "denied");
}

#[test]
fn denied_disposition_with_feedback_round_trips() {
    let disposition = GateResumeDisposition::Denied {
        feedback: Some(DenialFeedback::new("use the staging bucket instead").unwrap()),
    };
    let wire = serde_json::to_value(&disposition).unwrap();
    assert_eq!(
        wire,
        json!({"denied": {"feedback": "use the staging bucket instead"}})
    );
    let back: GateResumeDisposition = serde_json::from_value(wire).unwrap();
    assert_eq!(back, disposition);
    assert_eq!(
        back.denial_feedback().map(DenialFeedback::as_str),
        Some("use the staging bucket instead")
    );
}

#[test]
fn disposition_rejects_unknown_variants_and_invalid_feedback() {
    assert!(serde_json::from_value::<GateResumeDisposition>(json!("approved")).is_err());
    assert!(
        serde_json::from_value::<GateResumeDisposition>(json!({"denied": {"feedback": "   "}}))
            .is_err(),
        "feedback is validated on the wire, not only at construction"
    );
}

#[test]
fn denial_feedback_is_trimmed_bounded_and_control_free() {
    assert_eq!(DenialFeedback::new("  try X \n").unwrap().as_str(), "try X");
    assert_eq!(
        DenialFeedback::new(" \n\t "),
        Err(DenialFeedbackError::Empty)
    );
    assert_eq!(
        DenialFeedback::new("é".repeat(DENIAL_FEEDBACK_MAX_CHARS))
            .map(|f| f.as_str().chars().count()),
        Ok(DENIAL_FEEDBACK_MAX_CHARS),
        "the bound counts characters, not bytes"
    );
    assert_eq!(
        DenialFeedback::new("x".repeat(DENIAL_FEEDBACK_MAX_CHARS + 1)),
        Err(DenialFeedbackError::TooLong)
    );
    assert_eq!(
        DenialFeedback::new("line one\nline\ttwo").unwrap().as_str(),
        "line one\nline\ttwo"
    );
    assert_eq!(
        DenialFeedback::new("bell\u{7}"),
        Err(DenialFeedbackError::ControlCharacter)
    );
}

#[test]
fn audit_decision_rows_without_feedback_still_load() {
    let old: DecisionSummary =
        serde_json::from_value(json!({"kind": "denied", "reason": null})).unwrap();
    assert_eq!(old.feedback, None);
    let wire = serde_json::to_value(&old).unwrap();
    assert!(
        wire.get("feedback").is_none(),
        "absent feedback is not written: {wire}"
    );
    let with: DecisionSummary = serde_json::from_value(
        json!({"kind": ApprovalDecisionKind::Denied.as_wire_str(), "reason": null, "feedback": "do Y"}),
    )
    .unwrap();
    assert_eq!(
        with.feedback.as_ref().map(DenialFeedback::as_str),
        Some("do Y")
    );
}
