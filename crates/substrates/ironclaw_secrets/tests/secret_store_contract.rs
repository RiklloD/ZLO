// untriaged discards: predate the workspace-wide deny (Cargo.toml [workspace.lints.clippy]).
#![allow(clippy::let_underscore_must_use)]
use ironclaw_host_api::{
    ids::{AgentId, InvocationId, MissionId, ProjectId, SecretHandle, TenantId, ThreadId, UserId},
    resource::ResourceScope,
};
use ironclaw_secrets::{SecretLeaseStatus, SecretMaterial, SecretStore, SecretStorePort};
use secrecy::ExposeSecret;

#[tokio::test]
async fn secret_store_returns_metadata_without_secret_material() {
    let store = SecretStore::ephemeral();
    let scope = sample_scope("tenant-a", "user-a");
    let handle = SecretHandle::new("github_token").unwrap();

    let metadata = store
        .put(
            scope.clone(),
            handle.clone(),
            SecretMaterial::from("ghp_secret_token"),
            None,
        )
        .await
        .unwrap();

    assert_eq!(metadata.scope, scope);
    assert_eq!(metadata.handle, handle);
    assert!(!format!("{metadata:?}").contains("ghp_secret_token"));
}

#[tokio::test]
async fn concurrent_put_if_absent_preserves_exactly_one_winner() {
    let store = std::sync::Arc::new(SecretStore::ephemeral());
    let scope = sample_scope("tenant-a", "user-a");
    let handle = SecretHandle::new("generated_key").unwrap();
    let first = {
        let store = std::sync::Arc::clone(&store);
        let scope = scope.clone();
        let handle = handle.clone();
        tokio::spawn(async move {
            store
                .put_if_absent(scope, handle, SecretMaterial::from("first"), None)
                .await
                .unwrap()
        })
    };
    let second = {
        let store = std::sync::Arc::clone(&store);
        let scope = scope.clone();
        let handle = handle.clone();
        tokio::spawn(async move {
            store
                .put_if_absent(scope, handle, SecretMaterial::from("second"), None)
                .await
                .unwrap()
        })
    };

    let (first_won, second_won) = (first.await.unwrap(), second.await.unwrap());
    assert_ne!(first_won, second_won, "exactly one create must commit");
    let lease = store.lease_once(&scope, &handle).await.unwrap();
    let material = store.consume(&scope, lease.id).await.unwrap();
    assert_eq!(
        material.expose_secret(),
        if first_won { "first" } else { "second" }
    );
}

#[tokio::test]
async fn secret_store_consumes_one_shot_secret_lease() {
    let store = SecretStore::ephemeral();
    let scope = sample_scope("tenant-a", "user-a");
    let handle = SecretHandle::new("api_key").unwrap();
    store
        .put(
            scope.clone(),
            handle.clone(),
            SecretMaterial::from("super-secret"),
            None,
        )
        .await
        .unwrap();

    let lease = store.lease_once(&scope, &handle).await.unwrap();
    assert_eq!(lease.scope, scope);
    assert_eq!(lease.handle, handle);
    assert_eq!(lease.status, SecretLeaseStatus::Active);

    let value = store.consume(&scope, lease.id).await.unwrap();
    assert_eq!(value.expose_secret(), "super-secret");

    let second = store.consume(&scope, lease.id).await.unwrap_err();
    assert!(second.is_consumed());
}

#[tokio::test]
async fn consuming_one_lease_does_not_delete_underlying_secret() {
    let store = SecretStore::ephemeral();
    let scope = sample_scope("tenant-a", "user-a");
    let handle = SecretHandle::new("refresh_token").unwrap();
    store
        .put(
            scope.clone(),
            handle.clone(),
            SecretMaterial::from("refresh-secret"),
            None,
        )
        .await
        .unwrap();

    let first_lease = store.lease_once(&scope, &handle).await.unwrap();
    assert_eq!(
        store
            .consume(&scope, first_lease.id)
            .await
            .unwrap()
            .expose_secret(),
        "refresh-secret"
    );
    assert!(
        store
            .consume(&scope, first_lease.id)
            .await
            .unwrap_err()
            .is_consumed()
    );

    let second_lease = store.lease_once(&scope, &handle).await.unwrap();
    assert_eq!(
        store
            .consume(&scope, second_lease.id)
            .await
            .unwrap()
            .expose_secret(),
        "refresh-secret"
    );
}

