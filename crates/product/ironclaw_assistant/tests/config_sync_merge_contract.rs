//! Configuration sync at the service seam for the domains both computers
//! write (`memory`, `skill_usage`) and the paged one-way `skills`: two
//! hosts, carried the way the app carries them (the receiver's index, the
//! sender's pages, then the return leg).

#![deny(clippy::let_underscore_must_use)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_assistant::config_sync::ConfigSyncService;
use ironclaw_assistant::config_sync::approvals::ApprovalsDomainHandler;
use ironclaw_assistant::config_sync::memory::{
    BaseChange, MemoryDomainHandler, MemorySyncBaseStore,
};
use ironclaw_assistant::config_sync::notes::{SettingsNotes, SyncNotes};
use ironclaw_assistant::config_sync::skill_usage::SkillUsageDomainHandler;
use ironclaw_assistant::config_sync::skills::SkillsDomainHandler;
use ironclaw_assistant::config_sync::{ConfigSyncError, items};
use ironclaw_filesystem::InMemoryBackend;
use ironclaw_host_api::ids::{InvocationId, TenantId, UserId};
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_product_contracts::config_sync::{
    ConfigSyncApplyReport, ConfigSyncApplyRequest, ConfigSyncDomain, ConfigSyncDomainPayload,
    ConfigSyncExportRequest,
};
use ironclaw_product_contracts::environment_access::{
    DeviceGrant, DeviceId, DeviceList, EnvironmentAccessError, EnvironmentAccessService,
    EnvironmentId, EnvironmentIdentity, PairClient, PairedDevice, PairingCodeIssued,
    PairingCodeRequest,
};
use ironclaw_product_contracts::memory_page::{
    MemoryArchiveRequest, MemoryArchiveResponse, MemoryDocument, MemoryDocumentEntry,
    MemoryDocumentsSource, MemoryPageWriteOutcome, MemoryProvenance, MemorySearchHit,
    MemoryWriteRequest, MemoryWriteResponse, memory_document_kind,
};
use ironclaw_product_contracts::surface::{ProductSurfaceCaller, ProductSurfaceError};
use ironclaw_skills::usage::{
    SharedSkillUsage, SkillLedger, SkillPin, SkillUsageDocument, SkillUsageError, SkillUsageName,
    SkillUsageObservation, SkillUsageOwner, SkillUsageSource, SkillUsageStore, SkillUse,
};
use ironclaw_skills::{ScopedSkillManagementPort, build_scoped_skill_management_port};

