//! Configuration sync between two of the person's computers, carried by
//! this client: it reads what changed on the source backend (This Mac) and
//! applies it to the target (another computer) over the connections the
//! app already holds, so the target never gets a credential that can read
//! the source.
//!
//! One pass ([`sync_once`]): both manifests, then only the domains whose
//! content hashes differ. Small domains travel together in one snapshot.
//! An itemized domain (skills, memory) travels alone in pages: the
//! receiver's index first, then the sender's pages with content only for
//! what the receiver lacks. A two-way domain (memory, skill usage) then
//! gets a return leg, target to source, that only merges. Unchanged
//! domains cost two small reads and nothing else.
//!
//! The backend decides what a domain holds and how it merges
//! (`ironclaw_assistant::config_sync`). Snapshots pass through here as
//! JSON this client does not interpret, and replies are read leniently, so
//! a backend newer than the app still syncs the domains both know. A
//! backend that predates the routes answers 404
//! ([`SyncOutcome::Unsupported`]).

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, Ordering};

pub use ironclaw_product_contracts::config_sync::ConfigSyncDomain;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{Client, V2};
use crate::error::ClientError;

/// Which of the two backends lacks the routes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Source,
    Target,
}

/// How one pass ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncOutcome {
    /// Nothing had to move.
    UpToDate,
    /// What the passes over the differing domains did.
    Applied(SyncReport),
    /// That backend is older than configuration sync.
    Unsupported(Side),
}

/// What a pass did, reduced to what the app shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// Domains in which something was written, on either computer.
    pub applied: Vec<ConfigSyncDomain>,
    /// Domains the target left alone, with its reason.
    pub skipped: Vec<(ConfigSyncDomain, String)>,
    pub warnings: Vec<String>,
    /// Config sections that take effect after the target restarts.
    pub restart_required: Vec<String>,
}

impl SyncReport {
    fn changed_nothing(&self) -> bool {
        self.applied.is_empty() && self.skipped.is_empty() && self.warnings.is_empty()
    }
}

/// Item keys and hashes named in one page request, in bytes: well under the
/// export route's body limit however long the keys are.
const HAVE_WINDOW_BYTES: usize = 384 * 1024;

/// A pass that was told to stop (its computer's sync was switched off, or
/// its connection went) sends nothing further.
fn live(stop: &AtomicBool) -> Result<(), ClientError> {
    if stop.load(Ordering::Relaxed) {
        return Err(ClientError::Config("configuration sync was stopped".into()));
    }
    Ok(())
}

/// Domains that differ and stay that way by design (the target keeps
/// items of its own): the pair of hashes a pass ended on without writing
/// anything. The same pair seen again is not carried again.
pub type SyncMemo = BTreeMap<ConfigSyncDomain, (String, String)>;

#[derive(Debug, Deserialize)]
struct Digest {
    hash: String,
}

/// A manifest as far as the carrier reads it. Domains this build does not
/// know are dropped, not an error.
#[derive(Debug)]
pub struct Manifest {
    pub environment_id: String,
    pub hashes: BTreeMap<ConfigSyncDomain, String>,
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    environment_id: String,
    #[serde(default)]
    domains: BTreeMap<String, Digest>,
}

