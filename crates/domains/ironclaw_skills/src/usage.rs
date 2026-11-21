//! Skill listing management: which skills a model sees in full, by name
//! only, or not at all, decided from how often the person uses them.
//!
//! Every skill a model can reach costs its description in every turn's
//! context. Skills used a lot stay fully listed; skills rarely used are
//! listed by name only (still callable, almost free); pinned skills follow
//! the person's choice. One policy serves every source that lists skills:
//! ZLO's own harness (the IronClaw agent loop) and each CLI runtime.
//!
//! - [`SkillUsageDocument`] — the per-user record: per [`SkillUsageSource`]
//!   the skills it last offered (its catalog) and how often each was used,
//!   plus the person's [`SkillPin`]s.
//! - [`listing`] — the pure policy over a catalog, a ledger and the pins;
//!   [`summarize`] — every skill across every source, for a settings page.
//! - [`SkillUsageStore`] — the typed store port; [`FilesystemSkillUsageStore`]
//!   keeps the document on the shared persistence plane (`cas_update`).
//!
//! The ledger is advisory: a lost update only ages a skill's ranking, so
//! callers treat store failures as "keep listing as before", never as a
//! failed turn.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use async_trait::async_trait;
use ironclaw_host_api::ids::{TenantId, UserId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod store;

pub use store::{FilesystemSkillUsageStore, SKILL_USAGE_DOCUMENT_PATH};

/// Longest skill name the ledger keeps, in characters.
pub const SKILL_USAGE_NAME_MAX_CHARS: usize = 128;
/// Longest CLI runtime id a source names, in characters.
pub const SKILL_USAGE_RUNTIME_ID_MAX_CHARS: usize = 64;
/// Most skills one source's catalog keeps; the rest of a larger catalog is
/// not managed (and so stays listed as the source lists it).
pub const SKILL_USAGE_CATALOG_MAX: usize = 1024;
/// Most skills one source's ledger counts.
pub const SKILL_USAGE_LEDGER_MAX: usize = 2048;
/// Most sources one document keeps.
pub const SKILL_USAGE_SOURCES_MAX: usize = 32;
/// Most pins one document keeps.
pub const SKILL_USAGE_PINS_MAX: usize = 1024;

const DAY_MS: f64 = 86_400_000.0;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SkillUsageError {
    #[error("invalid skill usage name: {reason}")]
    InvalidName { reason: &'static str },
    #[error("invalid skill usage source: {reason}")]
    InvalidSource { reason: &'static str },
    #[error("skill usage document is full: {reason}")]
    Full { reason: &'static str },
    #[error("skill usage serialization failed: {reason}")]
    Serialization { reason: String },
    #[error("skill usage backend failed: {reason}")]
    Backend { reason: String },
    #[error("skill usage update conflicted past its retries")]
    Conflict,
}

/// A skill's name as its source lists it (`grilling`, `ironclaw:deploy`,
/// `anthropic-skills:pdf`): non-empty, at most
/// [`SKILL_USAGE_NAME_MAX_CHARS`], no whitespace or control characters.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct SkillUsageName(String);

impl SkillUsageName {
    fn validate(s: &str) -> Result<(), SkillUsageError> {
        if s.is_empty() {
            return Err(SkillUsageError::InvalidName { reason: "empty" });
        }
        if s.chars().count() > SKILL_USAGE_NAME_MAX_CHARS {
            return Err(SkillUsageError::InvalidName { reason: "too long" });
        }
        if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(SkillUsageError::InvalidName {
                reason: "whitespace or control character",
            });
        }
        Ok(())
    }

    pub fn new(raw: impl Into<String>) -> Result<Self, SkillUsageError> {
        let s = raw.into();
        Self::validate(&s)?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl TryFrom<String> for SkillUsageName {
    type Error = SkillUsageError;
    fn try_from(value: String) -> Result<Self, SkillUsageError> {
        Self::validate(&value)?;
        Ok(Self(value))
    }
}

impl AsRef<str> for SkillUsageName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SkillUsageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<SkillUsageName> for String {
    fn from(name: SkillUsageName) -> Self {
        name.0
    }
}

/// Who lists the skills: ZLO's own agent loop, or one CLI runtime (by its
/// runtime id, e.g. `claude-cli`, `codex`). Wire form `harness` or
/// `cli:<runtime id>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum SkillUsageSource {
    Harness,
    CliRuntime(String),
}

impl SkillUsageSource {
    const HARNESS: &'static str = "harness";
    const CLI_PREFIX: &'static str = "cli:";

    /// A CLI runtime source: the id is the runtime's own (ASCII letters,
    /// digits, `-`, `_`, `.`), at most [`SKILL_USAGE_RUNTIME_ID_MAX_CHARS`].
    pub fn cli_runtime(runtime_id: impl Into<String>) -> Result<Self, SkillUsageError> {
        let id = runtime_id.into();
        if id.is_empty() || id.len() > SKILL_USAGE_RUNTIME_ID_MAX_CHARS {
            return Err(SkillUsageError::InvalidSource {
                reason: "runtime id length",
            });
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(SkillUsageError::InvalidSource {
                reason: "runtime id character",
            });
        }
        Ok(Self::CliRuntime(id))
    }

    /// The wire form (`harness`, `cli:<id>`), also what a UI shows.
    pub fn wire(&self) -> String {
        match self {
            Self::Harness => Self::HARNESS.to_string(),
            Self::CliRuntime(id) => format!("{}{id}", Self::CLI_PREFIX),
        }
    }
}

impl TryFrom<String> for SkillUsageSource {
    type Error = SkillUsageError;
    fn try_from(value: String) -> Result<Self, SkillUsageError> {
        if value == Self::HARNESS {
            return Ok(Self::Harness);
        }
        match value.strip_prefix(Self::CLI_PREFIX) {
            Some(id) => Self::cli_runtime(id),
            None => Err(SkillUsageError::InvalidSource {
                reason: "unknown source kind",
            }),
        }
    }
}

impl From<SkillUsageSource> for String {
    fn from(source: SkillUsageSource) -> Self {
        source.wire()
    }
}

/// Whose document: the per-user record of one tenant's user.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SkillUsageOwner {
    pub tenant_id: TenantId,
    pub user_id: UserId,
}

/// How a skill is listed to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillListing {
    /// Name and description.
    Full,
    /// Name only: still callable, almost free.
    NameOnly,
    /// Not listed.
    Off,
}

impl SkillListing {
    pub const fn wire(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::NameOnly => "name_only",
            Self::Off => "off",
        }
    }
}

/// The person's choice for one skill, across every source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillPin {
    AlwaysFull,
    NameOnly,
    Off,
}

