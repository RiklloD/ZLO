//! Configuration sync between two of the person's computers, carried by
//! this client: it reads what changed on the source backend (This Mac) and
//! applies it to the target (another computer) over the connections the
//! app already holds, so the target never gets a credential that can read
//! the source.
//!
//! One pass ([`sync_once`]): both manifests, the domains whose content
//! hashes differ, one export of those from the source, one apply to the
//! target. Unchanged domains cost two small reads and nothing else. The
//! backend decides what a domain holds and how it merges
//! (`ironclaw_assistant::config_sync`); a backend that predates the routes
//! answers 404, reported as [`SyncOutcome::Unsupported`].

use std::collections::BTreeSet;

pub use ironclaw_product_contracts::config_sync::ConfigSyncDomain;
use ironclaw_product_contracts::config_sync::{
    ConfigSnapshotV1, ConfigSyncApplyOutcome, ConfigSyncApplyReport, ConfigSyncExportOutcome,
    ConfigSyncManifest, DomainApplyStatus,
};
use serde_json::json;

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
    /// Every domain both sides know already matched.
    UpToDate,
    /// What the target reported for the domains that differed.
    Applied(SyncReport),
    /// That backend is older than configuration sync.
    Unsupported(Side),
}

/// What an apply did, reduced to what the app shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub applied: Vec<ConfigSyncDomain>,
    /// Domains the target left alone, with its reason.
    pub skipped: Vec<(ConfigSyncDomain, String)>,
    pub warnings: Vec<String>,
    /// Config sections that take effect after the target restarts.
    pub restart_required: Vec<String>,
}

impl From<ConfigSyncApplyReport> for SyncReport {
    fn from(report: ConfigSyncApplyReport) -> Self {
        let mut out = SyncReport {
            restart_required: report.restart_required,
            ..SyncReport::default()
        };
        for (domain, outcome) in report.domains {
            match outcome.status {
                DomainApplyStatus::Applied => out.applied.push(domain),
                DomainApplyStatus::SkippedUnchanged => {}
                DomainApplyStatus::SkippedWithReason => out
                    .skipped
                    .push((domain, outcome.reason.unwrap_or_default())),
            }
            out.warnings.extend(outcome.warnings);
        }
        out
    }
}

/// The domains both backends know whose content differs, minus `excluded`.
pub fn differing(
    source: &ConfigSyncManifest,
    target: &ConfigSyncManifest,
    excluded: &BTreeSet<ConfigSyncDomain>,
) -> Vec<ConfigSyncDomain> {
    source
        .domains
        .iter()
        .filter(|(domain, _)| !excluded.contains(domain))
        .filter(|(domain, digest)| {
            target
                .domains
                .get(domain)
                .is_some_and(|theirs| theirs.hash != digest.hash)
        })
        .map(|(domain, _)| *domain)
        .collect()
}

impl Client {
    pub async fn config_sync_manifest(&self) -> Result<ConfigSyncManifest, ClientError> {
        self.get(&format!("{V2}/operator/config-sync/manifest"))
            .await
    }

    pub async fn config_sync_export(
        &self,
        domains: &[ConfigSyncDomain],
    ) -> Result<ConfigSnapshotV1, ClientError> {
        let outcome: ConfigSyncExportOutcome = self
            .post(
                &format!("{V2}/operator/config-sync/export"),
                &json!({ "domains": domains }),
            )
            .await?;
        match outcome {
            ConfigSyncExportOutcome::Exported(snapshot) => Ok(snapshot),
            ConfigSyncExportOutcome::Rejected { field, reason } => Err(ClientError::Config(
                format!("configuration export refused ({field}): {reason}"),
            )),
        }
    }