#[tokio::test]
async fn secret_store_isolates_same_handle_between_tenants() {
    let store = SecretStore::ephemeral();
    let tenant_a = sample_scope("tenant-a", "user-a");
    let tenant_b = sample_scope("tenant-b", "user-a");
    let handle = SecretHandle::new("shared_name").unwrap();
    store
        .put(
            tenant_a.clone(),
            handle.clone(),
            SecretMaterial::from("tenant-a-secret"),
            None,
        )
        .await
        .unwrap();
    store
        .put(
            tenant_b.clone(),
            handle.clone(),
            SecretMaterial::from("tenant-b-secret"),
            None,
        )
        .await
        .unwrap();

    let tenant_a_lease = store.lease_once(&tenant_a, &handle).await.unwrap();
    let tenant_b_lease = store.lease_once(&tenant_b, &handle).await.unwrap();

    assert_eq!(
        store
            .consume(&tenant_a, tenant_a_lease.id)
            .await
            .unwrap()
            .expose_secret(),
        "tenant-a-secret"
    );
    assert_eq!(
        store
            .consume(&tenant_b, tenant_b_lease.id)
            .await
            .unwrap()
            .expose_secret(),
        "tenant-b-secret"
    );

    let cross_scope = store
        .consume(&tenant_b, tenant_a_lease.id)
        .await
        .unwrap_err();
    assert!(cross_scope.is_unknown_lease());
}

#[tokio::test]
async fn secret_store_delete_is_idempotent_and_scope_isolated() {
    let store = SecretStore::ephemeral();
    let owner = sample_scope("tenant-a", "user-a");
    let other = sample_scope("tenant-a", "user-b");
    let handle = SecretHandle::new("shared_name").unwrap();
    store
        .put(
            owner.clone(),
            handle.clone(),
            SecretMaterial::from("owner-secret"),
            None,
        )
        .await
        .unwrap();
    store
        .put(
            other.clone(),
            handle.clone(),
            SecretMaterial::from("other-secret"),
            None,
        )
        .await
        .unwrap();

    assert!(store.delete(&owner, &handle).await.unwrap());
    assert!(!store.delete(&owner, &handle).await.unwrap());
    assert!(store.metadata(&owner, &handle).await.unwrap().is_none());
    assert!(store.metadata(&other, &handle).await.unwrap().is_some());

    let other_lease = store.lease_once(&other, &handle).await.unwrap();
    assert_eq!(
        store
            .consume(&other, other_lease.id)
            .await
            .unwrap()
            .expose_secret(),
        "other-secret"
    );
}

#[tokio::test]
async fn secret_store_isolates_same_handle_between_users_and_projects() {
    let store = SecretStore::ephemeral();
    let user_a = sample_scope("tenant-a", "user-a");
    let user_b = sample_scope("tenant-a", "user-b");
    let project_b = ResourceScope {
        project_id: Some(ProjectId::new("project-b").unwrap()),
        ..sample_scope("tenant-a", "user-a")
    };
    let handle = SecretHandle::new("shared_name").unwrap();
    store
        .put(
            user_a.clone(),
            handle.clone(),
            SecretMaterial::from("user-a-project-a-secret"),
            None,
        )
        .await
        .unwrap();
    store
        .put(
            user_b.clone(),
            handle.clone(),
            SecretMaterial::from("user-b-project-a-secret"),
            None,
        )
        .await
        .unwrap();
    store
        .put(
            project_b.clone(),
            handle.clone(),
            SecretMaterial::from("user-a-project-b-secret"),
            None,
        )
        .await
        .unwrap();

    let user_a_lease = store.lease_once(&user_a, &handle).await.unwrap();
    let user_b_lease = store.lease_once(&user_b, &handle).await.unwrap();
    let project_b_lease = store.lease_once(&project_b, &handle).await.unwrap();

    assert_eq!(
        store
            .consume(&user_a, user_a_lease.id)
            .await
            .unwrap()
            .expose_secret(),
        "user-a-project-a-secret"
    );
    assert_eq!(
        store
            .consume(&user_b, user_b_lease.id)
            .await
            .unwrap()
            .expose_secret(),
        "user-b-project-a-secret"
    );
    assert_eq!(
        store
            .consume(&project_b, project_b_lease.id)
            .await
            .unwrap()
            .expose_secret(),
        "user-a-project-b-secret"
    );

    let cross_user = store.consume(&user_b, user_a_lease.id).await.unwrap_err();
    assert!(cross_user.is_unknown_lease());
    let cross_project = store
        .consume(&project_b, user_a_lease.id)
        .await
        .unwrap_err();
    assert!(cross_project.is_unknown_lease());
}

