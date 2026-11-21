//! Wire shapes for skill listing management: how each skill is listed to
//! the model (full, by name only, or not at all), by which source, and the
//! person's pins.
//!
//! The domain vocabulary is `ironclaw_skills::usage`, which this contracts
//! crate cannot see; these are its wire-stable projections, kept separate
//! so the stored document can evolve without changing the API.

use serde::{Deserialize, Serialize};

/// How a skill is listed to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebornSkillListingState {
    Full,
    NameOnly,
    Off,
}

/// Why a skill is listed the way it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebornSkillListingBasis {
    /// Decided from how often the skill is used.
    Auto,
    /// The person pinned it.
    Pinned,
}

/// The person's choice for one skill, across every source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebornSkillPin {
    AlwaysFull,
    NameOnly,
    Off,
}

/// One source's view of a skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebornSkillListingSource {
    /// `harness` (ZLO's own agent loop) or `cli:<runtime id>`.
    pub source: String,
    pub listing: RebornSkillListingState,
    pub basis: RebornSkillListingBasis,
    /// Uses this source counted.
    pub uses: u64,
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
    /// The listing the policy gives it now (the pin's, when pinned).
    pub listing: RebornSkillListingState,
    pub basis: RebornSkillListingBasis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<RebornSkillPin>,
    pub sources: Vec<RebornSkillListingSource>,
}

/// The `skill_listing` view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebornSkillListingResponse {
    /// `false` when this deployment does not manage skill listings (every
    /// skill is then listed in full and pins cannot be set).
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
    pub pin: Option<RebornSkillPin>,
}