struct Identity(&'static str);

#[async_trait]
impl EnvironmentAccessService for Identity {
    async fn environment(&self) -> Result<EnvironmentIdentity, EnvironmentAccessError> {
        Ok(EnvironmentIdentity {
            environment_id: EnvironmentId::new(self.0).expect("environment id"),
            label: "test".into(),
        })
    }
    async fn issue_pairing_code(
        &self,
        _: PairingCodeRequest,
    ) -> Result<PairingCodeIssued, EnvironmentAccessError> {
        Err(EnvironmentAccessError::InvalidCode)
    }
    async fn redeem_pairing_code(
        &self,
        _: &str,
        _: PairClient,
    ) -> Result<PairedDevice, EnvironmentAccessError> {
        Err(EnvironmentAccessError::InvalidCode)
    }
    async fn pair_tailnet_owner(
        &self,
        _: &str,
        _: PairClient,
    ) -> Result<PairedDevice, EnvironmentAccessError> {
        Err(EnvironmentAccessError::InvalidCode)
    }
    async fn authenticate_device(
        &self,
        _: &str,
    ) -> Result<Option<DeviceGrant>, EnvironmentAccessError> {
        Ok(None)
    }
    async fn list_devices(
        &self,
        _: Option<&DeviceId>,
    ) -> Result<DeviceList, EnvironmentAccessError> {
        Err(EnvironmentAccessError::DeviceNotFound)
    }
    async fn revoke_device(&self, _: &DeviceId) -> Result<(), EnvironmentAccessError> {
        Err(EnvironmentAccessError::DeviceNotFound)
    }
}

/// Memory documents in a map, with the conditional write the page has.
#[derive(Default)]
struct Documents(Mutex<BTreeMap<String, String>>);

impl Documents {
    fn set(&self, path: &str, content: &str) {
        self.0
            .lock()
            .expect("documents")
            .insert(path.into(), content.into());
    }
    fn remove(&self, path: &str) {
        self.0.lock().expect("documents").remove(path);
    }
    fn get(&self, path: &str) -> Option<String> {
        self.0.lock().expect("documents").get(path).cloned()
    }
    fn paths(&self) -> Vec<String> {
        self.0.lock().expect("documents").keys().cloned().collect()
    }
    fn document(path: &str, content: String) -> MemoryDocument {
        MemoryDocument {
            path: path.into(),
            kind: memory_document_kind(path),
            content_hash: format!("h{}", items::sha256(content.as_bytes())),
            word_count: content.split_whitespace().count(),
            content,
            provenance: MemoryProvenance::default(),
        }
    }
}

#[async_trait]
impl MemoryDocumentsSource for Documents {
    async fn tree(
        &self,
        _: &ProductSurfaceCaller,
    ) -> Result<Vec<MemoryDocumentEntry>, ProductSurfaceError> {
        Ok(self
            .paths()
            .into_iter()
            .map(|path| MemoryDocumentEntry {
                kind: memory_document_kind(&path),
                path,
            })
            .collect())
    }
    async fn read(
        &self,
        _: &ProductSurfaceCaller,
        path: &str,
    ) -> Result<MemoryDocument, ProductSurfaceError> {
        self.get(path)
            .map(|content| Self::document(path, content))
            .ok_or_else(ProductSurfaceError::not_found)
    }
    async fn search(
        &self,
        _: &ProductSurfaceCaller,
        _: &str,
        _: usize,
    ) -> Result<Vec<MemorySearchHit>, ProductSurfaceError> {
        Ok(Vec::new())
    }
    async fn write(
        &self,
        _: &ProductSurfaceCaller,
        request: MemoryWriteRequest,
    ) -> Result<MemoryWriteResponse, ProductSurfaceError> {
        let held = self
            .get(&request.path)
            .map(|content| Self::document(&request.path, content));
        if held.as_ref().map(|d| d.content_hash.clone()) != request.expected_content_hash {
            return Ok(MemoryWriteResponse {
                outcome: MemoryPageWriteOutcome::Conflict,
                document: held,
            });
        }
        self.set(&request.path, &request.content);
        Ok(MemoryWriteResponse {
            outcome: MemoryPageWriteOutcome::Written,
            document: Some(Self::document(&request.path, request.content)),
        })
    }
    async fn archive(
        &self,
        _: &ProductSurfaceCaller,
        _: MemoryArchiveRequest,
    ) -> Result<MemoryArchiveResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
}

#[derive(Default)]
struct Bases(Mutex<BTreeMap<String, BTreeMap<String, String>>>);

#[async_trait]
impl MemorySyncBaseStore for Bases {
    async fn load(
        &self,
        _: &ProductSurfaceCaller,
        peer: &str,
    ) -> Result<BTreeMap<String, String>, ConfigSyncError> {
        Ok(self
            .0
            .lock()
            .expect("bases")
            .get(peer)
            .cloned()
            .unwrap_or_default())
    }
    async fn record(
        &self,
        _: &ProductSurfaceCaller,
        peer: &str,
        changes: &BTreeMap<String, BaseChange>,
    ) -> Result<(), ConfigSyncError> {
        let mut bases = self.0.lock().expect("bases");
        let base = bases.entry(peer.into()).or_default();
        for (path, change) in changes {
            if base.get(path) == change.seen.as_ref() {
                base.insert(path.clone(), change.agreed.clone());
            }
        }
        Ok(())
    }
}

/// One owner's usage document, kept in memory.
#[derive(Debug, Default)]
struct Usage(Mutex<SkillUsageDocument>);

#[async_trait]
impl SkillUsageStore for Usage {
    async fn load(
        &self,
        _: &SkillUsageOwner,
    ) -> Result<Option<SkillUsageDocument>, SkillUsageError> {
        Ok(Some(self.0.lock().expect("usage").clone()))
    }
    async fn record(
        &self,
        _: &SkillUsageOwner,
        source: &SkillUsageSource,
        observation: SkillUsageObservation,
        now_ms: u64,
    ) -> Result<(), SkillUsageError> {
        self.0
            .lock()
            .expect("usage")
            .apply(source, &observation, now_ms)
            .map(|_| ())
    }
    async fn import_ledger(
        &self,
        _: &SkillUsageOwner,
        source: &SkillUsageSource,
        ledger: SkillLedger,
    ) -> Result<(), SkillUsageError> {
        let mut document = self.0.lock().expect("usage");
        let record = document.sources.entry(source.clone()).or_default();
        for (name, used) in ledger {
            let entry = record.ledger.entry(name).or_default();
            *entry = entry.merged(used);
        }
        Ok(())
    }
    async fn set_pin(
        &self,
        _: &SkillUsageOwner,
        name: &SkillUsageName,
        pin: Option<SkillPin>,
    ) -> Result<SkillUsageDocument, SkillUsageError> {
        let mut document = self.0.lock().expect("usage");
        document.set_pin(name, pin)?;
        Ok(document.clone())
    }
}

struct Host {
    id: &'static str,
    documents: Arc<Documents>,
    usage: Arc<Usage>,
    skills: Arc<ScopedSkillManagementPort>,
    notes: Arc<dyn SyncNotes>,
    service: ConfigSyncService,
}

fn host(id: &'static str) -> Host {
    let documents = Arc::new(Documents::default());
    let usage = Arc::new(Usage::default());
    let skills = build_scoped_skill_management_port(
        UserId::new("alice").expect("user"),
        Arc::new(InMemoryBackend::new()),
    );
    let notes: Arc<dyn SyncNotes> = Arc::new(SettingsNotes::new(Arc::new(InMemoryBackend::new())));
    let service = ConfigSyncService::new(Arc::new(Identity(id)), "/data", "/home/alice")
        .with_handler(Arc::new(
            MemoryDomainHandler::new(
                documents.clone() as Arc<dyn MemoryDocumentsSource>,
                Arc::new(Bases::default()),
            )
            .with_index_ttl(Duration::ZERO),
        ))
        .with_handler(Arc::new(SkillUsageDomainHandler::new(
            SharedSkillUsage::new(usage.clone()),
        )))
        .with_handler(Arc::new(SkillsDomainHandler::new(
            skills.clone(),
            notes.clone(),
        )));
    Host {
        id,
        documents,
        usage,
        skills,
        notes,
        service,
    }
}

fn caller() -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("tenant-a").expect("tenant"),
        UserId::new("alice").expect("user"),
        None,
        None,
    )
}