#[tokio::test]
async fn secret_store_isolates_same_handle_between_agents() {
    let store = SecretStore::ephemeral();
    let mut agent_a = sample_scope("tenant-a", "user-a");
    agent_a.agent_id = Some(AgentId::new("agent-a").unwrap());
    let mut agent_b = agent_a.clone();
    agent_b.agent_id = Some(AgentId::new("agent-b").unwrap());
    let handle = SecretHandle::new("shared_name").unwrap();
    store
        .put(
            agent_a.clone(),
            handle.clone(),
            SecretMaterial::from("agent-a-secret"),
            None,
        )
        .await
        .unwrap();
    store
        .put(
            agent_b.clone(),
            handle.clone(),
            SecretMaterial::from("agent-b-secret"),
            None,
        )
        .await
        .unwrap();

    let agent_a_lease = store.lease_once(&agent_a, &handle).await.unwrap();
    let agent_b_lease = store.lease_once(&agent_b, &handle).await.unwrap();

    let cross_agent = store.consume(&agent_b, agent_a_lease.id).await.unwrap_err();
    assert!(cross_agent.is_unknown_lease());
    assert_eq!(
        store
            .consume(&agent_a, agent_a_lease.id)
            .await
            .unwrap()
            .expose_secret(),
        "agent-a-secret"
    );
    assert_eq!(
        store
            .consume(&agent_b, agent_b_lease.id)
            .await
            .unwrap()
            .expose_secret(),
        "agent-b-secret"
    );
}

#[tokio::test]
async fn revoked_secret_lease_cannot_be_consumed() {
    let store = SecretStore::ephemeral();
    let scope = sample_scope("tenant-a", "user-a");
    let handle = SecretHandle::new("api_key").unwrap();
    store
        .put(
            scope.clone(),
            handle.clone(),
            SecretMaterial::from("super-secret"),
            None,
        )
        .await
        .unwrap();

    let lease = store.lease_once(&scope, &handle).await.unwrap();
    let revoked = store.revoke(&scope, lease.id).await.unwrap();
    assert_eq!(revoked.status, SecretLeaseStatus::Revoked);

    let error = store.consume(&scope, lease.id).await.unwrap_err();
    assert!(error.is_revoked());
}

#[tokio::test]
async fn missing_secret_fails_without_creating_lease() {
    let store = SecretStore::ephemeral();
    let scope = sample_scope("tenant-a", "user-a");
    let handle = SecretHandle::new("missing").unwrap();

    let error = store.lease_once(&scope, &handle).await.unwrap_err();
    assert!(error.is_unknown_secret());
    assert_eq!(store.leases_for_scope(&scope).await.unwrap(), Vec::new());
}

fn sample_scope(tenant: &str, user: &str) -> ResourceScope {
    ResourceScope {
        tenant_id: TenantId::new(tenant).unwrap(),
        user_id: UserId::new(user).unwrap(),
        agent_id: None,
        project_id: Some(ProjectId::new("project-a").unwrap()),
        mission_id: Some(MissionId::new("mission-a").unwrap()),
        thread_id: Some(ThreadId::new("thread-a").unwrap()),
        invocation_id: InvocationId::new(),
    }
}

// -- Lease listing conformance (every backend) ------------------------------
//
// Expand step of the lease-scope index rollout
// (`docs/internal/reborn/contracts/secrets.md`):
//
// - `leases_for_scope` reads by directory scan, so it lists every lease of
//   exactly the caller's scope — including leases a pre-index binary wrote or
//   updated (no projection) — and performs no write of any kind;
// - every lease write carries the `secret_leases_by_scope_v1` projection;
// - `migrate_lease_scope_index` is the explicit, idempotent backfill;
// - `leases_for_scope_via_index_unreleased` (the contract-step read, not yet
//   called in production) is one bounded keyset traversal whose result equals
//   the scan once every row is projected.

mod lease_listing {
    use std::{
        collections::{BTreeMap, BTreeSet},
        io::Write as _,
        sync::Arc,
    };

    use ironclaw_filesystem::{
        CasExpectation, DiskFilesystem, Fault, FaultInjecting, FilesystemOperation,
        InMemoryBackend, IsolatedPostgresDatabase, IsolatedPostgresProvisioner,
        LibSqlRootFilesystem, PostgresRootFilesystem, PostgresUnreachable, RootFilesystem,
    };
    use ironclaw_host_api::{
        ids::{InvocationId, ProjectId, SecretHandle, TenantId},
        path::{HostPath, VirtualPath},
        resource::ResourceScope,
    };
    use ironclaw_secrets::{
        SecretLease, SecretLeaseId, SecretLeaseStatus, SecretMaterial, SecretStore, SecretStorePort,
    };

    use super::sample_scope;

    /// The only indexed projection a lease row carried before the
    /// lease-scope index existed. Stripping a row back to this set
    /// reproduces exactly what a pre-index binary persists.
    const LEGACY_LEASE_INDEXED_KEYS: &[&str] = &["tenant_id"];

    /// Above `Page::MAX_LIMIT` (the index page, 1024) and four migration
    /// pages (256 each), so both cursors are followed past page one.
    const MANY_LEASES: usize = 1100;

    fn owner_root(scope: &ResourceScope) -> String {
        let mut owner = format!(
            "/tenants/{}/users/{}/secrets",
            scope.tenant_id.as_str(),
            scope.user_id.as_str()
        );
        if let Some(agent) = &scope.agent_id {
            owner.push_str(&format!("/agents/{}", agent.as_str()));
        }
        if let Some(project) = &scope.project_id {
            owner.push_str(&format!("/projects/{}", project.as_str()));
        }
        owner
    }