impl SkillPin {
    pub const fn listing(self) -> SkillListing {
        match self {
            Self::AlwaysFull => SkillListing::Full,
            Self::NameOnly => SkillListing::NameOnly,
            Self::Off => SkillListing::Off,
        }
    }
}

/// Why a skill is listed the way it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillListingBasis {
    /// The usage policy decided.
    Auto,
    /// The person pinned it.
    Pinned,
    /// The source keeps it this way regardless (its own profile plugin, or
    /// a skill whose tools the source has switched off).
    Source,
}

/// One skill's listing and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkillListingState {
    pub listing: SkillListing,
    pub basis: SkillListingBasis,
}

/// One skill's uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillUse {
    pub count: u64,
    /// Unix milliseconds of the latest use.
    pub last_used_ms: u64,
}

impl SkillUse {
    /// Uses, halved every week, never below a tenth (Claude Code's own
    /// ranking).
    pub fn score(self, now_ms: u64) -> f64 {
        let days = now_ms.saturating_sub(self.last_used_ms) as f64 / DAY_MS;
        self.count as f64 * 0.5_f64.powf(days / 7.0).max(0.1)
    }

    /// Both readings folded together: the larger count, the later use.
    pub fn merged(self, other: Self) -> Self {
        Self {
            count: self.count.max(other.count),
            last_used_ms: self.last_used_ms.max(other.last_used_ms),
        }
    }
}

