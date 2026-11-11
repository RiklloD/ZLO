use ironclaw_filesystem::{Fault, FaultInjecting, FilesystemOperation, InMemoryBackend};
use ironclaw_host_api::{host_port::HostPortCatalog, ids::ExtensionId, path::VirtualPath};

use super::*;
use crate::ManifestSource;
use crate::installations::test_support::*;

#[tokio::test]
async fn load_migrates_pre_resolved_manifest_rows_before_projection() {
    let backend = Arc::new(InMemoryBackend::new());
    let root =
        VirtualPath::new("/system/extensions/.installations/migration-test").expect("valid root");
    let store = ExtensionInstallationStore::load_at(
        backend.clone(),
        root.clone(),
        HostPortCatalog::empty(),
        capability_provider_contracts(),
    )
    .await
    .expect("initial store");
    let record = manifest_record("legacy-fixture", Some("hash-legacy"));
    let path = store
        .manifest_path(record.extension_id())
        .expect("manifest path");
    let wire = WireManifestRecord {
        raw_toml: record.raw_toml().to_string(),
        source: WireManifestSource::from_manifest_source(record.manifest().source),
        resolved: None,
        manifest_hash: record.manifest_hash().cloned(),
        removal_cleanup_requirements: Vec::new(),
        definition_retention: PackageDefinitionRetention::RemoveWithLastInstallation,
    };
    let payload = serde_json::to_value(wire).expect("legacy wire payload");
    let entry = Entry::record(
        record_kind(MANIFEST_RECORD_KIND).expect("record kind"),
        &payload,
    )
    .expect("legacy manifest entry")
    .with_indexed(
        index_key("extension_id").expect("index key"),
        IndexValue::Text(record.extension_id().as_str().to_string()),
    )
    .with_indexed(
        index_key("manifest_source").expect("index key"),
        IndexValue::Text("host_bundled".to_string()),
    );
    backend
        .put(&path, entry, CasExpectation::Any)
        .await
        .expect("seed legacy row");
    // The merged v2 record imports legacy state as a manifest+installation
    // pair; an orphan legacy manifest is a v1 crash artifact and stays
    // uninstalled, so pair the seeded wire with its installation row.
    store
        .put_installation(
            &installation("legacy-fixture", Some("hash-legacy")),
            CasExpectation::Any,
        )
        .await
        .expect("seed legacy installation row");
    drop(store);

    let reopened = ExtensionInstallationStore::load_at(
        backend,
        root,
        HostPortCatalog::empty(),
        capability_provider_contracts(),
    )
    .await
    .expect("migration succeeds");
    let loaded = reopened
        .get_manifest(record.extension_id())
        .await
        .expect("load migrated row")
        .expect("migrated row exists");
    assert_eq!(loaded.resolved(), record.resolved());
}

#[tokio::test]
async fn delete_manifest_rejects_active_installations() {
    let store = installation_store().await;
    let manifest = manifest_record("fixture", Some("hash-1"));
    let extension_id = manifest.extension_id().clone();
    store
        .upsert_manifest_and_installation(manifest, installation("fixture", Some("hash-1")))
        .await
        .expect("install fixture");

    let error = store
        .delete_manifest(&extension_id)
        .await
        .expect_err("active installation blocks manifest delete");

    assert!(matches!(
        error,
        ExtensionInstallationError::InvalidInstallation { .. }
    ));
    assert!(store.get_manifest(&extension_id).await.unwrap().is_some());
}