#[derive(Debug, Default, Deserialize)]
struct RawOutcome {
    #[serde(default)]
    status: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    applied_count: u64,
    #[serde(default)]
    warnings: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawReport {
    #[serde(default)]
    domains: BTreeMap<String, RawOutcome>,
    #[serde(default)]
    restart_required: Vec<String>,
}

/// The domains both backends list whose content differs, minus `excluded`
/// and those `memo` says are settled as they are.
pub fn differing(
    source: &Manifest,
    target: &Manifest,
    excluded: &BTreeSet<ConfigSyncDomain>,
    memo: &SyncMemo,
) -> Vec<ConfigSyncDomain> {
    source
        .hashes
        .iter()
        .filter(|(domain, _)| !excluded.contains(domain))
        .filter_map(|(domain, ours)| {
            let theirs = target.hashes.get(domain)?;
            let settled = memo
                .get(domain)
                .is_some_and(|(a, b)| a == ours && b == theirs);
            (ours != theirs && !settled).then_some(*domain)
        })
        .collect()
}

/// A reply that is either the result or `{"outcome":"rejected",…}`.
fn accepted(what: &str, reply: Value) -> Result<Value, ClientError> {
    if reply.get("outcome").and_then(Value::as_str) == Some("rejected") {
        let field = reply.get("field").and_then(Value::as_str).unwrap_or("");
        let reason = reply.get("reason").and_then(Value::as_str).unwrap_or("");
        return Err(ClientError::Config(format!(
            "configuration {what} refused ({field}): {reason}"
        )));
    }
    Ok(reply)
}

impl Client {
    pub async fn config_sync_manifest(&self) -> Result<Manifest, ClientError> {
        let raw: RawManifest = self
            .get(&format!("{V2}/operator/config-sync/manifest"))
            .await?;
        Ok(Manifest {
            environment_id: raw.environment_id,
            hashes: raw
                .domains
                .into_iter()
                .filter_map(|(name, digest)| {
                    Some((ConfigSyncDomain::from_wire(&name)?, digest.hash))
                })
                .collect(),
        })
    }

    /// A snapshot (or one page of an itemized domain), as the backend made
    /// it: handed to [`Self::config_sync_apply`] untouched.
    async fn config_sync_export(&self, request: &Value) -> Result<Value, ClientError> {
        let reply: Value = self
            .post(&format!("{V2}/operator/config-sync/export"), request)
            .await?;
        accepted("export", reply)
    }

