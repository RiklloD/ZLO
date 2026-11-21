//! How a CLI runtime lists the profile's skills, from the shared skill
//! usage ledger (`ironclaw_skills::usage`), and how it counts their uses.
//!
//! - The profile index ([`ProfileSkills::entries`]) is re-rendered per the
//!   listing policy: full skills keep their description, name-only ones are
//!   listed by name and SKILL.md path, off ones are left out.
//! - The profile's skills are recorded as the runtime's catalog.
//! - A turn's stream that names a listed skill's `skills/<name>/SKILL.md`
//!   (a read, a `cat`, a tool call's input) counts one use of it.
//!
//! Advisory throughout: a store failure lists every skill in full, as the
//! index always did, and never fails a turn.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::runtime::{ProfileSkills, TurnProfile};
use ironclaw_skills::usage::{
    SkillLedger, SkillListing, SkillListingPolicy, SkillListingRequest, SkillListingState,
    SkillUsageDocument, SkillUsageName, SkillUsageObservation, SkillUsageOwner, SkillUsageSource,
    SkillUsageStore, UnmanagedSkillUsage, listing,
};
use serde_json::Value;

/// The shared skill usage store and whose document a CLI runtime writes
/// (the deployment's owner: CLI runtimes run for the local person).
#[derive(Debug, Clone)]
pub struct CliSkillUsage {
    store: Arc<dyn SkillUsageStore>,
    owner: Option<SkillUsageOwner>,
}

impl Default for CliSkillUsage {
    /// Manages nothing: every runtime lists its skills as before.
    fn default() -> Self {
        Self {
            store: Arc::new(UnmanagedSkillUsage),
            owner: None,
        }
    }
}

impl CliSkillUsage {
    pub fn new(store: Arc<dyn SkillUsageStore>, owner: SkillUsageOwner) -> Self {
        Self {
            store,
            owner: Some(owner),
        }
    }

    /// The owner's document; `None` when listing is not managed or the
    /// store failed (then nothing changes).
    pub(crate) async fn document(&self) -> Option<SkillUsageDocument> {
        let owner = self.owner.as_ref()?;
        match self.store.load(owner).await {
            Ok(document) => document,
            Err(error) => {
                tracing::debug!(%error, "cli skills: usage not read; listing all skills");
                None
            }
        }
    }

    pub(crate) async fn record(
        &self,
        source: &SkillUsageSource,
        observation: SkillUsageObservation,
    ) {
        let Some(owner) = self.owner.as_ref() else {
            return; // silent-ok: an unmanaged deployment keeps no ledger
        };
        if observation.is_empty() {
            return;
        }
        if let Err(error) = self
            .store
            .record(owner, source, observation, now_ms())
            .await
        {
            tracing::debug!(%error, "cli skills: usage not recorded");
        }
    }