    fn lease_virtual_path(scope: &ResourceScope, lease_id: &str) -> VirtualPath {
        VirtualPath::new(format!(
            "{}/secret-leases/{lease_id}.json",
            owner_root(scope)
        ))
        .unwrap()
    }

    /// Rewrite a stored lease the way a pre-index binary's serializer does on
    /// any write (create, consume, revoke, expiry promotion): `edit` may
    /// change the body, and only the legacy projection is kept. On
    /// PostgreSQL this UPDATE drops the row from the ordered index, exactly
    /// as the projection trigger does for an old binary. Returns the body.
    async fn write_as_old_binary<F: RootFilesystem>(
        backend: &F,
        scope: &ResourceScope,
        lease_id: &str,
        edit: impl FnOnce(&mut serde_json::Value),
    ) -> Vec<u8> {
        let path = lease_virtual_path(scope, lease_id);
        let current = backend.get(&path).await.unwrap().expect("lease row exists");
        let mut entry = current.entry;
        let mut body: serde_json::Value = serde_json::from_slice(&entry.body).unwrap();
        edit(&mut body);
        entry.body = serde_json::to_vec(&body).unwrap();
        entry
            .indexed
            .retain(|key, _| LEGACY_LEASE_INDEXED_KEYS.contains(&key.as_str()));
        let written = entry.body.clone();
        backend
            .put(&path, entry, CasExpectation::Version(current.version))
            .await
            .unwrap();
        written
    }

    /// Same body, legacy projection only: a lease persisted before the index.
    async fn strip_to_legacy_projection<F: RootFilesystem>(
        backend: &F,
        scope: &ResourceScope,
        lease_id: SecretLeaseId,
    ) -> Vec<u8> {
        let path = lease_virtual_path(scope, &lease_id.to_string());
        let current = backend.get(&path).await.unwrap().expect("lease row exists");
        let mut entry = current.entry;
        entry
            .indexed
            .retain(|key, _| LEGACY_LEASE_INDEXED_KEYS.contains(&key.as_str()));
        let body = entry.body.clone();
        backend
            .put(&path, entry, CasExpectation::Version(current.version))
            .await
            .unwrap();
        body
    }

    async fn projected<F: RootFilesystem>(
        backend: &F,
        scope: &ResourceScope,
        lease_id: SecretLeaseId,
    ) -> bool {
        let row = backend
            .get(&lease_virtual_path(scope, &lease_id.to_string()))
            .await
            .unwrap()
            .expect("lease row exists");
        row.entry.indexed.len() > LEGACY_LEASE_INDEXED_KEYS.len()
    }

    fn by_id(
        leases: Vec<SecretLease>,
        scope: &ResourceScope,
    ) -> BTreeMap<String, SecretLeaseStatus> {
        leases
            .into_iter()
            .inspect(|lease| assert_eq!(&lease.scope, scope, "lease leaked across scopes"))
            .map(|lease| (lease.id.to_string(), lease.status))
            .collect()
    }

    async fn scanned<F: RootFilesystem>(
        store: &SecretStore<F>,
        scope: &ResourceScope,
    ) -> BTreeMap<String, SecretLeaseStatus> {
        by_id(store.leases_for_scope(scope).await.unwrap(), scope)
    }

    async fn indexed<F: RootFilesystem>(
        store: &SecretStore<F>,
        scope: &ResourceScope,
    ) -> BTreeMap<String, SecretLeaseStatus> {
        let leases = store
            .leases_for_scope_via_index_unreleased(scope)
            .await
            .unwrap()
            .expect("record-shaped backends serve the ordered index");
        by_id(leases, scope)
    }

    async fn put_secret<F: RootFilesystem>(
        store: &SecretStore<F>,
        scope: &ResourceScope,
        handle: &SecretHandle,
    ) {
        store
            .put(
                scope.clone(),
                handle.clone(),
                SecretMaterial::from("lease-listing-secret"),
                None,
            )
            .await
            .unwrap();
    }