fn scope() -> ResourceScope {
    ResourceScope {
        tenant_id: TenantId::new("tenant-a").expect("tenant"),
        user_id: UserId::new("alice").expect("user"),
        agent_id: None,
        project_id: None,
        mission_id: None,
        thread_id: None,
        invocation_id: InvocationId::new(),
    }
}

/// One leg of `domain` from `from` to `to`, the way the app carries it.
async fn leg(
    from: &Host,
    to: &Host,
    domain: ConfigSyncDomain,
    merge_back: bool,
) -> Vec<ConfigSyncApplyReport> {
    let mut reports = Vec::new();
    let mut have = BTreeMap::new();
    if domain.itemized() {
        let index = to
            .service
            .export(
                caller(),
                ConfigSyncExportRequest {
                    domains: vec![domain],
                    index_only: true,
                    ..Default::default()
                },
            )
            .await
            .expect("index");
        if let Some(ConfigSyncDomainPayload::Memory(page) | ConfigSyncDomainPayload::Skills(page)) =
            index.domains.get(&domain)
        {
            assert!(page.items.iter().all(|item| item.files.is_none()));
            have = page
                .items
                .iter()
                .map(|item| (item.key.clone(), item.hash.clone()))
                .collect();
        }
    }
    let mut after = None;
    loop {
        let snapshot = from
            .service
            .export(
                caller(),
                ConfigSyncExportRequest {
                    domains: vec![domain],
                    index_only: false,
                    have: have.clone(),
                    after: after.clone(),
                    until: None,
                    peer: Some(to.id.to_string()),
                },
            )
            .await
            .expect("export");
        after = match snapshot.domains.get(&domain) {
            Some(ConfigSyncDomainPayload::Memory(page) | ConfigSyncDomainPayload::Skills(page)) => {
                page.next.clone()
            }
            _ => None,
        };
        reports.push(
            to.service
                .apply(
                    caller(),
                    ConfigSyncApplyRequest {
                        snapshot,
                        domains: Vec::new(),
                        merge_back,
                    },
                )
                .await
                .expect("apply"),
        );
        if after.is_none() {
            return reports;
        }
    }
}

/// A whole pass: there, and back for a two-way domain.
async fn sync(mac: &Host, boxed: &Host, domain: ConfigSyncDomain) -> Vec<String> {
    let mut reports = leg(mac, boxed, domain, false).await;
    if domain.two_way() {
        reports.extend(leg(boxed, mac, domain, true).await);
        // Something moved: one more round, which carries nothing and lets
        // each side record what both now hold.
        if domain.itemized()
            && reports
                .iter()
                .any(|r| r.domains.values().any(|o| o.applied_count > 0))
        {
            reports.extend(leg(mac, boxed, domain, false).await);
            reports.extend(leg(boxed, mac, domain, true).await);
        }
    }
    reports
        .into_iter()
        .flat_map(|report| {
            report
                .domains
                .into_values()
                .flat_map(|outcome| outcome.warnings)
        })
        .collect()
}

async fn hash(host: &Host, domain: ConfigSyncDomain) -> String {
    host.service
        .manifest(caller())
        .await
        .expect("manifest")
        .domains[&domain]
        .hash
        .clone()
}

const MAC: &str = "0f6c2d0e-3b0a-4a51-9b7e-6b1c8f1f2a10";
const BOX: &str = "1a7d3e1f-4c1b-4b62-8c8f-7c2d9a2a3b21";
const MEMORY: ConfigSyncDomain = ConfigSyncDomain::Memory;

#[tokio::test]
async fn memory_written_on_either_computer_reaches_the_other_and_nothing_is_lost() {
    let (mac, boxed) = (host(MAC), host(BOX));
    mac.documents
        .set("MEMORY.md", "The person prefers short answers.\n");
    mac.documents.set("notes/mac.md", "written on the Mac\n");
    boxed.documents.set("notes/box.md", "written on the box\n");

    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    for host in [&mac, &boxed] {
        assert_eq!(
            host.documents.paths(),
            ["MEMORY.md", "notes/box.md", "notes/mac.md"],
            "{}",
            host.id
        );
    }
    assert_eq!(hash(&mac, MEMORY).await, hash(&boxed, MEMORY).await);

    // An agent on the box changes one, the person changes another on the
    // Mac: each change moves to the other side and neither is undone.
    boxed.documents.set(
        "MEMORY.md",
        "The person prefers short answers.\nThey deploy on Fridays.\n",
    );
    mac.documents.set("notes/mac.md", "rewritten on the Mac\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    for host in [&mac, &boxed] {
        assert_eq!(
            host.documents.get("MEMORY.md").as_deref(),
            Some("The person prefers short answers.\nThey deploy on Fridays.\n"),
            "{}",
            host.id
        );
        assert_eq!(
            host.documents.get("notes/mac.md").as_deref(),
            Some("rewritten on the Mac\n")
        );
    }

    // Removed on the box: it stays removed there and stays on the Mac.
    boxed.documents.remove("notes/mac.md");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    assert_eq!(boxed.documents.get("notes/mac.md"), None);
    assert!(mac.documents.get("notes/mac.md").is_some());
}