/// Uses per skill.
pub type SkillLedger = BTreeMap<SkillUsageName, SkillUse>;

/// What one source offered and how its skills were used.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillSourceRecord {
    /// The skills the source last offered, in its order.
    #[serde(default)]
    pub catalog: Vec<SkillUsageName>,
    #[serde(default)]
    pub ledger: SkillLedger,
}

/// The per-user record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillUsageDocument {
    #[serde(default)]
    pub sources: BTreeMap<SkillUsageSource, SkillSourceRecord>,
    #[serde(default)]
    pub pins: BTreeMap<SkillUsageName, SkillPin>,
}

impl SkillUsageDocument {
    /// Every source's uses together: counts add up, the latest use wins. A
    /// skill used a lot anywhere is hot wherever it is listed.
    pub fn combined_ledger(&self) -> SkillLedger {
        let mut out = SkillLedger::new();
        for record in self.sources.values() {
            for (name, used) in &record.ledger {
                let entry = out.entry(name.clone()).or_default();
                entry.count = entry.count.saturating_add(used.count);
                entry.last_used_ms = entry.last_used_ms.max(used.last_used_ms);
            }
        }
        out
    }

    /// The catalog one source last offered; empty when it never reported.
    pub fn catalog(&self, source: &SkillUsageSource) -> &[SkillUsageName] {
        self.sources
            .get(source)
            .map(|r| r.catalog.as_slice())
            .unwrap_or(&[])
    }

    /// Fold one observation in; whether anything changed.
    pub fn apply(
        &mut self,
        source: &SkillUsageSource,
        observation: &SkillUsageObservation,
        now_ms: u64,
    ) -> Result<bool, SkillUsageError> {
        if observation.is_empty() {
            return Ok(false);
        }
        if !self.sources.contains_key(source) && self.sources.len() >= SKILL_USAGE_SOURCES_MAX {
            return Err(SkillUsageError::Full { reason: "sources" });
        }
        let record = self.sources.entry(source.clone()).or_default();
        let mut changed = false;
        if let Some(catalog) = &observation.catalog {
            let mut seen = BTreeSet::new();
            let bounded: Vec<SkillUsageName> = catalog
                .iter()
                .filter(|n| seen.insert((*n).clone()))
                .take(SKILL_USAGE_CATALOG_MAX)
                .cloned()
                .collect();
            if bounded != record.catalog {
                record.catalog = bounded;
                changed = true;
            }
        }
        for name in &observation.used {
            if !record.ledger.contains_key(name) && record.ledger.len() >= SKILL_USAGE_LEDGER_MAX {
                tracing::debug!(skill = %name, "skill usage: ledger full; use not counted");
                continue;
            }
            let entry = record.ledger.entry(name.clone()).or_default();
            entry.count = entry.count.saturating_add(1);
            entry.last_used_ms = entry.last_used_ms.max(now_ms);
            changed = true;
        }
        Ok(changed)
    }

    /// Set or clear one pin; whether anything changed.
    pub fn set_pin(
        &mut self,
        name: &SkillUsageName,
        pin: Option<SkillPin>,
    ) -> Result<bool, SkillUsageError> {
        match pin {
            Some(pin) => {
                if !self.pins.contains_key(name) && self.pins.len() >= SKILL_USAGE_PINS_MAX {
                    return Err(SkillUsageError::Full { reason: "pins" });
                }
                Ok(self.pins.insert(name.clone(), pin) != Some(pin))
            }
            None => Ok(self.pins.remove(name).is_some()),
        }
    }
}

/// What one turn of one source says about skills: the catalog it offered
/// (when it reports one) and the skills it loaded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillUsageObservation {
    pub catalog: Option<Vec<SkillUsageName>>,
    pub used: Vec<SkillUsageName>,
}

impl SkillUsageObservation {
    pub fn is_empty(&self) -> bool {
        self.catalog.is_none() && self.used.is_empty()
    }
}