    /// Scope isolation, no-write reads, explicit backfill, and index/scan
    /// agreement once every row is projected.
    async fn lease_listing_contract<F>(backend: Arc<FaultInjecting<F>>)
    where
        F: RootFilesystem + 'static,
    {
        let writer = SecretStore::ephemeral_over(Arc::clone(&backend));
        let handle = SecretHandle::new("api_key").unwrap();

        let scope = sample_scope("tenant-a", "user-a");
        // Same owner, different invocation: shares the lease directory.
        let sibling_invocation = ResourceScope {
            invocation_id: InvocationId::new(),
            ..scope.clone()
        };
        // Different project under the same user.
        let other_project = ResourceScope {
            project_id: Some(ProjectId::new("project-b").unwrap()),
            ..scope.clone()
        };
        // Different tenant with the identical user/project/mission/thread/
        // invocation tuple: isolation must come from the mount, not the ids.
        let other_tenant = ResourceScope {
            tenant_id: TenantId::new("tenant-b").unwrap(),
            ..scope.clone()
        };
        for owner in [&scope, &other_project, &other_tenant] {
            put_secret(&writer, owner, &handle).await;
        }

        let consumed = writer.lease_once(&scope, &handle).await.unwrap();
        writer.consume(&scope, consumed.id).await.unwrap();
        let revoked = writer.lease_once(&scope, &handle).await.unwrap();
        writer.revoke(&scope, revoked.id).await.unwrap();
        let active = writer.lease_once(&scope, &handle).await.unwrap();
        let sibling = writer
            .lease_once(&sibling_invocation, &handle)
            .await
            .unwrap();
        let project_lease = writer.lease_once(&other_project, &handle).await.unwrap();
        let tenant_lease = writer.lease_once(&other_tenant, &handle).await.unwrap();

        // New-binary writes carry the projection without any migration.
        assert!(projected(backend.as_ref(), &scope, active.id).await);

        // Leases persisted before the index existed: no lease-scope projection.
        let legacy_revoked_body =
            strip_to_legacy_projection(backend.as_ref(), &scope, revoked.id).await;
        let legacy_sibling_body =
            strip_to_legacy_projection(backend.as_ref(), &sibling_invocation, sibling.id).await;

        // A fresh store — a restarted process with no in-memory state. The
        // scan lists legacy rows, and it reads only: no write, no index
        // declaration, no ordered query.
        let reader = SecretStore::ephemeral_over(Arc::clone(&backend));
        let expected_scope = BTreeMap::from([
            (consumed.id.to_string(), SecretLeaseStatus::Consumed),
            (revoked.id.to_string(), SecretLeaseStatus::Revoked),
            (active.id.to_string(), SecretLeaseStatus::Active),
        ]);
        let writes = backend.count(FilesystemOperation::WriteFile);
        let declarations = backend.count(FilesystemOperation::EnsureIndex);
        let queries = backend.count(FilesystemOperation::Query);
        assert_eq!(scanned(&reader, &scope).await, expected_scope);
        assert_eq!(
            scanned(&reader, &sibling_invocation).await,
            BTreeMap::from([(sibling.id.to_string(), SecretLeaseStatus::Active)])
        );
        assert_eq!(
            scanned(&reader, &other_project).await,
            BTreeMap::from([(project_lease.id.to_string(), SecretLeaseStatus::Active)])
        );
        assert_eq!(
            scanned(&reader, &other_tenant).await,
            BTreeMap::from([(tenant_lease.id.to_string(), SecretLeaseStatus::Active)])
        );
        assert_eq!(backend.count(FilesystemOperation::WriteFile), writes);
        assert_eq!(
            backend.count(FilesystemOperation::EnsureIndex),
            declarations
        );
        assert_eq!(backend.count(FilesystemOperation::Query), queries);
        assert!(
            !projected(backend.as_ref(), &scope, revoked.id).await,
            "a read re-projected a legacy lease"
        );

        // Why reads stay on the scan: the index misses unprojected rows.
        assert!(
            !indexed(&reader, &scope)
                .await
                .contains_key(&revoked.id.to_string())
        );

        // The explicit migration re-projects exactly the two legacy rows of
        // the owner root (both invocations share it), body bytes unchanged.
        assert_eq!(reader.migrate_lease_scope_index(&scope).await.unwrap(), 2);
        for (owner, lease_id, legacy_body) in [
            (&scope, revoked.id, &legacy_revoked_body),
            (&sibling_invocation, sibling.id, &legacy_sibling_body),
        ] {
            let row = backend
                .get(&lease_virtual_path(owner, &lease_id.to_string()))
                .await
                .unwrap()
                .expect("legacy lease row still present");
            assert_eq!(&row.entry.body, legacy_body, "legacy lease body rewritten");
            assert!(projected(backend.as_ref(), owner, lease_id).await);
        }
        // Idempotent: nothing left to rewrite, and no write issued.
        let writes = backend.count(FilesystemOperation::WriteFile);
        assert_eq!(reader.migrate_lease_scope_index(&scope).await.unwrap(), 0);
        assert_eq!(backend.count(FilesystemOperation::WriteFile), writes);

        // Contract-step read: equals the scan, as one bounded traversal with
        // no directory walk and no per-lease read.
        let gets = backend.count(FilesystemOperation::ReadFile);
        let lists = backend.count(FilesystemOperation::ListDir);
        let queries = backend.count(FilesystemOperation::Query);
        assert_eq!(indexed(&reader, &scope).await, expected_scope);
        assert_eq!(backend.count(FilesystemOperation::ReadFile) - gets, 0);
        assert_eq!(backend.count(FilesystemOperation::ListDir) - lists, 0);
        assert_eq!(backend.count(FilesystemOperation::Query) - queries, 1);
        assert_eq!(
            indexed(&reader, &sibling_invocation).await,
            scanned(&reader, &sibling_invocation).await
        );

        // A new-binary status transition keeps the row in both listings.
        writer.consume(&scope, active.id).await.unwrap();
        for listing in [
            scanned(&reader, &scope).await,
            indexed(&reader, &scope).await,
        ] {
            assert_eq!(
                listing.get(&active.id.to_string()),
                Some(&SecretLeaseStatus::Consumed)
            );
        }
    }