#[tokio::test]
async fn a_document_changed_on_both_computers_is_kept_twice() {
    let (mac, boxed) = (host(MAC), host(BOX));
    mac.documents.set("notes/plan.md", "v1\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());

    mac.documents.set("notes/plan.md", "the Mac's plan\n");
    boxed.documents.set("notes/plan.md", "the box's plan\n");
    let warnings = sync(&mac, &boxed, MEMORY).await;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains("changed on both computers"),
        "{warnings:?}"
    );

    for host in [&mac, &boxed] {
        assert_eq!(
            host.documents.get("notes/plan.md").as_deref(),
            Some("the Mac's plan\n"),
            "{}",
            host.id
        );
        let kept: Vec<String> = host
            .documents
            .paths()
            .into_iter()
            .filter(|path| path.starts_with("sync-conflicts/"))
            .collect();
        assert_eq!(kept.len(), 1, "{}: {kept:?}", host.id);
        assert!(kept[0].ends_with("/notes/plan.md"));
        assert_eq!(
            host.documents.get(&kept[0]).as_deref(),
            Some("the box's plan\n")
        );
    }
    assert_eq!(hash(&mac, MEMORY).await, hash(&boxed, MEMORY).await);
    // Settled: another pass changes nothing and warns of nothing.
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    assert_eq!(mac.documents.paths().len(), 2);

    // Both appended to a log before ever agreeing on it: the longer text
    // continues the shorter, so it wins without a copy.
    mac.documents.set("daily/2026-11-26.md", "09:00 started\n");
    boxed
        .documents
        .set("daily/2026-11-26.md", "09:00 started\n10:00 shipped\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    assert_eq!(
        mac.documents.get("daily/2026-11-26.md").as_deref(),
        Some("09:00 started\n10:00 shipped\n")
    );
}

#[tokio::test]
async fn a_large_memory_travels_in_pages() {
    let (mac, boxed) = (host(MAC), host(BOX));
    // Three documents of 900 KiB: more than one page holds.
    for name in ["a", "b", "c"] {
        mac.documents
            .set(&format!("notes/{name}.md"), &name.repeat(900 * 1024));
    }
    let reports = leg(&mac, &boxed, MEMORY, false).await;
    assert!(reports.len() > 1, "one page carried everything");
    assert_eq!(boxed.documents.paths().len(), 3);
    assert_eq!(hash(&mac, MEMORY).await, hash(&boxed, MEMORY).await);
}

fn name(raw: &str) -> SkillUsageName {
    SkillUsageName::new(raw).expect("skill name")
}

#[tokio::test]
async fn skill_uses_merge_both_ways_and_pins_follow_the_mac() {
    let (mac, boxed) = (host(MAC), host(BOX));
    let harness: SkillUsageSource =
        serde_json::from_value(serde_json::json!("harness")).expect("source");
    let used = |count, last_used_ms| SkillUse {
        count,
        last_used_ms,
    };
    {
        let mut document = mac.usage.0.lock().expect("usage");
        document
            .sources
            .entry(harness.clone())
            .or_default()
            .ledger
            .insert(name("deploy"), used(3, 100));
        document.pins.insert(name("deploy"), SkillPin::AlwaysFull);
    }
    {
        let mut document = boxed.usage.0.lock().expect("usage");
        let ledger = &mut document.sources.entry(harness.clone()).or_default().ledger;
        ledger.insert(name("deploy"), used(7, 50));
        ledger.insert(name("review"), used(2, 300));
        document.pins.insert(name("old"), SkillPin::Off);
    }

    assert!(
        sync(&mac, &boxed, ConfigSyncDomain::SkillUsage)
            .await
            .is_empty()
    );
    for host in [&mac, &boxed] {
        let document = host.usage.0.lock().expect("usage").clone();
        let ledger = &document.sources[&harness].ledger;
        assert_eq!(ledger[&name("deploy")], used(7, 100), "{}", host.id);
        assert_eq!(ledger[&name("review")], used(2, 300), "{}", host.id);
        // The Mac's pins are the pins: its own kept, the box's own gone.
        assert_eq!(
            document.pins,
            BTreeMap::from([(name("deploy"), SkillPin::AlwaysFull)]),
            "{}",
            host.id
        );
    }
    assert_eq!(
        hash(&mac, ConfigSyncDomain::SkillUsage).await,
        hash(&boxed, ConfigSyncDomain::SkillUsage).await
    );
}