#[tokio::test]
async fn package_definition_admission_is_immutable_and_exactly_idempotent() {
    let store = installation_store().await;
    let record = manifest_record("registered", Some("hash-one"))
        .with_definition_retention(PackageDefinitionRetention::RetainInCatalog);

    assert_eq!(
        store
            .admit_package_definition(record.clone())
            .await
            .expect("create definition"),
        PackageDefinitionAdmissionOutcome::Created
    );
    assert_eq!(
        store
            .admit_package_definition(record.clone())
            .await
            .expect("replay exact definition"),
        PackageDefinitionAdmissionOutcome::ExactExisting
    );

    let mut changed_semantics = record.resolved().clone();
    changed_semantics.description = "changed description".to_string();
    let mut changed_root = record.resolved().clone();
    changed_root.root_binding = PackageRootBinding::Materialized(
        VirtualPath::new("/system/extensions/registered").expect("package root"),
    );
    let conflicts = [
        manifest_record("registered", Some("hash-two"))
            .with_definition_retention(PackageDefinitionRetention::RetainInCatalog),
        manifest_record("registered", Some("hash-one")),
        ExtensionManifestRecord::from_resolved(
            format!("{}\n# byte drift", record.raw_toml()),
            record.manifest().source,
            record.resolved().clone(),
            record.manifest_hash().cloned(),
        )
        .expect("raw-different record")
        .with_definition_retention(PackageDefinitionRetention::RetainInCatalog),
        ExtensionManifestRecord::from_resolved(
            record.raw_toml(),
            record.manifest().source,
            changed_semantics,
            record.manifest_hash().cloned(),
        )
        .expect("semantic-different record")
        .with_definition_retention(PackageDefinitionRetention::RetainInCatalog),
        ExtensionManifestRecord::from_resolved(
            record.raw_toml(),
            record.manifest().source,
            changed_root,
            record.manifest_hash().cloned(),
        )
        .expect("root-different record")
        .with_definition_retention(PackageDefinitionRetention::RetainInCatalog),
    ];
    for conflict in conflicts {
        let error = store
            .admit_package_definition(conflict)
            .await
            .expect_err("different definition conflicts");
        assert!(matches!(
            error,
            ExtensionInstallationError::PackageDefinitionConflict { .. }
        ));
    }
    assert_eq!(
        store
            .get_registered_package_definition(record.extension_id())
            .await
            .expect("read admitted definition"),
        Some(record)
    );
    assert!(
        store
            .list_installations()
            .await
            .expect("installations")
            .is_empty()
    );
}

#[tokio::test]
async fn registered_definition_survives_its_final_installation_removal() {
    let store = installation_store().await;
    let record = manifest_record("retained", Some("hash-one"))
        .with_definition_retention(PackageDefinitionRetention::RetainInCatalog);
    store
        .admit_package_definition(record.clone())
        .await
        .expect("admit definition");
    let installed = installation("retained", Some("hash-one"));
    let installation_id = installed.installation_id().clone();
    store
        .upsert_manifest_and_installation(record.clone(), installed)
        .await
        .expect("install");
    store
        .delete_installation(&installation_id)
        .await
        .expect("remove final installation");

    assert_eq!(
        store
            .get_registered_package_definition(record.extension_id())
            .await
            .expect("retained definition"),
        Some(record)
    );
}

#[tokio::test]
async fn registered_definition_accessor_ignores_legacy_installation_manifests() {
    let store = installation_store().await;
    let record = manifest_record("legacy-only", Some("hash-one"));
    store
        .upsert_manifest_and_installation(
            record.clone(),
            installation("legacy-only", Some("hash-one")),
        )
        .await
        .expect("install legacy-shaped package");

    assert!(
        store
            .get_manifest(record.extension_id())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        store
            .get_registered_package_definition(record.extension_id())
            .await
            .expect("registered definition lookup"),
        None
    );
}

#[test]
fn legacy_definition_wire_defaults_to_remove_with_last_installation() {
    let record = manifest_record("legacy-retention", Some("hash-one"));
    let mut payload = serde_json::to_value(WireManifestRecord::from(&record)).expect("wire");
    payload
        .as_object_mut()
        .expect("wire object")
        .remove("definition_retention");
    let restored: WireManifestRecord = serde_json::from_value(payload).expect("legacy wire");

    assert_eq!(
        restored
            .into_manifest_record()
            .expect("manifest record")
            .definition_retention(),
        PackageDefinitionRetention::RemoveWithLastInstallation
    );
}