    /// Rolling deploy: an old binary consumes a lease the new binary wrote
    /// (stripping its projection) and creates a lease of its own. The scan
    /// still lists both with their real status; the index does not, until
    /// the explicit migration runs.
    async fn mixed_version_contract<F>(backend: Arc<FaultInjecting<F>>)
    where
        F: RootFilesystem + 'static,
    {
        let store = SecretStore::ephemeral_over(Arc::clone(&backend));
        let handle = SecretHandle::new("api_key").unwrap();
        let scope = sample_scope("tenant-mixed", "user-mixed");
        put_secret(&store, &scope, &handle).await;
        let updated = store.lease_once(&scope, &handle).await.unwrap();
        let untouched = store.lease_once(&scope, &handle).await.unwrap();
        assert!(projected(backend.as_ref(), &scope, updated.id).await);

        // Old binary consumes `updated`.
        write_as_old_binary(backend.as_ref(), &scope, &updated.id.to_string(), |body| {
            body["status"] = serde_json::json!("consumed");
        })
        .await;
        // Old binary creates a lease: same shape, no lease-scope projection.
        let created = SecretLeaseId::new();
        {
            let template = backend
                .get(&lease_virtual_path(&scope, &untouched.id.to_string()))
                .await
                .unwrap()
                .expect("template lease row exists");
            let mut entry = template.entry;
            let mut body: serde_json::Value = serde_json::from_slice(&entry.body).unwrap();
            body["lease_id"] = serde_json::json!(created.to_string());
            entry.body = serde_json::to_vec(&body).unwrap();
            entry
                .indexed
                .retain(|key, _| LEGACY_LEASE_INDEXED_KEYS.contains(&key.as_str()));
            backend
                .put(
                    &lease_virtual_path(&scope, &created.to_string()),
                    entry,
                    CasExpectation::Absent,
                )
                .await
                .unwrap();
        }

        let expected = BTreeMap::from([
            (updated.id.to_string(), SecretLeaseStatus::Consumed),
            (untouched.id.to_string(), SecretLeaseStatus::Active),
            (created.to_string(), SecretLeaseStatus::Active),
        ]);
        assert_eq!(scanned(&store, &scope).await, expected);
        // A fresh process agrees: nothing depends on in-memory state.
        let restarted = SecretStore::ephemeral_over(Arc::clone(&backend));
        assert_eq!(scanned(&restarted, &scope).await, expected);

        // The hazard the expand step avoids: the index lost both rows.
        let before = indexed(&restarted, &scope).await;
        assert!(!before.contains_key(&updated.id.to_string()));
        assert!(!before.contains_key(&created.to_string()));

        assert_eq!(
            restarted.migrate_lease_scope_index(&scope).await.unwrap(),
            2
        );
        assert_eq!(indexed(&restarted, &scope).await, expected);
    }

    /// Both cursors past page one: the index keyset (`Page::MAX_LIMIT`) and
    /// the migration's `list_dir_page` walk.
    async fn many_leases_contract<F>(backend: Arc<FaultInjecting<F>>)
    where
        F: RootFilesystem + 'static,
    {
        let store = SecretStore::ephemeral_over(Arc::clone(&backend));
        let handle = SecretHandle::new("api_key").unwrap();
        let scope = sample_scope("tenant-many", "user-many");
        put_secret(&store, &scope, &handle).await;
        let mut ids = Vec::with_capacity(MANY_LEASES);
        for _ in 0..MANY_LEASES {
            ids.push(store.lease_once(&scope, &handle).await.unwrap().id);
        }
        let expected: BTreeSet<String> = ids.iter().map(ToString::to_string).collect();
        let keys = |listing: BTreeMap<String, SecretLeaseStatus>| -> BTreeSet<String> {
            listing.into_keys().collect()
        };

        let queries = backend.count(FilesystemOperation::Query);
        assert_eq!(keys(indexed(&store, &scope).await), expected);
        assert!(
            backend.count(FilesystemOperation::Query) - queries >= 2,
            "index listing did not follow its keyset cursor past page one"
        );
        assert_eq!(keys(scanned(&store, &scope).await), expected);

        for id in &ids {
            strip_to_legacy_projection(backend.as_ref(), &scope, *id).await;
        }
        assert!(indexed(&store, &scope).await.is_empty());
        assert_eq!(keys(scanned(&store, &scope).await), expected);

        let lists = backend.count(FilesystemOperation::ListDir);
        assert_eq!(
            store.migrate_lease_scope_index(&scope).await.unwrap(),
            MANY_LEASES
        );
        assert!(
            backend.count(FilesystemOperation::ListDir) - lists >= 5,
            "migration did not page its directory walk"
        );
        assert_eq!(keys(indexed(&store, &scope).await), expected);
        assert_eq!(store.migrate_lease_scope_index(&scope).await.unwrap(), 0);
    }

