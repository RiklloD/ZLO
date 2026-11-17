#![deny(clippy::let_underscore_must_use)]
//! Two-writer recreate race on PostgreSQL (CONTRACT.md invariant 9).
//!
//! Schedule: v1 is committed; transaction A updates the record to v2 and
//! stays open; B's `put(Absent)` on the same path starts; A deletes the
//! record and commits. Before path locking, B computed its version from a
//! statement snapshot taken while A was open (it saw v1, so it chose 2),
//! waited on A's unique key, and inserted v2 — a token A had already handed
//! out. With every create/update/delete holding the path's advisory lock,
//! B waits for A before it computes, sees A's floor, and lands above v2.
//!
//! Its own binary (like `postgres_delete_if_version_race`): the deliberately
//! open transaction must not stall behind unrelated DDL from the broad
//! contract suite. Needs `IRONCLAW_FILESYSTEM_POSTGRES_URL` or
//! `DATABASE_URL`; skips without one unless `IRONCLAW_REQUIRE_POSTGRES` is
//! set, which turns the skip into a failure.

use std::sync::Arc;

use ironclaw_filesystem::{
    CasExpectation, Entry, FilesystemError, PostgresRootFilesystem, RootFilesystem,
};
use ironclaw_host_api::path::VirtualPath;

async fn postgres_pool(application_name: &str) -> Option<deadpool_postgres::Pool> {
    let url = match std::env::var("IRONCLAW_FILESYSTEM_POSTGRES_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
    {
        Ok(url) => url,
        Err(_) => {
            assert!(
                std::env::var_os("IRONCLAW_REQUIRE_POSTGRES").is_none(),
                "IRONCLAW_REQUIRE_POSTGRES is set but no PostgreSQL URL is configured"
            );
            return None;
        }
    };
    let mut config = url.parse::<tokio_postgres::Config>().ok()?;
    config.application_name(application_name);
    let manager = deadpool_postgres::Manager::new(config, tokio_postgres::NoTls);
    deadpool_postgres::Pool::builder(manager)
        .max_size(4)
        .build()
        .ok()
}

/// Wait until a backend of `application_name` is blocked on a lock (the
/// advisory path lock after the fix, A's unique key before it).
async fn wait_for_lock_wait(observer: &deadpool_postgres::Pool, application_name: &str) {
    let client = observer.get().await.expect("observer connection");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: bool = client
            .query_one(
                "SELECT EXISTS (
                    SELECT 1 FROM pg_stat_activity
                    WHERE application_name = $1
                      AND state = 'active'
                      AND wait_event_type = 'Lock'
                )",
                &[&application_name],
            )
            .await
            .expect("observe the lock wait")
            .get(0);
        if waiting {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the concurrent create never blocked on the open transaction"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_create_racing_an_update_then_delete_never_reuses_the_updated_version() {
    let racer_name = format!("ironclaw_recreate_race_{}", uuid::Uuid::new_v4().simple());
    let Some(pool) = postgres_pool(&racer_name).await else {
        return;
    };
    let Some(observer) = postgres_pool("ironclaw_recreate_race_observer").await else {
        return;
    };
    let fs = Arc::new(PostgresRootFilesystem::new(pool));
    fs.run_migrations().await.expect("migrations");
    let prefix = format!("/secrets/leases/pgtest_{}", uuid::Uuid::new_v4().simple());
    let path = VirtualPath::new(format!("{prefix}/record")).unwrap();

    let v1 = fs
        .put(&path, Entry::bytes(vec![1]), CasExpectation::Absent)
        .await
        .unwrap();

    // Transaction A: update to v2 and keep the transaction open.
    let mut a = fs
        .begin(&VirtualPath::new(prefix.clone()).unwrap())
        .await
        .expect("begin A");
    let v2 = a
        .put(&path, Entry::bytes(vec![2]), CasExpectation::Version(v1))
        .await
        .expect("A updates to v2");

    // B creates the same path while A is open.
    let b_fs = Arc::clone(&fs);
    let b_path = path.clone();
    let b = tokio::spawn(async move {
        b_fs.put(&b_path, Entry::bytes(vec![3]), CasExpectation::Absent)
            .await
    });
    wait_for_lock_wait(&observer, &racer_name).await;

    // A deletes the record and commits.
    a.delete(&path).await.expect("A deletes");
    a.commit().await.expect("A commits");

    let vb = b
        .await
        .expect("B must not panic")
        .expect("B's create lands once A is gone");
    assert!(
        vb > v2,
        "B reused a version A handed out: A's v2 = {v2}, B's create = {vb}"
    );
    assert!(
        matches!(
            fs.put(&path, Entry::bytes(vec![9]), CasExpectation::Version(v2))
                .await,
            Err(FilesystemError::VersionMismatch { .. })
        ),
        "A's v2 token must not authorize a write to B's record"
    );
    assert!(
        matches!(
            fs.delete_if_version(&path, v2).await,
            Err(FilesystemError::VersionMismatch { .. })
        ),
        "A's v2 token must not authorize deleting B's record"
    );
    let current = fs.get(&path).await.unwrap().expect("B's record");
    assert_eq!((current.version, current.entry.body), (vb, vec![3]));
}