#[tokio::test]
async fn finalization_swaps_only_the_matching_pending_aggregate() {
    let store = installation_store().await;
    let pending_manifest = pending_manifest_record("fixture", Some("hash-pending"));
    let pending = pending_installation("fixture", Some("hash-pending"));
    let installation_id = pending.installation_id().clone();
    let incarnation_id = pending
        .incarnation_id()
        .cloned()
        .expect("fresh incarnation");
    let pending_ref = pending.manifest_ref().clone();
    store
        .upsert_manifest_and_installation(pending_manifest, pending)
        .await
        .expect("persist pending aggregate");

    let finalized = manifest_record("fixture", Some("hash-final"));
    let winner = store
        .finalize_preparation(&installation_id, &incarnation_id, &pending_ref, finalized)
        .await
        .expect("matching finalizer wins");
    assert_eq!(
        winner
            .manifest_ref()
            .manifest_hash()
            .map(ManifestHash::as_str),
        Some("hash-final")
    );
    assert_eq!(winner.incarnation_id(), Some(&incarnation_id));

    // Regression: take_v2_preparation_lease's (narrower) checks can pass
    // and commit the lease while finish_v2_preparation's real validation
    // (lease match, extension-id match, finalized_is_ready) still
    // rejects the finalize. That must not strand the aggregate under a
    // permanently held lease — is_visible() requires lease.is_none(), so
    // a stranded lease would make get_installation return None until a
    // store-construction repair pass (repair_interrupted_v2_leases)
    // runs, which only happens on restart.
    let stranded_manifest = pending_manifest_record("stranded", Some("hash-pending"));
    let stranded = pending_installation("stranded", Some("hash-pending"));
    let stranded_id = stranded.installation_id().clone();
    let stranded_incarnation = stranded
        .incarnation_id()
        .cloned()
        .expect("fresh incarnation");
    let stranded_ref = stranded.manifest_ref().clone();
    store
        .upsert_manifest_and_installation(stranded_manifest, stranded)
        .await
        .expect("persist stranded pending aggregate");

    // A finalized manifest for a different extension id passes
    // take_v2_preparation_lease's checks (which don't compare extension
    // ids) but fails finish_v2_preparation's extension-id match, so the
    // lease is committed before the rejection is returned.
    let mismatched_finalized = manifest_record("other", Some("hash-final"));
    let rejection = store
        .finalize_preparation(
            &stranded_id,
            &stranded_incarnation,
            &stranded_ref,
            mismatched_finalized,
        )
        .await
        .expect_err("mismatched finalize is rejected");
    assert!(matches!(
        rejection,
        ExtensionInstallationError::PreparationFinalizationRejected { .. }
    ));

    // The aggregate must remain immediately visible with no store
    // reconstruction/restart repair pass in between.
    assert!(
        store
            .get_installation(&stranded_id)
            .await
            .expect("load after rejected finalize")
            .is_some(),
        "rejected finalize must not strand the aggregate under a held lease"
    );
}

#[tokio::test]
async fn checkpoint_replaces_only_the_manifest_and_remains_pending() {
    let store = installation_store().await;
    let pending = pending_installation("fixture", Some("hash-one"));
    let installation_id = pending.installation_id().clone();
    let incarnation = pending.incarnation_id().cloned().expect("incarnation");
    let expected_ref = pending.manifest_ref().clone();
    store
        .upsert_manifest_and_installation(
            pending_manifest_record("fixture", Some("hash-one")),
            pending,
        )
        .await
        .expect("seed pending");

    let checkpointed = store
        .checkpoint_preparation(
            &installation_id,
            &incarnation,
            &expected_ref,
            pending_manifest_record("fixture", Some("hash-two")),
        )
        .await
        .expect("checkpoint pending manifest");
    assert_eq!(checkpointed.incarnation_id(), Some(&incarnation));
    assert_eq!(
        checkpointed
            .manifest_ref()
            .manifest_hash()
            .map(ManifestHash::as_str),
        Some("hash-two")
    );
}