    pub async fn config_sync_apply(
        &self,
        snapshot: &ConfigSnapshotV1,
        domains: &[ConfigSyncDomain],
    ) -> Result<ConfigSyncApplyReport, ClientError> {
        let outcome: ConfigSyncApplyOutcome = self
            .post(
                &format!("{V2}/operator/config-sync/apply"),
                &json!({ "snapshot": snapshot, "domains": domains }),
            )
            .await?;
        match outcome {
            ConfigSyncApplyOutcome::Completed(report) => Ok(report),
            ConfigSyncApplyOutcome::Rejected { field, reason } => Err(ClientError::Config(
                format!("configuration apply refused ({field}): {reason}"),
            )),
        }
    }
}

/// One pass from `source` to `target`. Never the other way: `source` is
/// only read.
pub async fn sync_once(
    source: &Client,
    target: &Client,
    excluded: &BTreeSet<ConfigSyncDomain>,
) -> Result<SyncOutcome, ClientError> {
    let ours = match source.config_sync_manifest().await {
        Ok(manifest) => manifest,
        Err(e) if e.is_not_found() => return Ok(SyncOutcome::Unsupported(Side::Source)),
        Err(e) => return Err(e),
    };
    let theirs = match target.config_sync_manifest().await {
        Ok(manifest) => manifest,
        Err(e) if e.is_not_found() => return Ok(SyncOutcome::Unsupported(Side::Target)),
        Err(e) => return Err(e),
    };
    // The same installation on both ends (a tunnel back to This Mac): there
    // is nothing to carry.
    if ours.environment_id == theirs.environment_id {
        return Ok(SyncOutcome::UpToDate);
    }
    let domains = differing(&ours, &theirs, excluded);
    if domains.is_empty() {
        return Ok(SyncOutcome::UpToDate);
    }
    let snapshot = source.config_sync_export(&domains).await?;
    let report = target.config_sync_apply(&snapshot, &domains).await?;
    Ok(SyncOutcome::Applied(report.into()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use ironclaw_product_contracts::config_sync::ConfigSyncDomainDigest;

    use super::*;

    fn manifest(id: &str, hashes: &[(ConfigSyncDomain, &str)]) -> ConfigSyncManifest {
        ConfigSyncManifest {
            schema_version: 1,
            environment_id: id.into(),
            os: "macos".into(),
            domains: hashes
                .iter()
                .map(|(domain, hash)| {
                    (
                        *domain,
                        ConfigSyncDomainDigest {
                            hash: (*hash).into(),
                            updated_at: None,
                            item_count: 1,
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
        }
    }

    #[test]
    fn only_domains_both_know_that_differ_and_are_not_excluded_travel() {
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
        assert_eq!(
            differing(&ours, &theirs, &BTreeSet::new()),
            vec![Config, Profiles]
        );
        assert_eq!(
            differing(&ours, &theirs, &BTreeSet::from([Profiles])),
            vec![Config]
        );
    }

    #[test]
    fn a_report_keeps_what_was_applied_skipped_and_needs_a_restart() {
        use ironclaw_product_contracts::config_sync::DomainApplyOutcome;
        let outcome = |status, reason: Option<&str>| DomainApplyOutcome {
            status,
            reason: reason.map(str::to_string),
            applied_count: 1,
            skipped_count: 0,
            warnings: Vec::new(),
        };
        let report = ConfigSyncApplyReport {
            schema_version: 1,
            target_environment_id: "b".into(),
            domains: BTreeMap::from([
                (
                    ConfigSyncDomain::Config,
                    outcome(DomainApplyStatus::Applied, None),
                ),
                (
                    ConfigSyncDomain::Prompts,
                    outcome(DomainApplyStatus::SkippedUnchanged, None),
                ),
                (
                    ConfigSyncDomain::Profiles,
                    outcome(DomainApplyStatus::SkippedWithReason, Some("not enabled")),
                ),
            ]),
            restart_required: vec!["budget".into()],
        };
        let shown = SyncReport::from(report);
        assert_eq!(shown.applied, vec![ConfigSyncDomain::Config]);
        assert_eq!(
            shown.skipped,
            vec![(ConfigSyncDomain::Profiles, "not enabled".to_string())]
        );
        assert_eq!(shown.restart_required, vec!["budget".to_string()]);
    }
}
