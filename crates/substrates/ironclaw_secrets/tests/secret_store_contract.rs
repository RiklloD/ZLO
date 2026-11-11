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
// `leases_for_scope` must return exactly the caller's leases — same
// tenant/user/agent/project AND same mission/thread/invocation — with their
// effective status, on every backend the `/secrets` mount can resolve to.
// The suite also pins the two properties of the indexed listing path:
//
// - steady-state listing is one bounded indexed traversal, not a
//   `list_dir` + per-entry `get` fan-out (the former N+1);
// - leases written before the index existed (no lease-scope projection) are
//   still listed — they are re-projected once, body bytes unchanged.

mod lease_listing {
    use std::{collections::BTreeMap, sync::Arc};

    use ironclaw_filesystem::{
        CasExpectation, DiskFilesystem, FaultInjecting, FilesystemOperation, InMemoryBackend,
        IsolatedPostgresDatabase, IsolatedPostgresProvisioner, LibSqlRootFilesystem,
        PostgresRootFilesystem, PostgresUnreachable, RootFilesystem,
    };
    use ironclaw_host_api::{
        ids::{InvocationId, ProjectId, SecretHandle},
        path::{HostPath, VirtualPath},
        resource::ResourceScope,
    };
    use ironclaw_secrets::{
        SecretLeaseId, SecretLeaseStatus, SecretMaterial, SecretStore, SecretStorePort,
    };

    use super::sample_scope;

    /// The only indexed projection a lease row carried before the
    /// lease-scope index existed. Stripping a row back to this set
    /// reproduces exactly what a pre-index binary persisted.
    const LEGACY_LEASE_INDEXED_KEYS: &[&str] = &["tenant_id"];

    fn lease_virtual_path(scope: &ResourceScope, lease_id: SecretLeaseId) -> VirtualPath {
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
        VirtualPath::new(format!("{owner}/secret-leases/{lease_id}.json")).unwrap()
    }

    /// Rewrite a stored lease as a pre-index binary would have written it:
    /// same body bytes, legacy projection only. Returns the body bytes.
    async fn strip_to_legacy_projection<F: RootFilesystem>(
        backend: &F,
        scope: &ResourceScope,
        lease_id: SecretLeaseId,
    ) -> Vec<u8> {
        let path = lease_virtual_path(scope, lease_id);
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

    async fn statuses<F: RootFilesystem>(
        store: &SecretStore<F>,
        scope: &ResourceScope,
    ) -> BTreeMap<String, SecretLeaseStatus> {
        store
            .leases_for_scope(scope)
            .await
            .unwrap()
            .into_iter()
            .inspect(|lease| assert_eq!(&lease.scope, scope, "lease leaked across scopes"))
            .map(|lease| (lease.id.to_string(), lease.status))
            .collect()
    }

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
            tenant_id: ironclaw_host_api::ids::TenantId::new("tenant-b").unwrap(),
            ..scope.clone()
        };
        for owner in [&scope, &other_project, &other_tenant] {
            writer
                .put(
                    owner.clone(),
                    handle.clone(),
                    SecretMaterial::from("lease-listing-secret"),
                    None,
                )
                .await
                .unwrap();
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

        // Leases persisted before the index existed: no lease-scope projection.
        let legacy_revoked_body =
            strip_to_legacy_projection(backend.as_ref(), &scope, revoked.id).await;
        let legacy_sibling_body =
            strip_to_legacy_projection(backend.as_ref(), &sibling_invocation, sibling.id).await;

        // A fresh store — a restarted process with no in-memory state.
        let reader = SecretStore::ephemeral_over(Arc::clone(&backend));
        let expected_scope = BTreeMap::from([
            (consumed.id.to_string(), SecretLeaseStatus::Consumed),
            (revoked.id.to_string(), SecretLeaseStatus::Revoked),
            (active.id.to_string(), SecretLeaseStatus::Active),
        ]);
        assert_eq!(statuses(&reader, &scope).await, expected_scope);
        assert_eq!(
            statuses(&reader, &sibling_invocation).await,
            BTreeMap::from([(sibling.id.to_string(), SecretLeaseStatus::Active)])
        );
        assert_eq!(
            statuses(&reader, &other_project).await,
            BTreeMap::from([(project_lease.id.to_string(), SecretLeaseStatus::Active)])
        );
        assert_eq!(
            statuses(&reader, &other_tenant).await,
            BTreeMap::from([(tenant_lease.id.to_string(), SecretLeaseStatus::Active)])
        );

        // Re-projection keeps legacy body bytes byte-for-byte and adds the
        // index projection, so the next listing no longer depends on it.
        for (owner, lease_id, legacy_body) in [
            (&scope, revoked.id, &legacy_revoked_body),
            (&sibling_invocation, sibling.id, &legacy_sibling_body),
        ] {
            let row = backend
                .get(&lease_virtual_path(owner, lease_id))
                .await
                .unwrap()
                .expect("legacy lease row still present");
            assert_eq!(&row.entry.body, legacy_body, "legacy lease body rewritten");
            assert!(
                row.entry.indexed.len() > LEGACY_LEASE_INDEXED_KEYS.len(),
                "legacy lease was not re-projected"
            );
        }

        // Steady state: one indexed traversal, no directory walk, no
        // per-lease read.
        let gets = backend.count(FilesystemOperation::ReadFile);
        let lists = backend.count(FilesystemOperation::ListDir);
        let queries = backend.count(FilesystemOperation::Query);
        assert_eq!(statuses(&reader, &scope).await, expected_scope);
        assert_eq!(backend.count(FilesystemOperation::ReadFile) - gets, 0);
        assert_eq!(backend.count(FilesystemOperation::ListDir) - lists, 0);
        assert_eq!(backend.count(FilesystemOperation::Query) - queries, 1);

        // A status transition keeps the row in the index.
        writer.consume(&scope, active.id).await.unwrap();
        assert_eq!(
            statuses(&reader, &scope).await.get(&active.id.to_string()),
            Some(&SecretLeaseStatus::Consumed)
        );
    }

