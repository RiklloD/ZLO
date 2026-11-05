//! `builtin.plan_update`: the native agent's working plan. Each call replaces
//! the run's plan; the result carries the plan back as a typed display
//! preview (`output_kind: "agent_plan"`), which the projection streams and
//! the timeline stores, so the desktop draws the checklist live and after a
//! reopen. No side effect beyond the result itself.

use ironclaw_extension_registry::{CapabilityManifest, ExtensionError};
use ironclaw_host_api::agent_plan::{
    AGENT_PLAN_OUTPUT_KIND, AgentPlan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
};
use ironclaw_host_api::capability::{EffectKind, PermissionMode};
use ironclaw_host_api::dispatch::CapabilityDisplayOutputPreview;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::FirstPartyCapabilityError;

use super::{first_party_capability_manifest, input_error, resource_profile};

pub const PLAN_UPDATE_CAPABILITY_ID: &str = "builtin.plan_update";
/// The preview must fit the projection's preview bound whole, or the
/// client could not read it back.
const MAX_PLAN_JSON_BYTES: usize = 12 * 1024;

const DESCRIPTION: &str = include_str!("../../prompts/plan_update.md");

pub(super) fn manifest() -> Result<CapabilityManifest, ExtensionError> {
    first_party_capability_manifest(
        PLAN_UPDATE_CAPABILITY_ID,
        DESCRIPTION.trim(),
        vec![EffectKind::DispatchCapability],
        PermissionMode::Allow,
        resource_profile(),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    entries: Vec<InputEntry>,
    #[serde(default)]
    explanation: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputEntry {
    content: String,
    status: PlanEntryStatus,
    #[serde(default)]
    priority: Option<PlanEntryPriority>,
    #[serde(default)]
    active_form: Option<String>,
}

/// The plan as the model gave it, bounded, plus its display preview.
pub(super) fn dispatch(
    input: &Value,
) -> Result<(Value, Option<CapabilityDisplayOutputPreview>), FirstPartyCapabilityError> {
    let input: Input = serde_json::from_value(input.clone()).map_err(|_| input_error())?;
    let entries = input.entries.into_iter().map(|e| {
        PlanEntry::new(e.content, e.status)
            .with_priority(e.priority)
            .with_active_form(e.active_form)
    });
    let plan = AgentPlan::new(entries, input.explanation, chrono::Utc::now());
    if plan.entries.is_empty() {
        return Err(input_error());
    }
    let encoded = serde_json::to_string(&plan).map_err(|_| input_error())?;
    if encoded.len() > MAX_PLAN_JSON_BYTES {
        return Err(input_error());
    }
    let (done, total) = plan.progress();
    let summary = format!("Plan {done}/{total}");
    let output = json!({
        "updated": true,
        "progress": {"completed": done, "total": total},
        "current": plan.current().map(|e| e.content.clone()),
    });
    let preview = CapabilityDisplayOutputPreview {
        output_summary: Some(summary.clone()),
        output_preview: encoded,
        output_kind: AGENT_PLAN_OUTPUT_KIND.to_string(),
        subtitle: Some(summary),
        truncated: false,
    };
    Ok((output, Some(preview)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_comes_back_as_an_agent_plan_preview() {
        let (output, preview) = dispatch(&json!({
            "entries": [
                {"content": "Read the code", "status": "completed"},
                {"content": "Write the fix", "status": "in_progress", "active_form": "Writing the fix"},
                {"content": "Run the tests", "status": "pending", "priority": "high"}
            ],
            "explanation": "Found the cause"
        }))
        .unwrap();
        assert_eq!(output["progress"], json!({"completed": 1, "total": 3}));
        assert_eq!(output["current"], "Write the fix");
        let preview = preview.unwrap();
        assert_eq!(preview.output_kind, AGENT_PLAN_OUTPUT_KIND);
        let plan: AgentPlan = serde_json::from_str(&preview.output_preview).unwrap();
        assert_eq!(plan.entries[2].priority, Some(PlanEntryPriority::High));
        assert_eq!(plan.explanation.as_deref(), Some("Found the cause"));
    }

    #[test]
    fn empty_unknown_and_oversized_plans_are_refused() {
        assert!(dispatch(&json!({"entries": []})).is_err());
        assert!(dispatch(&json!({"entries": [{"content": "  ", "status": "pending"}]})).is_err());
        assert!(dispatch(&json!({"entries": [{"content": "x", "status": "maybe"}]})).is_err());
        assert!(dispatch(&json!({"entries": [], "extra": 1})).is_err());
        let huge: Vec<Value> = (0..60)
            .map(|i| json!({"content": format!("{i} {}", "y".repeat(400)), "status": "pending"}))
            .collect();
        assert!(dispatch(&json!({"entries": huge})).is_err());
    }
}