    async fn config_sync_apply(
        &self,
        snapshot: &Value,
        domains: &[ConfigSyncDomain],
        merge_back: bool,
    ) -> Result<RawReport, ClientError> {
        // The export's own tag is not part of a snapshot.
        let mut snapshot = snapshot.clone();
        if let Some(fields) = snapshot.as_object_mut() {
            fields.remove("outcome");
        }
        let mut body = json!({ "snapshot": snapshot, "domains": domains });
        if merge_back {
            body["merge_back"] = Value::Bool(true);
        }
        let reply: Value = self
            .post(&format!("{V2}/operator/config-sync/apply"), &body)
            .await?;
        serde_json::from_value(accepted("apply", reply)?)
            .map_err(|e| ClientError::Config(format!("configuration apply reply: {e}")))
    }
}

/// What one or more applies did, gathered per domain.
#[derive(Default)]
struct Tally {
    report: SyncReport,
    /// Domains in which any apply wrote something.
    wrote: BTreeSet<ConfigSyncDomain>,
    /// Domains an apply could not finish (a store error, a document that
    /// changed under it, an item too large): never "settled", so the next
    /// check carries them again.
    unfinished: BTreeSet<ConfigSyncDomain>,
}

impl Tally {
    /// `to_target`: the forward leg, whose skips and restart notes are the
    /// ones the person acts on.
    fn add(&mut self, raw: RawReport, to_target: bool) {
        for (name, outcome) in raw.domains {
            let Some(domain) = ConfigSyncDomain::from_wire(&name) else {
                continue;
            };
            if outcome.applied_count > 0 || outcome.status == "applied" {
                self.wrote.insert(domain);
            }
            if outcome.status == "skipped_with_reason" || !outcome.warnings.is_empty() {
                self.unfinished.insert(domain);
            }
            if to_target
                && outcome.status == "skipped_with_reason"
                && !self.report.skipped.iter().any(|(d, _)| *d == domain)
            {
                self.report
                    .skipped
                    .push((domain, outcome.reason.unwrap_or_default()));
            }
            for warning in outcome.warnings {
                if !self.report.warnings.contains(&warning) {
                    self.report.warnings.push(warning);
                }
            }
        }
        if to_target {
            for section in raw.restart_required {
                if !self.report.restart_required.contains(&section) {
                    self.report.restart_required.push(section);
                }
            }
        }
    }
}

/// The page of `domain` inside a snapshot.
fn page(snapshot: &Value, domain: ConfigSyncDomain) -> Option<&Value> {
    snapshot.get("domains")?.get(domain.as_str())
}

/// The item keys and hashes in one page of an itemized domain.
fn listed(snapshot: &Value, domain: ConfigSyncDomain) -> Vec<(String, String)> {
    page(snapshot, domain)
        .and_then(|p| p.get("items"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some((
                        item.get("key")?.as_str()?.to_string(),
                        item.get("hash")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn next_of(snapshot: &Value, domain: ConfigSyncDomain) -> Option<String> {
    page(snapshot, domain)?
        .get("next")?
        .as_str()
        .map(str::to_string)
}

/// The part of `have` after `after` that fits one request, and the key it
/// is complete up to when it had to be cut (`None`: it reaches the end).
fn have_window(
    have: &BTreeMap<String, String>,
    after: Option<&String>,
) -> (BTreeMap<String, String>, Option<String>) {
    let from = after.map_or(Bound::Unbounded, Bound::Excluded);
    let mut window = BTreeMap::new();
    let mut bytes = 0usize;
    for (key, hash) in have.range::<String, _>((from, Bound::Unbounded)) {
        bytes += key.len() + hash.len() + 8;
        if bytes > HAVE_WINDOW_BYTES && !window.is_empty() {
            let until = window.keys().next_back().cloned();
            return (window, until);
        }
        window.insert(key.clone(), hash.clone());
    }
    (window, None)
}

/// One itemized domain from `from` to `to`: the receiver's whole index
/// (in pages), then the sender's pages until there is no next one, each
/// told what the receiver holds in the range it covers.
async fn carry_items(
    from: &Client,
    to: &Client,
    domain: ConfigSyncDomain,
    merge_back: bool,
    stop: &AtomicBool,
    tally: &mut Tally,
) -> Result<(), ClientError> {
    let mut have: BTreeMap<String, String> = BTreeMap::new();
    let mut after: Option<String> = None;
    loop {
        live(stop)?;
        let mut request = json!({ "domains": [domain], "index_only": true });
        if let Some(after) = &after {
            request["after"] = Value::String(after.clone());
        }
        let index = to.config_sync_export(&request).await?;
        have.extend(listed(&index, domain));
        let next = next_of(&index, domain);
        // A cursor that does not move would loop forever.
        if next.is_none() || next <= after {
            break;
        }
        after = next;
    }
    let mut after: Option<String> = None;
    loop {
        live(stop)?;
        let (window, until) = have_window(&have, after.as_ref());
        let mut request = json!({ "domains": [domain], "have": window });
        if let Some(after) = &after {
            request["after"] = Value::String(after.clone());
        }
        if let Some(until) = &until {
            request["until"] = Value::String(until.clone());
        }
        let snapshot = from.config_sync_export(&request).await?;
        let next = next_of(&snapshot, domain);
        live(stop)?;
        tally.add(
            to.config_sync_apply(&snapshot, &[domain], merge_back)
                .await?,
            !merge_back,
        );
        if next.is_none() || next <= after {
            return Ok(());
        }
        after = next;
    }
}

/// One pass from `source` to `target`. `source` is only written by the
/// return leg of a two-way domain, which merges and never replaces.
/// Returns the memo to pass next time.
pub async fn sync_once(
    source: &Client,
    target: &Client,
    excluded: &BTreeSet<ConfigSyncDomain>,
    mut memo: SyncMemo,
    stop: &AtomicBool,
) -> Result<(SyncOutcome, SyncMemo), ClientError> {
    live(stop)?;
    let ours = match source.config_sync_manifest().await {
        Ok(manifest) => manifest,
        Err(e) if e.is_not_found() => return Ok((SyncOutcome::Unsupported(Side::Source), memo)),
        Err(e) => return Err(e),
    };
    let theirs = match target.config_sync_manifest().await {
        Ok(manifest) => manifest,
        Err(e) if e.is_not_found() => return Ok((SyncOutcome::Unsupported(Side::Target), memo)),
        Err(e) => return Err(e),
    };
    // The same installation on both ends (a tunnel back to This Mac): there
    // is nothing to carry.
    if ours.environment_id == theirs.environment_id {
        return Ok((SyncOutcome::UpToDate, memo));
    }
    let domains = differing(&ours, &theirs, excluded, &memo);
    if domains.is_empty() {
        return Ok((SyncOutcome::UpToDate, memo));
    }
    let mut tally = Tally::default();
    let (paged, plain): (Vec<_>, Vec<_>) = domains.iter().copied().partition(|d| d.itemized());
    if !plain.is_empty() {
        live(stop)?;
        let snapshot = source
            .config_sync_export(&json!({ "domains": plain }))
            .await?;
        live(stop)?;
        tally.add(
            target.config_sync_apply(&snapshot, &plain, false).await?,
            true,
        );
        for domain in plain.iter().copied().filter(|d| d.two_way()) {
            live(stop)?;
            let back = target
                .config_sync_export(&json!({ "domains": [domain] }))
                .await?;
            tally.add(
                source.config_sync_apply(&back, &[domain], true).await?,
                false,
            );
        }
    }
    for domain in paged {
        carry_items(source, target, domain, false, stop, &mut tally).await?;
        if domain.two_way() {
            carry_items(target, source, domain, true, stop, &mut tally).await?;
        }
    }
    for domain in &domains {
        // Settled only when a pass ran clean and still wrote nothing: the
        // difference is then one the handlers keep on purpose.
        if tally.wrote.contains(domain) || tally.unfinished.contains(domain) {
            memo.remove(domain);
        } else if let (Some(a), Some(b)) = (ours.hashes.get(domain), theirs.hashes.get(domain)) {
            memo.insert(*domain, (a.clone(), b.clone()));
        }
    }
    tally.report.applied = tally.wrote.into_iter().collect();
    let outcome = if tally.report.changed_nothing() {
        SyncOutcome::UpToDate
    } else {
        SyncOutcome::Applied(tally.report)
    };
    Ok((outcome, memo))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str, hashes: &[(ConfigSyncDomain, &str)]) -> Manifest {
        Manifest {
            environment_id: id.into(),
            hashes: hashes
                .iter()
                .map(|(domain, hash)| (*domain, (*hash).to_string()))
                .collect(),
        }
    }

    #[test]
    fn only_domains_both_know_that_differ_and_are_not_excluded_or_settled_travel() {
        use ConfigSyncDomain::{Approvals, Config, Profiles, Prompts};
        let ours = manifest(
            "a",
            &[
                (Config, "1"),
                (Profiles, "2"),
                (Prompts, "3"),
                (Approvals, "4"),
            ],
        );
        // The target has no approvals domain (an older build), matches on
        // prompts, and differs on config and profiles.
        let theirs = manifest("b", &[(Config, "9"), (Profiles, "8"), (Prompts, "3")]);
        let none = SyncMemo::new();
        assert_eq!(
            differing(&ours, &theirs, &BTreeSet::new(), &none),
            vec![Config, Profiles]
        );
        assert_eq!(
            differing(&ours, &theirs, &BTreeSet::from([Profiles]), &none),
            vec![Config]
        );
        // Profiles differ by design (the target keeps one of its own): the
        // same pair is not carried again, a changed side is.
        let memo = SyncMemo::from([(Profiles, ("2".to_string(), "8".to_string()))]);
        assert_eq!(
            differing(&ours, &theirs, &BTreeSet::new(), &memo),
            vec![Config]
        );
        let stale = SyncMemo::from([(Profiles, ("1".to_string(), "8".to_string()))]);
        assert_eq!(
            differing(&ours, &theirs, &BTreeSet::new(), &stale),
            vec![Config, Profiles]
        );
    }

    #[test]
    fn a_tally_keeps_what_was_written_skipped_and_needs_a_restart() {
        let forward: RawReport = serde_json::from_value(json!({
            "schema_version": 1,
            "target_environment_id": "b",
            "domains": {
                "config": {"status": "applied", "applied_count": 1, "skipped_count": 0},
                "prompts": {"status": "skipped_unchanged", "applied_count": 0, "skipped_count": 1},
                "profiles": {"status": "skipped_with_reason", "reason": "not enabled",
                             "applied_count": 0, "skipped_count": 0},
                "a_domain_from_the_future": {"status": "applied", "applied_count": 3}
            },
            "restart_required": ["budget"]
        }))
        .expect("report");
        let back: RawReport = serde_json::from_value(json!({
            "domains": {
                "memory": {"status": "applied", "applied_count": 2, "skipped_count": 0,
                           "warnings": ["notes/plan.md changed on both computers"]}
            },
            "restart_required": ["never shown: the return leg wrote This Mac"]
        }))
        .expect("report");
        let mut tally = Tally::default();
        tally.add(forward, true);
        tally.add(back, false);
        assert_eq!(
            tally.wrote,
            BTreeSet::from([ConfigSyncDomain::Config, ConfigSyncDomain::Memory])
        );
        assert_eq!(
            tally.report.skipped,
            vec![(ConfigSyncDomain::Profiles, "not enabled".to_string())]
        );
        assert_eq!(tally.report.restart_required, vec!["budget".to_string()]);
        assert_eq!(tally.report.warnings.len(), 1);
    }

    #[test]
    fn a_failed_or_warned_domain_is_unfinished_so_it_is_carried_again() {
        let report: RawReport = serde_json::from_value(json!({
            "domains": {
                "memory": {"status": "skipped_unchanged", "applied_count": 0,
                           "warnings": ["MEMORY.md changed while syncing; it is taken next time"]},
                "skills": {"status": "skipped_with_reason", "reason": "store is down",
                           "applied_count": 0},
                "prompts": {"status": "skipped_unchanged", "applied_count": 0}
            }
        }))
        .expect("report");
        let mut tally = Tally::default();
        tally.add(report, true);
        assert!(tally.wrote.is_empty());
        assert_eq!(
            tally.unfinished,
            BTreeSet::from([ConfigSyncDomain::Skills, ConfigSyncDomain::Memory])
        );
    }

    #[test]
    fn a_long_receiver_index_is_named_in_windows_that_fit_a_request() {
        let have: BTreeMap<String, String> = (0..3_000)
            .map(|n| {
                (
                    format!("notes/{n:05}-{}", "x".repeat(400)),
                    "sha256:h".to_string(),
                )
            })
            .collect();
        let mut after: Option<String> = None;
        let mut seen = 0;
        let mut windows = 0;
        loop {
            let (window, until) = have_window(&have, after.as_ref());
            let bytes: usize = window.iter().map(|(k, h)| k.len() + h.len() + 8).sum();
            assert!(bytes <= HAVE_WINDOW_BYTES, "{bytes}");
            assert!(!window.is_empty());
            seen += window.len();
            windows += 1;
            // The window is complete up to `until`, where the next begins.
            assert_eq!(
                until.as_ref(),
                until.as_ref().and(window.keys().next_back())
            );
            match until {
                Some(until) => after = Some(until),
                None => break,
            }
        }
        assert_eq!(seen, 3_000);
        assert!(windows > 1);
        // A short index is one window with no cut.
        let small = BTreeMap::from([("a".to_string(), "h".to_string())]);
        assert_eq!(have_window(&small, None), (small.clone(), None));
        assert_eq!(
            have_window(&small, Some(&"a".to_string())),
            (BTreeMap::new(), None)
        );
    }

    #[test]
    fn a_stopped_pass_sends_nothing_further() {
        let stop = AtomicBool::new(false);
        assert!(live(&stop).is_ok());
        stop.store(true, Ordering::Relaxed);
        assert!(live(&stop).is_err());
    }

    #[test]
    fn a_rejection_is_an_error_with_its_reason() {
        let error = accepted(
            "apply",
            json!({"outcome": "rejected", "field": "schema_version", "reason": "too new"}),
        )
        .expect_err("rejected");
        assert!(error.to_string().contains("too new"), "{error}");
        assert!(accepted("export", json!({"outcome": "exported"})).is_ok());
    }
}