#[tokio::test]
async fn failed_checkpoint_releases_its_preparation_lease() {
    let backend = Arc::new(
        FaultInjecting::new(InMemoryBackend::new()).with_fault(
            Fault::on(FilesystemOperation::WriteFile)
                .path("/v2/installations/")
                .nth(3)
                .backend("interrupt checkpoint commit"),
        ),
    );
    let store = ExtensionInstallationStore::load_at(
        backend,
        VirtualPath::new("/system/extensions/.installations/checkpoint-failure")
            .expect("valid root"),
        HostPortCatalog::empty(),
        capability_provider_contracts(),
    )
    .await
    .expect("filesystem store");
    let pending = pending_installation("fixture", Some("hash-one"));
    let installation_id = pending.installation_id().clone();
    let incarnation = pending.incarnation_id().cloned().expect("incarnation");
    let expected_ref = pending.manifest_ref().clone();
    store
        .upsert_manifest_and_installation(
            pending_manifest_record("fixture", Some("hash-one")),
            pending.clone(),
        )
        .await
        .expect("seed pending");

    store
        .checkpoint_preparation(
            &installation_id,
            &incarnation,
            &expected_ref,
            pending_manifest_record("fixture", Some("hash-two")),
        )
        .await
        .expect_err("the injected checkpoint commit failure surfaces");
    let visible = store
        .get_installation(&installation_id)
        .await
        .expect("load after failed checkpoint")
        .expect("a failed checkpoint must release its durable lease immediately");
    assert_eq!(visible.manifest_ref(), pending.manifest_ref());
    assert_eq!(visible.incarnation_id(), pending.incarnation_id());
    assert_eq!(visible.owner(), pending.owner());

    let retry = store
        .checkpoint_preparation(
            &installation_id,
            &incarnation,
            &expected_ref,
            pending_manifest_record("fixture", Some("hash-two")),
        )
        .await
        .expect("the checkpoint remains retryable without reopening the store");
    assert_eq!(
        retry
            .manifest_ref()
            .manifest_hash()
            .map(ManifestHash::as_str),
        Some("hash-two")
    );
}

#[tokio::test]
async fn preparation_lease_cleanup_clears_only_the_matching_lease() {
    let store = installation_store().await;
    let pending = pending_installation("fixture", Some("hash-one"));
    let installation_id = pending.installation_id().clone();
    let incarnation = pending.incarnation_id().cloned().expect("incarnation");
    let expected_ref = pending.manifest_ref().clone();
    store
        .upsert_manifest_and_installation(
            pending_manifest_record("fixture", Some("hash-one")),
            pending,
        )
        .await
        .expect("seed pending");
    store
        .take_v2_preparation_lease(&installation_id, &incarnation, &expected_ref)
        .await
        .expect("take preparation lease");

    let different_ref = ExtensionManifestRef::new(
        ExtensionId::new("fixture").expect("extension id"),
        Some(ManifestHash::new("hash-other").expect("hash")),
    );
    store
        .clear_v2_preparation_lease(&installation_id, &incarnation, &different_ref)
        .await
        .expect("mismatched cleanup is a no-op");
    assert!(
        store
            .get_installation(&installation_id)
            .await
            .expect("load leased installation")
            .is_none(),
        "a mismatched cleanup must retain the newer preparation lease"
    );

    store
        .clear_v2_preparation_lease(&installation_id, &incarnation, &expected_ref)
        .await
        .expect("matching cleanup succeeds");
    assert!(
        store
            .get_installation(&installation_id)
            .await
            .expect("load released installation")
            .is_some(),
        "the matching cleanup restores visibility"
    );
}