    pub(crate) async fn import_ledger(&self, source: &SkillUsageSource, ledger: SkillLedger) {
        let Some(owner) = self.owner.as_ref() else {
            return; // silent-ok: an unmanaged deployment keeps no ledger
        };
        if let Err(error) = self.store.import_ledger(owner, source, ledger).await {
            tracing::debug!(%error, "cli skills: legacy ledger not imported");
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    u64::try_from(ironclaw_orchestrator::model::now_ms()).unwrap_or(0) // silent-ok: a clock before 1970 only ages every use
}

/// `name` as the ledger holds it; `None` for a name it cannot (that skill
/// then stays listed as the runtime lists it).
pub(crate) fn usage_name(name: &str) -> Option<SkillUsageName> {
    SkillUsageName::new(name).ok() // silent-ok: see the doc comment
}

/// How each of `catalog` is listed from `document`.
pub(crate) fn decide(
    document: &SkillUsageDocument,
    catalog: &[SkillUsageName],
    always_full: &BTreeSet<SkillUsageName>,
    extra_ledger: Option<&SkillLedger>,
) -> BTreeMap<SkillUsageName, SkillListingState> {
    let mut ledger = document.combined_ledger();
    for (name, theirs) in extra_ledger.into_iter().flatten() {
        let entry = ledger.entry(name.clone()).or_default();
        *entry = entry.merged(*theirs);
    }
    listing(
        &SkillListingPolicy::default(),
        SkillListingRequest {
            catalog,
            ledger: &ledger,
            pins: &document.pins,
            always_full,
            now_ms: now_ms(),
        },
    )
}

/// One runtime's side of listing management for agents that read the
/// profile index (Antigravity, Codex, Muse).
#[derive(Debug)]
pub(crate) struct CliSkills {
    usage: CliSkillUsage,
    source: Option<SkillUsageSource>,
    /// Per thread, the skills its sessions were shown (what a read of a
    /// SKILL.md path is matched against).
    shown: Mutex<HashMap<ThreadId, Arc<Vec<SkillUsageName>>>>,
}

impl CliSkills {
    pub(crate) fn new(usage: CliSkillUsage, runtime_id: &str) -> Self {
        let source = SkillUsageSource::cli_runtime(runtime_id)
            .inspect_err(|error| tracing::debug!(%error, "cli skills: runtime id not a source"))
            .ok(); // silent-ok: logged; the runtime then lists every skill in full
        Self {
            usage,
            source,
            shown: Mutex::new(HashMap::new()),
        }
    }

    /// `profile` with its skill index rendered per the listing (and the
    /// profile's skills recorded as this runtime's catalog). Unchanged when
    /// listing is not managed.
    pub(crate) async fn listed_profile(
        &self,
        thread: &ThreadId,
        profile: Option<&TurnProfile>,
    ) -> Option<TurnProfile> {
        let profile = profile?.clone();
        let Some(skills) = profile.skills.as_ref() else {
            return Some(profile);
        };
        let catalog: Vec<SkillUsageName> = skills
            .entries
            .iter()
            .filter_map(|e| usage_name(&e.name))
            .collect();
        if let Ok(mut shown) = self.shown.lock() {
            shown.insert(thread.clone(), Arc::new(catalog.clone()));
        }
        let (Some(source), Some(document)) = (&self.source, self.usage.document().await) else {
            return Some(profile);
        };
        if document.catalog(source) != catalog.as_slice() {
            self.usage
                .record(
                    source,
                    SkillUsageObservation {
                        catalog: Some(catalog.clone()),
                        used: Vec::new(),
                    },
                )
                .await;
        }
        let decided = decide(&document, &catalog, &BTreeSet::new(), None);
        let index = render_index(skills, &decided);
        Some(TurnProfile {
            skills: Some(ProfileSkills {
                index,
                ..skills.clone()
            }),
            ..profile
        })
    }

    /// A fresh watch for one turn of `thread`.
    pub(crate) fn watch(&self, thread: &ThreadId) -> SkillReadWatch {
        let shown = self
            .shown
            .lock()
            .ok()
            .and_then(|shown| shown.get(thread).cloned())
            .unwrap_or_default(); // silent-ok: a poisoned map only follows a panic elsewhere; nothing is matched
        SkillReadWatch::new(shown)
    }

    /// Count the skills one turn read.
    pub(crate) async fn record(&self, watch: SkillReadWatch) {
        let Some(source) = &self.source else {
            return; // silent-ok: logged at construction
        };
        let used: Vec<SkillUsageName> = watch.used.into_iter().collect();
        if used.is_empty() {
            return;
        }
        self.usage
            .record(
                source,
                SkillUsageObservation {
                    catalog: None,
                    used,
                },
            )
            .await;
    }
}

/// The index lines per listing: `- name: description (path)` for a full
/// skill, `- name (path)` for a name-only one, nothing for an off one. A
/// skill the listing has no entry for stays full.
pub(crate) fn render_index(
    skills: &ProfileSkills,
    decided: &BTreeMap<SkillUsageName, SkillListingState>,
) -> String {
    skills
        .entries
        .iter()
        .filter_map(|entry| {
            let listed = usage_name(&entry.name)
                .and_then(|n| decided.get(&n).map(|s| s.listing))
                .unwrap_or(SkillListing::Full);
            let path = entry.skill_md.display();
            match listed {
                SkillListing::Off => None,
                SkillListing::NameOnly => Some(format!("- {} ({path})", entry.name)),
                SkillListing::Full => {
                    Some(format!("- {}: {} ({path})", entry.name, entry.description))
                }
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Which of a turn's shown skills its stream read: any frame naming
/// `skills/<name>/SKILL.md` counts the skill once for the turn.
#[derive(Debug, Default)]
pub(crate) struct SkillReadWatch {
    patterns: Vec<(String, SkillUsageName)>,
    used: BTreeSet<SkillUsageName>,
}

impl SkillReadWatch {
    fn new(shown: Arc<Vec<SkillUsageName>>) -> Self {
        Self {
            patterns: shown
                .iter()
                .map(|name| (format!("skills/{name}/SKILL.md"), name.clone()))
                .collect(),
            used: BTreeSet::new(),
        }
    }

    /// One protocol message (`method` is the notification, or ACP's
    /// `sessionUpdate` kind). Model text (stream deltas, ACP chunks, message
    /// and reasoning items) only mentions a path, and is skipped.
    pub(crate) fn push(&mut self, method: &str, params: &Value) {
        if self.patterns.is_empty() || method.ends_with("/delta") || method.ends_with("_chunk") {
            return;
        }
        let item_kind = params
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str);
        if matches!(
            item_kind,
            Some("agentMessage" | "userMessage" | "reasoning" | "message")
        ) {
            return;
        }
        let text = params.to_string();
        if !text.contains("SKILL.md") {
            return;
        }
        for (pattern, name) in &self.patterns {
            if text.contains(pattern.as_str()) {
                self.used.insert(name.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_orchestrator::runtime::ProfileSkillEntry;
    use ironclaw_skills::usage::{SkillListingBasis, SkillUse};

    fn skills() -> ProfileSkills {
        let entry = |name: &str, description: &str| ProfileSkillEntry {
            name: name.into(),
            description: description.into(),
            skill_md: format!("/p/skills/{name}/SKILL.md").into(),
        };
        ProfileSkills {
            plugin_dir: "/p".into(),
            index: String::new(),
            entries: vec![
                entry("hot", "Used a lot"),
                entry("cold", "Rarely used"),
                entry("gone", "Pinned off"),
            ],
        }
    }

    #[test]
    fn the_index_lists_name_only_skills_by_name_and_path_and_drops_off_ones() {
        let state = |listing| SkillListingState {
            listing,
            basis: SkillListingBasis::Auto,
        };
        let decided = BTreeMap::from([
            (usage_name("hot").unwrap(), state(SkillListing::Full)),
            (usage_name("cold").unwrap(), state(SkillListing::NameOnly)),
            (usage_name("gone").unwrap(), state(SkillListing::Off)),
        ]);
        assert_eq!(
            render_index(&skills(), &decided),
            "- hot: Used a lot (/p/skills/hot/SKILL.md)\n- cold (/p/skills/cold/SKILL.md)"
        );
    }

    #[test]
    fn a_read_of_a_shown_skill_md_counts_once_per_turn() {
        let shown = Arc::new(vec![
            usage_name("hot").unwrap(),
            usage_name("cold").unwrap(),
        ]);
        let mut watch = SkillReadWatch::new(shown);
        let read = serde_json::json!({"item": {"type": "commandExecution", "command": "sed -n 1,80p /p/skills/cold/SKILL.md"}});
        watch.push("item/started", &read);
        watch.push("item/completed", &read);
        watch.push(
            "item/agentMessage/delta",
            &serde_json::json!({"delta": "see skills/hot/SKILL.md"}),
        );
        watch.push(
            "item/completed",
            &serde_json::json!({"item": {"type": "agentMessage", "text": "skills/hot/SKILL.md"}}),
        );
        watch.push(
            "agent_message_chunk",
            &serde_json::json!({"content": {"text": "skills/hot/SKILL.md"}}),
        );
        let used: Vec<&str> = watch.used.iter().map(SkillUsageName::as_str).collect();
        assert_eq!(used, vec!["cold"]);
    }

    #[test]
    fn decide_merges_an_extra_ledger() {
        let document = SkillUsageDocument::default();
        let catalog = vec![usage_name("a").unwrap()];
        let extra = SkillLedger::from([(
            usage_name("a").unwrap(),
            SkillUse {
                count: 3,
                last_used_ms: now_ms(),
            },
        )]);
        let got = decide(&document, &catalog, &BTreeSet::new(), Some(&extra));
        assert_eq!(got[&catalog[0]].listing, SkillListing::Full);
    }
}
