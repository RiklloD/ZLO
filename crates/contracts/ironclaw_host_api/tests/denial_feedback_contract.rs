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
fn feedback_rides_a_sibling_key_through_the_field_helpers() {
    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct Record {
        #[serde(flatten, with = "ironclaw_host_api::turn::resume_disposition_fields")]
        resume_disposition: Option<GateResumeDisposition>,
    }
    let feedback = DenialFeedback::new("use the staging bucket instead").unwrap();
    let record = Record {
        resume_disposition: Some(GateResumeDisposition::Denied {
            feedback: Some(feedback.clone()),
        }),
    };
    let wire = serde_json::to_value(&record).unwrap();
    assert_eq!(
        wire,
        json!({"auth_resume_disposition": "denied", "resume_denial_feedback": "use the staging bucket instead"})
    );
    assert_eq!(serde_json::from_value::<Record>(wire).unwrap(), record);
    // Absent stays absent; a feedback key without a disposition is no denial.
    assert_eq!(
        serde_json::to_value(Record {
            resume_disposition: None
        })
        .unwrap(),
        json!({})
    );
    assert_eq!(
        serde_json::from_value::<Record>(json!({"resume_denial_feedback": "x"})).unwrap(),
        Record {
            resume_disposition: None
        }
    );
}

#[test]
fn a_bare_disposition_refuses_to_drop_feedback() {
    let with_feedback = GateResumeDisposition::Denied {
        feedback: Some(DenialFeedback::new("do Y").unwrap()),
    };
    assert!(
        serde_json::to_value(&with_feedback).is_err(),
        "feedback outside the field helpers fails loudly instead of vanishing"
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
