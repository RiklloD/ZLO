//! Contract for the skill usage store at its public seam: every concurrent
//! writer's use is counted (CAS, no process lock), a reopened store reads
//! what was written, owners are isolated, and pins read back.
#![deny(clippy::let_underscore_must_use)]
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;

use ironclaw_filesystem::{
    InMemoryBackend, LibSqlRootFilesystem, RootFilesystem, ScopedFilesystem,
};
use ironclaw_host_api::{
    ids::{TenantId, UserId},
    mount::{MountGrant, MountPermissions, MountView},
    path::{MountAlias, VirtualPath},
};
use ironclaw_skills::usage::{
    FilesystemSkillUsageStore, SkillLedger, SkillPin, SkillUsageName, SkillUsageObservation,
    SkillUsageOwner, SkillUsageSource, SkillUsageStore, SkillUse,
};

const NOW: u64 = 1_800_000_000_000;

fn scoped<F: RootFilesystem>(backend: Arc<F>, user: &str) -> Arc<ScopedFilesystem<F>> {
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/skill-usage").expect("alias"),
        VirtualPath::new(format!("/engine/tenants/t/users/{user}/skill-usage")).expect("target"),
        MountPermissions::read_write(),
    )])
    .expect("mount view");
    Arc::new(ScopedFilesystem::with_fixed_view(backend, mounts))
}

fn owner(user: &str) -> SkillUsageOwner {
    SkillUsageOwner {
        tenant_id: TenantId::new("t").expect("tenant"),
        user_id: UserId::new(user).expect("user"),
    }
}

fn name(s: &str) -> SkillUsageName {
    SkillUsageName::new(s).expect("name")
}

fn used(skill: &str) -> SkillUsageObservation {
    SkillUsageObservation {
        catalog: None,
        used: vec![name(skill)],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writers_keep_every_use_and_a_reopened_store_reads_them() {
    let backend = Arc::new(InMemoryBackend::new());
    let store = Arc::new(FilesystemSkillUsageStore::new(scoped(
        Arc::clone(&backend),
        "u",
    )));
    let claude = SkillUsageSource::cli_runtime("claude-cli").expect("source");
    let writers: Vec<_> = (0..24)
        .map(|i| {
            let store = Arc::clone(&store);
            let source = if i % 2 == 0 {
                claude.clone()
            } else {
                SkillUsageSource::Harness
            };
            tokio::spawn(async move {
                store
                    .record(&owner("u"), &source, used("grilling"), NOW)
                    .await
                    .expect("record");
            })
        })
        .collect();
    for writer in writers {
        writer.await.expect("writer");
    }
    let reopened = FilesystemSkillUsageStore::new(scoped(backend, "u"));
    let document = reopened
        .load(&owner("u"))
        .await
        .expect("load")
        .expect("managed");
    assert_eq!(
        document.sources[&claude].ledger[&name("grilling")].count,
        12
    );
    assert_eq!(
        document.sources[&SkillUsageSource::Harness].ledger[&name("grilling")].count,
        12
    );
    assert_eq!(document.combined_ledger()[&name("grilling")].count, 24);
}

#[tokio::test]
async fn owners_are_isolated_and_pins_read_back() {
    let backend = Arc::new(InMemoryBackend::new());
    let alice = FilesystemSkillUsageStore::new(scoped(Arc::clone(&backend), "alice"));
    let bob = FilesystemSkillUsageStore::new(scoped(Arc::clone(&backend), "bob"));
    alice
        .record(
            &owner("alice"),
            &SkillUsageSource::Harness,
            SkillUsageObservation {
                catalog: Some(vec![name("a"), name("b")]),
                used: vec![name("a")],
            },
            NOW,
        )
        .await
        .expect("record");
    let written = alice
        .set_pin(&owner("alice"), &name("b"), Some(SkillPin::Off))
        .await
        .expect("pin");
    assert_eq!(written.pins[&name("b")], SkillPin::Off);
    let read_back = alice
        .load(&owner("alice"))
        .await
        .expect("load")
        .expect("managed");
    assert_eq!(read_back, written);
    let theirs = bob
        .load(&owner("bob"))
        .await
        .expect("load")
        .expect("managed");
    assert!(theirs.sources.is_empty() && theirs.pins.is_empty());
    let cleared = alice
        .set_pin(&owner("alice"), &name("b"), None)
        .await
        .expect("unpin");
    assert!(cleared.pins.is_empty());
}

#[tokio::test]
async fn an_imported_ledger_merges_max_count_and_latest_use() {
    let backend = Arc::new(InMemoryBackend::new());
    let store = FilesystemSkillUsageStore::new(scoped(backend, "u"));
    let claude = SkillUsageSource::cli_runtime("claude-cli").expect("source");
    store
        .record(&owner("u"), &claude, used("a"), NOW)
        .await
        .expect("record");
    let imported: SkillLedger = [
        (
            name("a"),
            SkillUse {
                count: 5,
                last_used_ms: NOW - 10,
            },
        ),
        (
            name("b"),
            SkillUse {
                count: 2,
                last_used_ms: NOW - 20,
            },
        ),
    ]
    .into_iter()
    .collect();
    store
        .import_ledger(&owner("u"), &claude, imported.clone())
        .await
        .expect("import");
    // Importing the same ledger again changes nothing.
    store
        .import_ledger(&owner("u"), &claude, imported)
        .await
        .expect("import twice");
    let document = store
        .load(&owner("u"))
        .await
        .expect("load")
        .expect("managed");
    let ledger = &document.sources[&claude].ledger;
    assert_eq!(
        ledger[&name("a")],
        SkillUse {
            count: 5,
            last_used_ms: NOW
        }
    );
    assert_eq!(
        ledger[&name("b")],
        SkillUse {
            count: 2,
            last_used_ms: NOW - 20
        }
    );
}

#[tokio::test]
async fn the_document_survives_a_libsql_reopen() {
    let directory = tempfile::tempdir().expect("temporary libSQL directory");
    let database_path = directory.path().join("skill-usage.db");
    {
        let database = Arc::new(
            libsql::Builder::new_local(&database_path)
                .build()
                .await
                .expect("build database"),
        );
        let root = Arc::new(LibSqlRootFilesystem::new(database).expect("root filesystem"));
        root.run_migrations().await.expect("migrations");
        let store = FilesystemSkillUsageStore::new(scoped(root, "u"));
        store
            .record(&owner("u"), &SkillUsageSource::Harness, used("a"), NOW)
            .await
            .expect("record");
        store
            .set_pin(&owner("u"), &name("a"), Some(SkillPin::AlwaysFull))
            .await
            .expect("pin");
    }
    let database = Arc::new(
        libsql::Builder::new_local(&database_path)
            .build()
            .await
            .expect("reopen database"),
    );
    let root = Arc::new(LibSqlRootFilesystem::new(database).expect("root filesystem"));
    root.run_migrations().await.expect("migrations");
    let store = FilesystemSkillUsageStore::new(scoped(root, "u"));
    let document = store
        .load(&owner("u"))
        .await
        .expect("load")
        .expect("managed");
    assert_eq!(
        document.sources[&SkillUsageSource::Harness].ledger[&name("a")].count,
        1
    );
    assert_eq!(document.pins[&name("a")], SkillPin::AlwaysFull);
}