    /// A migration that fails partway leaves a consistent state, and a re-run
    /// finishes exactly the remaining rows.
    async fn partial_migration_contract<F>(backend: Arc<FaultInjecting<F>>)
    where
        F: RootFilesystem + 'static,
    {
        const LEASES: usize = 10;
        let store = SecretStore::ephemeral_over(Arc::clone(&backend));
        let handle = SecretHandle::new("api_key").unwrap();
        let scope = sample_scope("tenant-partial", "user-partial");
        put_secret(&store, &scope, &handle).await;
        let mut bodies = BTreeMap::new();
        for _ in 0..LEASES {
            let id = store.lease_once(&scope, &handle).await.unwrap().id;
            let body = strip_to_legacy_projection(backend.as_ref(), &scope, id).await;
            bodies.insert(id.to_string(), (id, body));
        }

        // The fourth lease rewrite fails with a backend error.
        backend.add_fault(
            Fault::on(FilesystemOperation::WriteFile)
                .path("secret-leases")
                .nth(4)
                .backend("injected mid-migration failure"),
        );
        assert!(store.migrate_lease_scope_index(&scope).await.is_err());

        let mut done = 0;
        for (id, body) in bodies.values() {
            let row = backend
                .get(&lease_virtual_path(&scope, &id.to_string()))
                .await
                .unwrap()
                .expect("lease row survives a failed migration");
            assert_eq!(&row.entry.body, body, "failed migration changed a body");
            if projected(backend.as_ref(), &scope, *id).await {
                done += 1;
            }
        }
        assert_eq!(done, 3, "rows before the failure stay migrated");
        // Reads are unaffected by the half-done migration.
        assert_eq!(scanned(&store, &scope).await.len(), LEASES);

        assert_eq!(
            store.migrate_lease_scope_index(&scope).await.unwrap(),
            LEASES - done
        );
        let all: BTreeSet<String> = bodies.keys().cloned().collect();
        assert_eq!(
            indexed(&store, &scope)
                .await
                .into_keys()
                .collect::<BTreeSet<_>>(),
            all
        );
    }

    // -- Backends -----------------------------------------------------------

    /// Keeps a backend's storage alive for one test.
    enum Guard {
        None,
        Dir(
            #[allow(dead_code, reason = "held only to keep the directory alive")] tempfile::TempDir,
        ),
        Postgres(Box<IsolatedPostgresDatabase>),
    }

    impl Guard {
        async fn finish(self) {
            if let Guard::Postgres(db) = self {
                db.cleanup().await;
            }
        }
    }

    type Fixture<F> = Option<(Guard, Arc<FaultInjecting<F>>)>;

    async fn in_memory() -> Fixture<InMemoryBackend> {
        Some((
            Guard::None,
            Arc::new(FaultInjecting::new(InMemoryBackend::new())),
        ))
    }

    async fn libsql() -> Fixture<LibSqlRootFilesystem> {
        let dir = tempfile::tempdir().unwrap();
        let database = Arc::new(
            ::libsql::Builder::new_local(dir.path().join("secrets.db"))
                .build()
                .await
                .unwrap(),
        );
        let backend = LibSqlRootFilesystem::new(database).unwrap();
        backend.run_migrations().await.unwrap();
        Some((Guard::Dir(dir), Arc::new(FaultInjecting::new(backend))))
    }

    /// The URL CI's Postgres-service lanes export (coverage sets it; the
    /// filesystem contract suite reads it too). When set, every test gets a
    /// private database on that server, and an unusable server panics.
    const POSTGRES_URL_ENV: &str = "DATABASE_URL";

    static POSTGRES: IsolatedPostgresProvisioner = IsolatedPostgresProvisioner::new(
        "postgres secrets lease listing contract",
        POSTGRES_URL_ENV,
        "secrets_lease_isolated_",
        PostgresUnreachable::Panic,
    );

    /// One throwaway server per test binary when no URL is configured — what
    /// makes this leg run in CI lanes that have Docker but export no URL.
    static CONTAINER: tokio::sync::OnceCell<Option<ContainerServer>> =
        tokio::sync::OnceCell::const_new();
    static CONTAINER_DATABASES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    struct ContainerServer {
        config: tokio_postgres::Config,
        _container: testcontainers_modules::testcontainers::ContainerAsync<
            testcontainers_modules::postgres::Postgres,
        >,
    }