#[tokio::test]
async fn checkpoint_rejects_stale_and_contract_mutating_requests_without_change() {
    let store = installation_store().await;
    let pending = pending_installation("fixture", Some("hash-one"));
    let installation_id = pending.installation_id().clone();
    let incarnation = pending.incarnation_id().cloned().expect("incarnation");
    let expected_ref = pending.manifest_ref().clone();
    store
        .upsert_manifest_and_installation(
            pending_manifest_record("fixture", Some("hash-one")),
            pending.clone(),
        )
        .await
        .expect("seed pending");

    let stale_incarnation = InstallationIncarnationId::fresh();
    let stale_ref = ExtensionManifestRef::new(
        ExtensionId::new("fixture").expect("extension id"),
        Some(ManifestHash::new("hash-stale").expect("hash")),
    );
    let source_mutation = ExtensionManifestRecord::from_toml_with_root_binding(
        manifest_toml("fixture"),
        ManifestSource::InstalledLocal,
        &HostPortCatalog::empty(),
        Some(ManifestHash::new("hash-two").expect("hash")),
        &capability_provider_contracts(),
        PackageRootBinding::FabricateOnLoad,
    )
    .expect("source mutation manifest");
    let root_mutation = ExtensionManifestRecord::from_toml_with_root_binding(
        manifest_toml("fixture"),
        ManifestSource::HostBundled,
        &HostPortCatalog::empty(),
        Some(ManifestHash::new("hash-two").expect("hash")),
        &capability_provider_contracts(),
        PackageRootBinding::Materialized(
            VirtualPath::new("/system/extensions/fixture").expect("root"),
        ),
    )
    .expect("root mutation manifest");
    for (incarnation_arg, ref_arg, next) in [
        (
            &stale_incarnation,
            &expected_ref,
            manifest_record("fixture", Some("hash-two")),
        ),
        (
            &incarnation,
            &stale_ref,
            manifest_record("fixture", Some("hash-two")),
        ),
        (&incarnation, &expected_ref, source_mutation),
        (&incarnation, &expected_ref, root_mutation),
        (
            &incarnation,
            &expected_ref,
            manifest_record("other", Some("hash-two")),
        ),
    ] {
        let error = store
            .checkpoint_preparation(&installation_id, incarnation_arg, ref_arg, next)
            .await
            .expect_err("stale or contract-mutating checkpoint must fail");
        assert!(matches!(
            error,
            ExtensionInstallationError::PreparationFinalizationRejected { .. }
        ));
        assert_eq!(
            store
                .get_installation(&installation_id)
                .await
                .expect("load unchanged"),
            Some(pending.clone())
        );
    }
}

#[tokio::test]
async fn checkpoint_rejects_ready_and_removed_installations() {
    let store = installation_store().await;
    let ready = installation("ready", Some("hash-one"));
    let ready_id = ready.installation_id().clone();
    store
        .upsert_manifest_and_installation(manifest_record("ready", Some("hash-one")), ready.clone())
        .await
        .expect("seed ready");
    let incarnation = InstallationIncarnationId::fresh();
    let error = store
        .checkpoint_preparation(
            &ready_id,
            &incarnation,
            ready.manifest_ref(),
            manifest_record("ready", Some("hash-two")),
        )
        .await
        .expect_err("ready checkpoint rejected");
    assert!(matches!(
        error,
        ExtensionInstallationError::PreparationFinalizationRejected { .. }
    ));

    let removed = pending_installation("removed", Some("hash-one"));
    let removed_id = removed.installation_id().clone();
    let removed_incarnation = removed.incarnation_id().cloned().expect("incarnation");
    let removed_ref = removed.manifest_ref().clone();
    store
        .upsert_manifest_and_installation(
            pending_manifest_record("removed", Some("hash-one")),
            removed,
        )
        .await
        .expect("seed removed candidate");
    store
        .finish_v2_installation_removal(&removed_id, Utc::now())
        .await
        .expect("tombstone installation");
    let error = store
        .checkpoint_preparation(
            &removed_id,
            &removed_incarnation,
            &removed_ref,
            manifest_record("removed", Some("hash-two")),
        )
        .await
        .expect_err("removed checkpoint rejected");
    assert!(matches!(
        error,
        ExtensionInstallationError::PreparationFinalizationRejected { .. }
    ));
}

