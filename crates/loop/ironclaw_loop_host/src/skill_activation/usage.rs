//! The harness side of skill listing management (`ironclaw_skills::usage`):
//! the run's visible skills are reported as the harness catalog, the
//! usage policy decides how each is listed, and every skill whose body
//! reaches the model is counted as a use.
//!
//! Advisory throughout: a store failure leaves the listing as it was
//! (every skill full) and never fails the prompt build.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use ironclaw_loop_contracts::LoopRunContext;
use ironclaw_skills::usage::{
    SkillListing, SkillListingPolicy, SkillListingRequest, SkillUsageName, SkillUsageObservation,
    SkillUsageOwner, SkillUsageSource, SkillUsageStore, listing,
};

/// Whose document the run reads and writes: the run's acting user (its
/// actor, else its explicit thread owner). A run with neither is a host run
/// with no person behind it, and its listing is not managed.
fn owner(run_context: &LoopRunContext) -> Option<SkillUsageOwner> {
    let user_id = run_context
        .actor()
        .map(|actor| actor.user_id.clone())
        .or_else(|| run_context.scope.explicit_owner_user_id().cloned())?;
    Some(SkillUsageOwner {
        tenant_id: run_context.scope.tenant_id.clone(),
        user_id,
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0) // silent-ok: a clock before 1970 only ages every use
}

fn usage_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<SkillUsageName> {
    let mut seen = BTreeSet::new();
    names
        .into_iter()
        .filter_map(|name| SkillUsageName::new(name).ok()) // silent-ok: a name the ledger cannot hold stays fully listed
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

/// How the harness lists each of `catalog` this run; `None` when listing is
/// not managed (no store, no owner, or the store failed), which lists every
/// skill as before. Records `catalog` as the harness catalog when it
/// changed.
pub(super) async fn harness_listing<'a>(
    store: &dyn SkillUsageStore,
    run_context: &LoopRunContext,
    catalog: impl IntoIterator<Item = &'a str>,
) -> Option<BTreeMap<String, SkillListing>> {
    let owner = owner(run_context)?;
    let document = match store.load(&owner).await {
        Ok(document) => document?,
        Err(error) => {
            tracing::debug!(%error, "skill usage: harness listing not read; listing all skills");
            return None;
        }
    };
    let catalog = usage_names(catalog);
    let source = SkillUsageSource::Harness;
    if document.catalog(&source) != catalog.as_slice() {
        let observation = SkillUsageObservation {
            catalog: Some(catalog.clone()),
            used: Vec::new(),
        };
        if let Err(error) = store.record(&owner, &source, observation, now_ms()).await {
            tracing::debug!(%error, "skill usage: harness catalog not recorded");
        }
    }
    let ledger = document.combined_ledger();
    let decided = listing(
        &SkillListingPolicy::default(),
        SkillListingRequest {
            catalog: &catalog,
            ledger: &ledger,
            pins: &document.pins,
            always_full: &BTreeSet::new(),
            now_ms: now_ms(),
        },
    );
    Some(
        decided
            .into_iter()
            .map(|(name, state)| (name.into_inner(), state.listing))
            .collect(),
    )
}

/// Count one use of each of `skills` (their bodies reached the model).
pub(super) async fn record_harness_use<'a>(
    store: &dyn SkillUsageStore,
    run_context: &LoopRunContext,
    skills: impl IntoIterator<Item = &'a str>,
) {
    let used = usage_names(skills);
    if used.is_empty() {
        return;
    }
    let Some(owner) = owner(run_context) else {
        return; // silent-ok: a run with no person behind it has no ledger
    };
    let observation = SkillUsageObservation {
        catalog: None,
        used,
    };
    if let Err(error) = store
        .record(&owner, &SkillUsageSource::Harness, observation, now_ms())
        .await
    {
        tracing::debug!(%error, "skill usage: harness use not recorded");
    }
}