/// The policy's knobs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkillListingPolicy {
    /// A skill scoring at least this is hot: one use within the past week,
    /// or more uses longer ago.
    pub hot_score: f64,
    /// At most this many skills stay fully listed for their score alone.
    pub hot_cap: usize,
}

impl Default for SkillListingPolicy {
    fn default() -> Self {
        Self {
            hot_score: 0.5,
            hot_cap: 12,
        }
    }
}

/// What [`listing`] decides over.
#[derive(Debug, Clone, Copy)]
pub struct SkillListingRequest<'a> {
    pub catalog: &'a [SkillUsageName],
    pub ledger: &'a SkillLedger,
    pub pins: &'a BTreeMap<SkillUsageName, SkillPin>,
    /// Always fully listed unless pinned otherwise (a source's own
    /// profile plugin skills); they do not take a hot slot.
    pub always_full: &'a BTreeSet<SkillUsageName>,
    pub now_ms: u64,
}

/// How each skill of the catalog is listed. A pin decides first; then
/// `always_full`; then up to `hot_cap` skills scoring at least `hot_score`
/// (highest first, then by name) are full; the rest are name-only. Only the
/// catalog's skills get an entry.
pub fn listing(
    policy: &SkillListingPolicy,
    request: SkillListingRequest<'_>,
) -> BTreeMap<SkillUsageName, SkillListingState> {
    let mut scored: Vec<(&SkillUsageName, f64)> = request
        .catalog
        .iter()
        .filter(|n| !request.pins.contains_key(*n) && !request.always_full.contains(*n))
        .filter_map(|n| {
            let score = request.ledger.get(n)?.score(request.now_ms);
            (score >= policy.hot_score).then_some((n, score))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
    let hot: BTreeSet<&SkillUsageName> = scored
        .into_iter()
        .take(policy.hot_cap)
        .map(|(n, _)| n)
        .collect();
    request
        .catalog
        .iter()
        .map(|name| {
            let state = if let Some(pin) = request.pins.get(name) {
                SkillListingState {
                    listing: pin.listing(),
                    basis: SkillListingBasis::Pinned,
                }
            } else if request.always_full.contains(name) {
                SkillListingState {
                    listing: SkillListing::Full,
                    basis: SkillListingBasis::Source,
                }
            } else {
                SkillListingState {
                    listing: if hot.contains(name) {
                        SkillListing::Full
                    } else {
                        SkillListing::NameOnly
                    },
                    basis: SkillListingBasis::Auto,
                }
            };
            (name.clone(), state)
        })
        .collect()
}

/// One source's view of a skill, for a summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillSourceListing {
    pub source: SkillUsageSource,
    pub state: SkillListingState,
    /// Uses this source counted.
    pub uses: u64,
}

/// One skill across every source that offered it (or pinned with none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillListingSummary {
    pub name: SkillUsageName,
    /// Uses across every source.
    pub uses: u64,
    /// The latest use; `None` when never used.
    pub last_used_ms: Option<u64>,
    /// The listing over every source's catalog together.
    pub state: SkillListingState,
    pub pin: Option<SkillPin>,
    /// Each source that offered it, in source order.
    pub sources: Vec<SkillSourceListing>,
}