const DEPLOY: &str = "---\nname: deploy\ndescription: ship it\n---\n# Deploy\n";

#[tokio::test]
async fn skills_arrive_whole_and_the_boxs_own_are_kept() {
    let (mac, boxed) = (host(MAC), host(BOX));
    mac.skills
        .install_for_scope(scope(), None, DEPLOY)
        .await
        .expect("install on the mac");
    boxed
        .skills
        .install_for_scope(
            scope(),
            None,
            "---\nname: deploy\ndescription: old\n---\n# Old\n",
        )
        .await
        .expect("install on the box");
    boxed
        .skills
        .install_for_scope(
            scope(),
            None,
            "---\nname: box-only\ndescription: local\n---\n# Local\n",
        )
        .await
        .expect("install on the box");

    assert!(
        sync(&mac, &boxed, ConfigSyncDomain::Skills)
            .await
            .is_empty()
    );
    assert_eq!(
        boxed
            .skills
            .read_content_for_scope(scope(), "deploy")
            .await
            .expect("deploy")
            .content,
        DEPLOY
    );
    assert!(
        boxed
            .skills
            .read_content_for_scope(scope(), "box-only")
            .await
            .is_ok(),
        "a skill only the box has is kept"
    );
    // The Mac was only read.
    assert_eq!(
        mac.skills
            .list_for_scope(scope())
            .await
            .expect("list")
            .len(),
        1
    );
    // Nothing left to apply.
    let again = leg(&mac, &boxed, ConfigSyncDomain::Skills, false).await;
    assert!(
        again
            .iter()
            .all(|r| r.domains[&ConfigSyncDomain::Skills].applied_count == 0)
    );
}

#[tokio::test]
async fn an_itemized_domain_is_never_part_of_an_everything_export() {
    let mac = host(MAC);
    mac.documents.set("MEMORY.md", "x\n");
    let all = mac
        .service
        .export(caller(), ConfigSyncExportRequest::default())
        .await
        .expect("export");
    assert_eq!(
        all.domains.keys().copied().collect::<Vec<_>>(),
        [ConfigSyncDomain::SkillUsage]
    );
}

