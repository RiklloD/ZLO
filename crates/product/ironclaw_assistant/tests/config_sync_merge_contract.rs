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
use ironclaw_assistant::config_sync::memory::{MemoryDomainHandler, MemorySyncBaseStore};
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
    async fn save(
        &self,
        _: &ProductSurfaceCaller,
        peer: &str,
        base: &BTreeMap<String, String>,
    ) -> Result<(), ConfigSyncError> {
        self.0
            .lock()
            .expect("bases")
            .insert(peer.into(), base.clone());
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
    service: ConfigSyncService,
}

fn host(id: &'static str) -> Host {
    let documents = Arc::new(Documents::default());
    let usage = Arc::new(Usage::default());
    let skills = build_scoped_skill_management_port(
        UserId::new("alice").expect("user"),
        Arc::new(InMemoryBackend::new()),
    );
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
        .with_handler(Arc::new(SkillsDomainHandler::new(skills.clone())));
    Host {
        id,
        documents,
        usage,
        skills,
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