/// Every skill of `document`: offered by some source, or pinned. The
/// overall state decides over every catalog together with every use (the
/// harness's own view); each source's over its own catalog. Sorted by name.
pub fn summarize(
    policy: &SkillListingPolicy,
    document: &SkillUsageDocument,
    now_ms: u64,
) -> Vec<SkillListingSummary> {
    let ledger = document.combined_ledger();
    let none = BTreeSet::new();
    let mut union: BTreeSet<SkillUsageName> = document
        .sources
        .values()
        .flat_map(|record| record.catalog.iter().cloned())
        .collect();
    union.extend(document.pins.keys().cloned());
    let union: Vec<SkillUsageName> = union.into_iter().collect();
    let overall = listing(
        policy,
        SkillListingRequest {
            catalog: &union,
            ledger: &ledger,
            pins: &document.pins,
            always_full: &none,
            now_ms,
        },
    );
    let per_source: Vec<(&SkillUsageSource, &SkillSourceRecord, BTreeMap<_, _>)> = document
        .sources
        .iter()
        .map(|(source, record)| {
            let decided = listing(
                policy,
                SkillListingRequest {
                    catalog: &record.catalog,
                    ledger: &ledger,
                    pins: &document.pins,
                    always_full: &none,
                    now_ms,
                },
            );
            (source, record, decided)
        })
        .collect();
    union
        .into_iter()
        .filter_map(|name| {
            let state = *overall.get(&name)?;
            let used = ledger.get(&name).copied().unwrap_or_default();
            let sources = per_source
                .iter()
                .filter_map(|(source, record, decided)| {
                    Some(SkillSourceListing {
                        source: (*source).clone(),
                        state: *decided.get(&name)?,
                        uses: record.ledger.get(&name).map_or(0, |u| u.count),
                    })
                })
                .collect();
            Some(SkillListingSummary {
                pin: document.pins.get(&name).copied(),
                uses: used.count,
                last_used_ms: (used.count > 0).then_some(used.last_used_ms),
                state,
                sources,
                name,
            })
        })
        .collect()
}

/// The typed store of [`SkillUsageDocument`]s, one per owner.
///
/// Also the deployment's switch for listing management: a store that
/// returns `None` from [`Self::load`] manages nothing, and every source then
/// lists skills as it always has.
#[async_trait]
pub trait SkillUsageStore: Send + Sync + fmt::Debug {
    /// The owner's document (empty when none was written yet); `None` when
    /// this store does not manage listings.
    async fn load(
        &self,
        owner: &SkillUsageOwner,
    ) -> Result<Option<SkillUsageDocument>, SkillUsageError>;

    /// Fold one source's observation into the owner's document.
    async fn record(
        &self,
        owner: &SkillUsageOwner,
        source: &SkillUsageSource,
        observation: SkillUsageObservation,
        now_ms: u64,
    ) -> Result<(), SkillUsageError>;

    /// Merge a ledger imported from elsewhere into one source's record
    /// (the larger count and the later use of each skill).
    async fn import_ledger(
        &self,
        owner: &SkillUsageOwner,
        source: &SkillUsageSource,
        ledger: SkillLedger,
    ) -> Result<(), SkillUsageError>;

    /// Set (`Some`) or clear (`None`) one pin; returns the document as
    /// written, for read-back.
    async fn set_pin(
        &self,
        owner: &SkillUsageOwner,
        name: &SkillUsageName,
        pin: Option<SkillPin>,
    ) -> Result<SkillUsageDocument, SkillUsageError>;
}

/// A store that manages nothing: every source keeps its listing as is.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnmanagedSkillUsage;

#[async_trait]
impl SkillUsageStore for UnmanagedSkillUsage {
    async fn load(
        &self,
        _owner: &SkillUsageOwner,
    ) -> Result<Option<SkillUsageDocument>, SkillUsageError> {
        Ok(None)
    }

    async fn record(
        &self,
        _owner: &SkillUsageOwner,
        _source: &SkillUsageSource,
        _observation: SkillUsageObservation,
        _now_ms: u64,
    ) -> Result<(), SkillUsageError> {
        Ok(())
    }

    async fn import_ledger(
        &self,
        _owner: &SkillUsageOwner,
        _source: &SkillUsageSource,
        _ledger: SkillLedger,
    ) -> Result<(), SkillUsageError> {
        Ok(())
    }

