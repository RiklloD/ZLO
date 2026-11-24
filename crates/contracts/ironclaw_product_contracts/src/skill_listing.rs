//! Wire shapes of skill listing management: how each skill is listed to
//! the models and the person's pins. The policy, ledger and pins are
//! `ironclaw_skills::usage`, which this crate cannot see. A pin holds the
//! listing it forces, so a pinned skill's `listing` is its `pin`.

use serde::{Deserialize, Serialize};

/// How a skill is listed to the model; also the listing a pin forces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebornSkillListingState {
    /// Name and description. Pins were once sent as `always_full`.
    #[serde(alias = "always_full")]
    Full,
    NameOnly,
    Off,
}

/// One skill across every source that offered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebornSkillListingEntry {
    pub name: String,
    /// Uses across every source.
    pub uses: u64,
    /// Unix milliseconds of the latest use; `None` when never used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_ms: Option<u64>,
    pub listing: RebornSkillListingState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<RebornSkillListingState>,
    /// Each source that offered it: `harness` (ZLO's own agent loop) or
    /// `cli:<runtime id>`.
    pub sources: Vec<String>,
}

/// The `skill_listing` view. `managed: false` when this deployment does not
/// manage skill listings (every skill is listed in full, pins cannot be set).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebornSkillListingResponse {
    pub managed: bool,
    pub skills: Vec<RebornSkillListingEntry>,
}

/// Input of `builtin.skill_listing_pin_set`: set (`pin`) or clear
/// (`pin: null`) one skill's pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebornSkillListingPinRequest {
    pub name: String,
    #[serde(default)]
    pub pin: Option<RebornSkillListingState>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client that still sends the old `always_full` pin is read as `full`.
    #[test]
    fn the_old_always_full_pin_still_reads() {
        let request: RebornSkillListingPinRequest =
            serde_json::from_str(r#"{"name":"pdf","pin":"always_full"}"#).unwrap();
        assert_eq!(request.pin, Some(RebornSkillListingState::Full));
        assert_eq!(
            serde_json::to_value(RebornSkillListingState::Full).unwrap(),
            "full"
        );
    }
}