#[tokio::test]
async fn an_edit_that_lands_during_a_pass_is_never_written_over() {
    let (mac, boxed) = (host(MAC), host(BOX));
    mac.documents.set("MEMORY.md", "v0\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());

    // The Mac moves on to v1; the page that carries it is cut, and before
    // it is applied an agent on the box writes its own v2.
    mac.documents.set("MEMORY.md", "v1 from the Mac\n");
    let snapshot = mac
        .service
        .export(
            caller(),
            ConfigSyncExportRequest {
                domains: vec![MEMORY],
                ..Default::default()
            },
        )
        .await
        .expect("export");
    boxed.documents.set("MEMORY.md", "v2 from the box\n");
    let report = boxed
        .service
        .apply(
            caller(),
            ConfigSyncApplyRequest {
                snapshot,
                domains: Vec::new(),
                merge_back: false,
            },
        )
        .await
        .expect("apply");
    // Seen with the edit in view: both changed, so the box's text is kept
    // aside before the Mac's takes the path. Nothing is lost.
    let outcome = &report.domains[&MEMORY];
    assert!(
        outcome
            .warnings
            .iter()
            .any(|w| w.contains("changed on both")),
        "{outcome:?}"
    );
    let kept: Vec<String> = boxed
        .documents
        .paths()
        .into_iter()
        .filter(|path| path.starts_with("sync-conflicts/"))
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(
        boxed.documents.get(&kept[0]).as_deref(),
        Some("v2 from the box\n")
    );
}

#[tokio::test]
async fn an_edited_conflict_copy_is_never_written_over() {
    let (mac, boxed) = (host(MAC), host(BOX));
    mac.documents.set("notes/plan.md", "v1\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    mac.documents.set("notes/plan.md", "the Mac's plan\n");
    boxed.documents.set("notes/plan.md", "the box's plan\n");
    assert_eq!(sync(&mac, &boxed, MEMORY).await.len(), 1);
    let copy = boxed
        .documents
        .paths()
        .into_iter()
        .find(|path| path.starts_with("sync-conflicts/"))
        .expect("copy");

    // An agent rewrites the copy, and later the same conflict happens
    // again with the same losing text.
    for host in [&mac, &boxed] {
        host.documents
            .set(&copy, "the agent's notes on the conflict\n");
    }
    boxed.documents.set("notes/plan.md", "the box's plan\n");
    mac.documents.set("notes/plan.md", "the Mac's newer plan\n");
    let warnings = sync(&mac, &boxed, MEMORY).await;
    assert!(
        warnings.iter().any(|w| w.contains("changed on both")),
        "{warnings:?}"
    );
    assert_eq!(
        boxed.documents.get(&copy).as_deref(),
        Some("the agent's notes on the conflict\n"),
        "the edited copy is theirs"
    );
    let copies: Vec<String> = boxed
        .documents
        .paths()
        .into_iter()
        .filter(|path| path.starts_with("sync-conflicts/"))
        .collect();
    assert_eq!(copies.len(), 2, "{copies:?}");
    assert!(
        copies
            .iter()
            .any(|path| boxed.documents.get(path).as_deref() == Some("the box's plan\n"))
    );
}

#[tokio::test]
async fn a_document_archived_before_the_computers_ever_synced_stays_archived() {
    let (mac, boxed) = (host(MAC), host(BOX));
    // Both already hold the same document; no pass has recorded that yet.
    mac.documents.set("notes/old.md", "old\n");
    // The box archived its copy (the provider keeps it under archive/).
    boxed
        .documents
        .set("archive/20261126T100000Z/notes/old.md", "old\n");

    // Another was edited on the box and then archived there: its archived
    // text is not the Mac's, and it is a removal all the same.
    mac.documents.set("notes/draft.md", "first draft\n");
    boxed
        .documents
        .set("archive/20261126T110000Z/notes/draft.md", "second draft\n");
    // A document elsewhere that merely ends the same way is not confused
    // with it.
    mac.documents.set("old.md", "another document\n");

    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    assert_eq!(boxed.documents.get("notes/old.md"), None, "resurrected");
    assert_eq!(boxed.documents.get("notes/draft.md"), None, "resurrected");
    assert_eq!(
        boxed.documents.get("old.md").as_deref(),
        Some("another document\n")
    );
    // The archive is the box's own: it is not carried to the Mac.
    assert!(
        mac.documents
            .paths()
            .iter()
            .all(|path| !path.starts_with("archive/"))
    );
    // And it stays that way on the next pass.
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    assert_eq!(boxed.documents.get("notes/old.md"), None);
}

#[tokio::test]
async fn a_replaced_skill_loses_its_stale_files_and_keeps_loading() {
    let (mac, boxed) = (host(MAC), host(BOX));
    let files = |extra: &str| {
        vec![
            ("SKILL.md".to_string(), DEPLOY.as_bytes().to_vec()),
            (format!("scripts/{extra}.sh"), b"echo hi\n".to_vec()),
        ]
    };
    boxed
        .skills
        .replace_bundle_for_scope(scope(), "deploy", files("old"))
        .await
        .expect("first bundle");
    mac.skills
        .replace_bundle_for_scope(scope(), "deploy", files("new"))
        .await
        .expect("mac bundle");
    let before = items::files_hash(
        &boxed
            .skills
            .export_bundle_for_scope(scope(), "deploy")
            .await
            .expect("bundle"),
    );
    assert!(
        sync(&mac, &boxed, ConfigSyncDomain::Skills)
            .await
            .is_empty()
    );
    let mut names: Vec<String> = boxed
        .skills
        .export_bundle_for_scope(scope(), "deploy")
        .await
        .expect("bundle")
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    names.sort();
    assert_eq!(names, ["SKILL.md", "scripts/new.sh"]);
    // The box's own bundle, with the file only it had, was kept first.
    let short: String = before
        .trim_start_matches("sha256:")
        .chars()
        .take(16)
        .collect();
    let copy = boxed
        .notes
        .read(&["skill-backups", "tenant-a", "alice", "deploy", &short])
        .await
        .expect("read")
        .expect("a copy of the replaced bundle");
    let copy: Vec<serde_json::Value> = serde_json::from_slice(&copy).expect("json");
    assert!(
        copy.iter().any(|file| file["path"] == "scripts/old.sh"),
        "{copy:?}"
    );
    // A bundle with a path that leaves the skill is refused before anything
    // of the skill in place is touched.
    let bad = vec![
        ("SKILL.md".to_string(), DEPLOY.as_bytes().to_vec()),
        ("../escape.sh".to_string(), b"x".to_vec()),
    ];
    assert!(
        boxed
            .skills
            .replace_bundle_for_scope(scope(), "deploy", bad)
            .await
            .is_err()
    );
    assert_eq!(
        boxed
            .skills
            .export_bundle_for_scope(scope(), "deploy")
            .await
            .expect("bundle")
            .len(),
        2
    );
}

#[tokio::test]
async fn a_change_back_to_an_earlier_text_still_moves() {
    let (mac, boxed) = (host(MAC), host(BOX));
    mac.documents.set("notes/a.md", "X\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    // The box moves to Y, which reaches the Mac.
    boxed.documents.set("notes/a.md", "Y\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    assert_eq!(mac.documents.get("notes/a.md").as_deref(), Some("Y\n"));
    // The Mac goes back to X: a real edit since both agreed on Y, so it
    // moves, although X is also what they agreed on before.
    mac.documents.set("notes/a.md", "X\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    assert_eq!(boxed.documents.get("notes/a.md").as_deref(), Some("X\n"));
    assert_eq!(hash(&mac, MEMORY).await, hash(&boxed, MEMORY).await);
}

#[tokio::test]
async fn a_skill_file_that_would_become_a_folder_is_refused_and_the_skill_stays() {
    let boxed = host(BOX);
    let bundle = |extra: &str| {
        vec![
            ("SKILL.md".to_string(), DEPLOY.as_bytes().to_vec()),
            (extra.to_string(), b"x\n".to_vec()),
        ]
    };
    boxed
        .skills
        .replace_bundle_for_scope(scope(), "deploy", bundle("scripts/tool"))
        .await
        .expect("file");
    for swapped in ["scripts/tool/run.sh", "scripts"] {
        assert!(
            boxed
                .skills
                .replace_bundle_for_scope(scope(), "deploy", bundle(swapped))
                .await
                .is_err(),
            "{swapped}"
        );
        let mut names: Vec<String> = boxed
            .skills
            .export_bundle_for_scope(scope(), "deploy")
            .await
            .expect("bundle")
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        names.sort();
        assert_eq!(names, ["SKILL.md", "scripts/tool"], "untouched");
    }
}

#[tokio::test]
async fn a_pass_cut_short_does_not_strand_the_next_edit() {
    let (mac, boxed) = (host(MAC), host(BOX));
    mac.documents.set("notes/a.md", "v0\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    // The Mac's v1 reaches the box, and the pass ends there: the Mac never
    // records that the box took it.
    mac.documents.set("notes/a.md", "v1\n");
    leg(&mac, &boxed, MEMORY, false).await;
    assert_eq!(boxed.documents.get("notes/a.md").as_deref(), Some("v1\n"));
    // The box then rewrites it (not an append).
    boxed.documents.set("notes/a.md", "rewritten on the box\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    for host in [&mac, &boxed] {
        assert_eq!(
            host.documents.get("notes/a.md").as_deref(),
            Some("rewritten on the box\n"),
            "{}",
            host.id
        );
    }
}

#[tokio::test]
async fn a_document_archived_after_it_was_agreed_stays_archived_when_the_other_side_edits_it() {
    let (mac, boxed) = (host(MAC), host(BOX));
    mac.documents.set("notes/a.md", "v0\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    // Archived on the box, then edited on the Mac.
    boxed.documents.remove("notes/a.md");
    boxed
        .documents
        .set("archive/20261126T120000Z/notes/a.md", "v0\n");
    mac.documents.set("notes/a.md", "v1\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    assert_eq!(boxed.documents.get("notes/a.md"), None, "resurrected");
    assert_eq!(mac.documents.get("notes/a.md").as_deref(), Some("v1\n"));
}

#[tokio::test]
async fn a_skill_that_holds_a_key_is_carried_neither_out_nor_over() {
    let (mac, boxed) = (host(MAC), host(BOX));
    let key = b"-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj\n-----END PRIVATE KEY-----\n".to_vec();
    let bundle_of = |name: &str, extra: (&str, Vec<u8>)| {
        let manifest = format!("---\nname: {name}\ndescription: test\n---\n# {name}\n");
        vec![
            ("SKILL.md".to_string(), manifest.into_bytes()),
            (extra.0.to_string(), extra.1),
        ]
    };
    // On the Mac: a skill with a key file is not exported, and says so.
    mac.skills
        .replace_bundle_for_scope(
            scope(),
            "deploy",
            bundle_of("deploy", ("deploy.pem", key.clone())),
        )
        .await
        .expect("mac skill");
    let warnings = sync(&mac, &boxed, ConfigSyncDomain::Skills).await;
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("deploy.pem"), "{warnings:?}");
    assert!(
        boxed
            .skills
            .read_content_for_scope(scope(), "deploy")
            .await
            .is_err()
    );

    // On the box: a skill that holds a key of its own is not replaced.
    mac.skills
        .replace_bundle_for_scope(
            scope(),
            "review",
            bundle_of("review", ("notes.md", b"new\n".to_vec())),
        )
        .await
        .expect("mac skill");
    boxed
        .skills
        .replace_bundle_for_scope(
            scope(),
            "review",
            bundle_of("review", ("notes.md", key.clone())),
        )
        .await
        .expect("box skill");
    let warnings = sync(&mac, &boxed, ConfigSyncDomain::Skills).await;
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("the one here holds a key")),
        "{warnings:?}"
    );
    let kept = boxed
        .skills
        .export_bundle_for_scope(scope(), "review")
        .await
        .expect("bundle");
    assert!(
        kept.iter().any(|(_, bytes)| *bytes == key),
        "the key is still there"
    );

    // A bundle that names a path as a file and as a folder is refused
    // before anything of the skill in place is touched.
    let malformed = vec![
        ("SKILL.md".to_string(), DEPLOY.as_bytes().to_vec()),
        ("notes.md".to_string(), b"changed\n".to_vec()),
        ("z".to_string(), b"x".to_vec()),
        ("z/run.sh".to_string(), b"x".to_vec()),
    ];
    assert!(
        boxed
            .skills
            .replace_bundle_for_scope(scope(), "review", malformed)
            .await
            .is_err()
    );
    let after = boxed
        .skills
        .export_bundle_for_scope(scope(), "review")
        .await
        .expect("bundle");
    assert!(after.iter().any(|(_, bytes)| *bytes == key), "untouched");
}

#[tokio::test]
async fn conversation_records_are_each_computers_own() {
    let (mac, boxed) = (host(MAC), host(BOX));
    mac.documents
        .set("threads/t1/run-1.md", "user: hi\nassistant: hello\n");
    mac.documents.set("MEMORY.md", "kept\n");
    assert!(sync(&mac, &boxed, MEMORY).await.is_empty());
    assert_eq!(boxed.documents.paths(), ["MEMORY.md"]);
    assert_eq!(hash(&mac, MEMORY).await, hash(&boxed, MEMORY).await);
}

struct NoTools;

#[async_trait]
impl ironclaw_product_contracts::operator_tools::RebornOperatorToolCatalog for NoTools {
    async fn list_operator_tools(
        &self,
        _: &UserId,
    ) -> Vec<ironclaw_product_contracts::operator_tools::RebornOperatorToolInfo> {
        Vec::new()
    }
}

#[tokio::test]
async fn a_restriction_the_mac_resets_is_taken_back_and_the_boxs_own_are_kept() {
    use ironclaw_approvals::test_support::{
        in_memory_backed_auto_approve_setting_store,
        in_memory_backed_capability_permission_override_store,
        in_memory_backed_persistent_approval_policy_store,
    };
    use ironclaw_approvals::{
        CapabilityPermissionOverride, CapabilityPermissionOverrideInput,
        CapabilityPermissionOverrideKey, CapabilityPermissionOverrideStorePort,
    };
    use ironclaw_host_api::ids::CapabilityId;
    use ironclaw_host_api::scope::Principal;

    const APPROVALS: ConfigSyncDomain = ConfigSyncDomain::Approvals;
    struct Side {
        overrides: Arc<dyn CapabilityPermissionOverrideStorePort>,
        service: ConfigSyncService,
    }
    let side = |id: &'static str| {
        let overrides: Arc<dyn CapabilityPermissionOverrideStorePort> =
            Arc::new(in_memory_backed_capability_permission_override_store());
        let notes: Arc<dyn SyncNotes> =
            Arc::new(SettingsNotes::new(Arc::new(InMemoryBackend::new())));
        let service = ConfigSyncService::new(Arc::new(Identity(id)), "/data", "/home/alice")
            .with_handler(Arc::new(ApprovalsDomainHandler::new(
                Arc::new(in_memory_backed_auto_approve_setting_store()),
                overrides.clone(),
                Arc::new(in_memory_backed_persistent_approval_policy_store()),
                Arc::new(NoTools),
                notes,
            )));
        Side { overrides, service }
    };
    let (mac, boxed) = (side(MAC), side(BOX));
    let set = |side: &Side, raw: &str, state: Option<CapabilityPermissionOverride>| {
        let overrides = side.overrides.clone();
        let capability_id = CapabilityId::new(raw).expect("capability");
        async move {
            match state {
                Some(state) => {
                    overrides
                        .set(CapabilityPermissionOverrideInput {
                            scope: scope(),
                            capability_id,
                            state,
                            updated_by: Principal::User(UserId::new("alice").expect("user")),
                        })
                        .await
                        .expect("set");
                }
                None => {
                    overrides
                        .clear(&CapabilityPermissionOverrideKey::new(
                            &scope(),
                            capability_id,
                        ))
                        .await
                        .expect("clear");
                }
            }
        }
    };
    let held = |side: &Side| {
        let overrides = side.overrides.clone();
        async move {
            let mut out: Vec<String> = overrides
                .list_for_scope(&scope())
                .await
                .expect("list")
                .into_iter()
                .map(|record| record.key.capability_id.as_str().to_string())
                .collect();
            out.sort();
            out
        }
    };
    let pass = || async {
        let snapshot = mac
            .service
            .export(
                caller(),
                ConfigSyncExportRequest {
                    domains: vec![APPROVALS],
                    ..Default::default()
                },
            )
            .await
            .expect("export");
        boxed
            .service
            .apply(
                caller(),
                ConfigSyncApplyRequest {
                    snapshot,
                    domains: Vec::new(),
                    merge_back: false,
                },
            )
            .await
            .expect("apply");
    };

    let off = Some(CapabilityPermissionOverride::Disabled);
    set(&mac, "demo.shared", off).await;
    set(&mac, "demo.edited", off).await;
    set(&mac, "demo.set_again", off).await;
    set(&boxed, "demo.box_only", off).await;
    pass().await;
    assert_eq!(
        held(&boxed).await,
        [
            "demo.box_only",
            "demo.edited",
            "demo.set_again",
            "demo.shared"
        ]
    );
    // On the box one is put back to default and then switched off again:
    // the same state as sync left, and the box's own decision now.
    tokio::time::sleep(Duration::from_millis(20)).await;
    set(&boxed, "demo.set_again", None).await;
    set(&boxed, "demo.set_again", off).await;

    // The box changes one that sync made; the Mac then resets both of its.
    set(
        &boxed,
        "demo.edited",
        Some(CapabilityPermissionOverride::AskEachTime),
    )
    .await;
    set(&mac, "demo.shared", None).await;
    set(&mac, "demo.edited", None).await;
    set(&mac, "demo.set_again", None).await;
    pass().await;
    // Taken back: the one still as sync made it. Kept: the box's own, the
    // one the box changed, and the one it set again itself.
    assert_eq!(
        held(&boxed).await,
        ["demo.box_only", "demo.edited", "demo.set_again"]
    );
}