    async fn set_pin(
        &self,
        _owner: &SkillUsageOwner,
        _name: &SkillUsageName,
        _pin: Option<SkillPin>,
    ) -> Result<SkillUsageDocument, SkillUsageError> {
        Err(SkillUsageError::Backend {
            reason: "skill listing is not managed in this deployment".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000_000;
    const WEEK: u64 = 7 * 86_400_000;

    fn name(s: &str) -> SkillUsageName {
        SkillUsageName::new(s).unwrap()
    }

    fn used(count: u64, ago: u64) -> SkillUse {
        SkillUse {
            count,
            last_used_ms: NOW - ago,
        }
    }

    fn names(list: &[&str]) -> Vec<SkillUsageName> {
        list.iter().map(|s| name(s)).collect()
    }

    fn ledger(entries: &[(&str, SkillUse)]) -> SkillLedger {
        entries.iter().map(|(n, u)| (name(n), *u)).collect()
    }

    fn decide(
        catalog: &[SkillUsageName],
        ledger: &SkillLedger,
        pins: &BTreeMap<SkillUsageName, SkillPin>,
        always_full: &BTreeSet<SkillUsageName>,
    ) -> BTreeMap<SkillUsageName, SkillListingState> {
        listing(
            &SkillListingPolicy::default(),
            SkillListingRequest {
                catalog,
                ledger,
                pins,
                always_full,
                now_ms: NOW,
            },
        )
    }

    #[test]
    fn score_halves_weekly_and_floors_at_a_tenth() {
        assert_eq!(used(4, 0).score(NOW), 4.0);
        assert!((used(4, WEEK).score(NOW) - 2.0).abs() < 1e-9);
        assert!((used(4, 52 * WEEK).score(NOW) - 0.4).abs() < 1e-9);
    }

    #[test]
    fn hot_skills_stay_full_cold_ones_go_name_only_and_pins_win() {
        let catalog = names(&[
            "grilling",
            "pdf",
            "ironclaw:deploy",
            "old",
            "pinned",
            "never",
        ]);
        let ledger = ledger(&[
            ("grilling", used(3, 0)),
            ("old", used(1, 8 * WEEK)),
            ("pdf", used(9, 0)),
        ]);
        let pins = BTreeMap::from([
            (name("pinned"), SkillPin::AlwaysFull),
            (name("pdf"), SkillPin::Off),
        ]);
        let own = BTreeSet::from([name("ironclaw:deploy")]);
        let got = decide(&catalog, &ledger, &pins, &own);
        let state = |n: &str| got[&name(n)];
        assert_eq!(state("grilling").listing, SkillListing::Full);
        assert_eq!(state("grilling").basis, SkillListingBasis::Auto);
        assert_eq!(state("ironclaw:deploy").basis, SkillListingBasis::Source);
        assert_eq!(state("pinned").listing, SkillListing::Full);
        assert_eq!(state("pinned").basis, SkillListingBasis::Pinned);
        assert_eq!(state("old").listing, SkillListing::NameOnly);
        assert_eq!(state("never").listing, SkillListing::NameOnly);
        assert_eq!(
            state("pdf").listing,
            SkillListing::Off,
            "a pin beats a hot score"
        );
        assert_eq!(got.len(), catalog.len(), "only catalog skills get an entry");
    }

    #[test]
    fn a_pin_beats_the_sources_own_skills() {
        let catalog = names(&["ironclaw:deploy"]);
        let pins = BTreeMap::from([(name("ironclaw:deploy"), SkillPin::NameOnly)]);
        let own = BTreeSet::from([name("ironclaw:deploy")]);
        let got = decide(&catalog, &SkillLedger::new(), &pins, &own);
        assert_eq!(
            got[&name("ironclaw:deploy")].listing,
            SkillListing::NameOnly
        );
    }

    #[test]
    fn the_hot_set_is_capped_by_score() {
        let all: Vec<String> = (0..20).map(|i| format!("s{i:02}")).collect();
        let catalog: Vec<SkillUsageName> = all.iter().map(|s| name(s)).collect();
        let ledger: SkillLedger = catalog
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), used(i as u64 + 1, 0)))
            .collect();
        let got = decide(&catalog, &ledger, &BTreeMap::new(), &BTreeSet::new());
        let full: Vec<&str> = got
            .iter()
            .filter(|(_, s)| s.listing == SkillListing::Full)
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(full.len(), SkillListingPolicy::default().hot_cap);
        assert!(full.contains(&"s19") && !full.contains(&"s07"));
    }