    /// Skips are written straight to the process stderr, past libtest's
    /// capture, so a run that tested nothing on PostgreSQL says so even when
    /// it passes. `IRONCLAW_REQUIRE_POSTGRES` (the existing CI switch) turns
    /// a skip into a failure.
    fn skip_postgres(reason: &str) -> Fixture<PostgresRootFilesystem> {
        assert!(
            std::env::var_os("IRONCLAW_REQUIRE_POSTGRES").is_none(),
            "IRONCLAW_REQUIRE_POSTGRES is set but the secrets lease PostgreSQL leg \
             cannot run: {reason}"
        );
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED: postgres secrets lease listing contract did not run: {reason}"
        );
        None
    }

    async fn container_database() -> Result<tokio_postgres::Config, String> {
        let server = CONTAINER
            .get_or_init(|| async {
                use testcontainers_modules::testcontainers::{ImageExt, runners::AsyncRunner};
                let container = testcontainers_modules::postgres::Postgres::default()
                    .with_user("postgres")
                    .with_password("postgres")
                    .with_tag("16-alpine")
                    .start()
                    .await
                    .ok()?;
                let host = container.get_host().await.ok()?;
                let port = container.get_host_port_ipv4(5432).await.ok()?;
                let config = format!("postgres://postgres:postgres@{host}:{port}/postgres")
                    .parse::<tokio_postgres::Config>()
                    .ok()?;
                Some(ContainerServer {
                    config,
                    _container: container,
                })
            })
            .await
            .as_ref()
            .ok_or_else(|| {
                format!("{POSTGRES_URL_ENV} not set and docker/testcontainers unavailable")
            })?;
        let (admin, connection) = server
            .config
            .connect(tokio_postgres::NoTls)
            .await
            .map_err(|error| format!("container postgres unreachable ({error})"))?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let name = format!(
            "secrets_lease_{}",
            CONTAINER_DATABASES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // A started container is a working environment: fail loud past here.
        admin
            .execute(&format!("CREATE DATABASE {name}"), &[])
            .await
            .expect("create a per-test database in the container");
        let mut config = server.config.clone();
        config.dbname(&name);
        Ok(config)
    }

    async fn postgres() -> Fixture<PostgresRootFilesystem> {
        let (guard, config) = if std::env::var_os(POSTGRES_URL_ENV).is_some() {
            let db = POSTGRES
                .provision()
                .await
                .expect("configured postgres provisions");
            let config = db.config().clone();
            (Guard::Postgres(Box::new(db)), config)
        } else {
            match container_database().await {
                Ok(config) => (Guard::None, config),
                Err(reason) => return skip_postgres(&reason),
            }
        };
        let manager = deadpool_postgres::Manager::new(config, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(manager)
            .max_size(4)
            .build()
            .expect("postgres pool");
        let backend = PostgresRootFilesystem::new(pool);
        backend.run_migrations().await.expect("migrate postgres");
        Some((guard, Arc::new(FaultInjecting::new(backend))))
    }

    macro_rules! backend_suite {
        ($backend:ident: $($scenario:ident),+ $(,)?) => {
            mod $backend {
                $(
                    #[tokio::test]
                    async fn $scenario() {
                        let Some((guard, backend)) = super::$backend().await else {
                            return;
                        };
                        super::$scenario(backend).await;
                        guard.finish().await;
                    }
                )+
            }
        };
    }

    backend_suite!(in_memory: lease_listing_contract, mixed_version_contract,
        many_leases_contract, partial_migration_contract);
    backend_suite!(libsql: lease_listing_contract, mixed_version_contract,
        many_leases_contract, partial_migration_contract);
    backend_suite!(postgres: lease_listing_contract, mixed_version_contract,
        many_leases_contract, partial_migration_contract);

    /// The byte-only local backend cannot hold record-shaped lease rows at
    /// all; listing over it keeps answering "no leases", the migration has
    /// nothing to do, and the index read reports it cannot serve.
    #[tokio::test]
    async fn local_filesystem_backend_lists_no_leases_without_an_index() {
        let storage = tempfile::tempdir().unwrap();
        let mut disk = DiskFilesystem::new();
        disk.mount_local(
            VirtualPath::new("/tenants").unwrap(),
            HostPath::from_path_buf(storage.path().to_path_buf()),
        )
        .unwrap();
        let store = SecretStore::ephemeral_over(Arc::new(disk));
        let scope = sample_scope("tenant-a", "user-a");
        assert_eq!(store.leases_for_scope(&scope).await.unwrap(), Vec::new());
        assert_eq!(store.migrate_lease_scope_index(&scope).await.unwrap(), 0);
        assert!(matches!(
            store.leases_for_scope_via_index_unreleased(&scope).await,
            Ok(None)
        ));
    }
}