    #[tokio::test]
    async fn in_memory_backend_satisfies_the_lease_listing_contract() {
        lease_listing_contract(Arc::new(FaultInjecting::new(InMemoryBackend::new()))).await;
    }

    #[tokio::test]
    async fn libsql_backend_satisfies_the_lease_listing_contract() {
        let dir = tempfile::tempdir().unwrap();
        let database = Arc::new(
            libsql::Builder::new_local(dir.path().join("secrets.db"))
                .build()
                .await
                .unwrap(),
        );
        let backend = LibSqlRootFilesystem::new(database).unwrap();
        backend.run_migrations().await.unwrap();
        lease_listing_contract(Arc::new(FaultInjecting::new(backend))).await;
    }

    static POSTGRES: IsolatedPostgresProvisioner = IsolatedPostgresProvisioner::new(
        "postgres secrets lease listing contract",
        "IRONCLAW_SECRETS_POSTGRES_URL",
        "secrets_lease_isolated_",
        PostgresUnreachable::Skip,
    );

    async fn postgres_backend() -> Option<(IsolatedPostgresDatabase, PostgresRootFilesystem)> {
        let db = POSTGRES.provision().await?;
        let manager = deadpool_postgres::Manager::new(db.config().clone(), tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(manager)
            .max_size(4)
            .build()
            .expect("postgres pool");
        let backend = PostgresRootFilesystem::new(pool);
        backend.run_migrations().await.expect("migrate postgres");
        Some((db, backend))
    }

    #[tokio::test]
    async fn postgres_backend_satisfies_the_lease_listing_contract() {
        let Some((db, backend)) = postgres_backend().await else {
            return;
        };
        lease_listing_contract(Arc::new(FaultInjecting::new(backend))).await;
        db.cleanup().await;
    }

    /// The byte-only local backend cannot hold record-shaped lease rows at
    /// all; listing over it must keep answering "no leases" rather than
    /// failing because it cannot serve the index.
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
    }
}