#[tokio::test]
async fn checkpoint_retries_after_reopen_repairs_its_interrupted_lease() {
    let backend = Arc::new(InMemoryBackend::new());
    let root =
        VirtualPath::new("/system/extensions/.installations/checkpoint-recovery").expect("root");
    let store = ExtensionInstallationStore::load_at(
        backend.clone(),
        root.clone(),
        HostPortCatalog::empty(),
        capability_provider_contracts(),
    )
    .await
    .expect("store");
    let pending = pending_installation("fixture", Some("hash-one"));
    let installation_id = pending.installation_id().clone();
    let incarnation = pending.incarnation_id().cloned().expect("incarnation");
    let expected_ref = pending.manifest_ref().clone();
    store
        .upsert_manifest_and_installation(
            pending_manifest_record("fixture", Some("hash-one")),
            pending,
        )
        .await
        .expect("seed pending");
    store
        .take_v2_preparation_lease(&installation_id, &incarnation, &expected_ref)
        .await
        .expect("take checkpoint lease");
    assert_eq!(
        store
            .get_installation(&installation_id)
            .await
            .expect("hidden read"),
        None
    );
    drop(store);

    let reopened = ExtensionInstallationStore::load_at(
        backend,
        root,
        HostPortCatalog::empty(),
        capability_provider_contracts(),
    )
    .await
    .expect("repair checkpoint lease");
    let _checkpointed = reopened
        .checkpoint_preparation(
            &installation_id,
            &incarnation,
            &expected_ref,
            pending_manifest_record("fixture", Some("hash-two")),
        )
        .await
        .expect("retry checkpoint");
}

#[tokio::test]
async fn delayed_finalizer_cannot_cross_a_replaced_pending_incarnation() {
    let store = installation_store().await;
    let first = pending_installation("fixture", Some("hash-one"));
    let installation_id = first.installation_id().clone();
    let first_incarnation = first.incarnation_id().cloned().expect("fresh incarnation");
    let first_ref = first.manifest_ref().clone();
    store
        .upsert_manifest_and_installation(
            pending_manifest_record("fixture", Some("hash-one")),
            first,
        )
        .await
        .expect("persist first pending aggregate");

    let replacement = pending_installation("fixture", Some("hash-two"));
    let replacement_incarnation = replacement
        .incarnation_id()
        .cloned()
        .expect("replacement incarnation");
    store
        .upsert_manifest_and_installation(
            pending_manifest_record("fixture", Some("hash-two")),
            replacement,
        )
        .await
        .expect("replace pending aggregate");

    let error = store
        .finalize_preparation(
            &installation_id,
            &first_incarnation,
            &first_ref,
            manifest_record("fixture", Some("hash-final")),
        )
        .await
        .expect_err("old finalizer must fail ABA");
    assert!(matches!(
        error,
        ExtensionInstallationError::PreparationFinalizationRejected { .. }
    ));
    let current = store
        .get_installation(&installation_id)
        .await
        .expect("load replacement")
        .expect("replacement stays present");
    assert_eq!(current.incarnation_id(), Some(&replacement_incarnation));
}

#[tokio::test]
async fn reopen_recovers_a_crashed_pending_finalizer_without_publishing() {
    let backend = Arc::new(InMemoryBackend::new());
    let root =
        VirtualPath::new("/system/extensions/.installations/pending-recovery").expect("valid root");
    let store = ExtensionInstallationStore::load_at(
        backend.clone(),
        root.clone(),
        HostPortCatalog::empty(),
        capability_provider_contracts(),
    )
    .await
    .expect("store");
    let pending = pending_installation("fixture", Some("hash-pending"));
    let installation_id = pending.installation_id().clone();
    let incarnation_id = pending
        .incarnation_id()
        .cloned()
        .expect("fresh incarnation");
    let pending_ref = pending.manifest_ref().clone();
    store
        .upsert_manifest_and_installation(
            pending_manifest_record("fixture", Some("hash-pending")),
            pending,
        )
        .await
        .expect("persist pending aggregate");
    store
        .take_v2_preparation_lease(&installation_id, &incarnation_id, &pending_ref)
        .await
        .expect("take finalization lease");
    drop(store);

    let reopened = ExtensionInstallationStore::load_at(
        backend,
        root,
        HostPortCatalog::empty(),
        capability_provider_contracts(),
    )
    .await
    .expect("reopen repairs interrupted lease");
    let recovered = reopened
        .get_installation(&installation_id)
        .await
        .expect("load recovered aggregate")
        .expect("pending aggregate remains");
    assert_eq!(recovered.incarnation_id(), Some(&incarnation_id));
}