    #[test]
    fn sources_add_up_in_the_combined_ledger() {
        let mut doc = SkillUsageDocument::default();
        let claude = SkillUsageSource::cli_runtime("claude-cli").unwrap();
        let observation = SkillUsageObservation {
            catalog: None,
            used: names(&["a"]),
        };
        assert!(doc.apply(&claude, &observation, NOW - WEEK).unwrap());
        assert!(
            doc.apply(&SkillUsageSource::Harness, &observation, NOW)
                .unwrap()
        );
        assert_eq!(doc.combined_ledger()[&name("a")], used(2, 0));
    }

    #[test]
    fn the_same_catalog_without_uses_changes_nothing() {
        let mut doc = SkillUsageDocument::default();
        let observation = SkillUsageObservation {
            catalog: Some(names(&["a", "b", "a"])),
            used: Vec::new(),
        };
        assert!(
            doc.apply(&SkillUsageSource::Harness, &observation, NOW)
                .unwrap()
        );
        assert_eq!(doc.catalog(&SkillUsageSource::Harness), names(&["a", "b"]));
        assert!(
            !doc.apply(&SkillUsageSource::Harness, &observation, NOW)
                .unwrap()
        );
    }

    #[test]
    fn names_and_sources_validate_on_the_wire() {
        assert!(SkillUsageName::new("").is_err());
        assert!(SkillUsageName::new("two words").is_err());
        assert!(serde_json::from_str::<SkillUsageName>("\"a\\nb\"").is_err());
        let source: SkillUsageSource = serde_json::from_str("\"cli:claude-cli\"").unwrap();
        assert_eq!(source, SkillUsageSource::cli_runtime("claude-cli").unwrap());
        assert_eq!(
            serde_json::to_string(&source).unwrap(),
            "\"cli:claude-cli\""
        );
        assert!(serde_json::from_str::<SkillUsageSource>("\"cli:a/b\"").is_err());
        assert!(serde_json::from_str::<SkillUsageSource>("\"other\"").is_err());
    }

    #[test]
    fn the_summary_spans_every_source_and_keeps_pinned_strays() {
        let mut doc = SkillUsageDocument::default();
        let claude = SkillUsageSource::cli_runtime("claude-cli").unwrap();
        doc.apply(
            &claude,
            &SkillUsageObservation {
                catalog: Some(names(&["a", "b"])),
                used: names(&["a"]),
            },
            NOW,
        )
        .unwrap();
        doc.apply(
            &SkillUsageSource::Harness,
            &SkillUsageObservation {
                catalog: Some(names(&["b"])),
                used: Vec::new(),
            },
            NOW,
        )
        .unwrap();
        doc.set_pin(&name("gone"), Some(SkillPin::Off)).unwrap();
        let summary = summarize(&SkillListingPolicy::default(), &doc, NOW);
        let names: Vec<&str> = summary.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "gone"]);
        assert_eq!(summary[0].state.listing, SkillListing::Full);
        assert_eq!(summary[0].uses, 1);
        assert_eq!(summary[0].sources.len(), 1);
        assert_eq!(summary[1].state.listing, SkillListing::NameOnly);
        assert_eq!(summary[1].sources.len(), 2);
        assert_eq!(summary[1].last_used_ms, None);
        assert_eq!(summary[2].state.basis, SkillListingBasis::Pinned);
        assert!(summary[2].sources.is_empty());
    }

    #[test]
    fn pins_set_and_clear() {
        let mut doc = SkillUsageDocument::default();
        assert!(doc.set_pin(&name("a"), Some(SkillPin::Off)).unwrap());
        assert!(!doc.set_pin(&name("a"), Some(SkillPin::Off)).unwrap());
        assert!(doc.set_pin(&name("a"), None).unwrap());
        assert!(doc.pins.is_empty());
    }
}
