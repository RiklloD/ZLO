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

/// The configured PostgreSQL URL. `None` (skip) only when none is set and
/// `IRONCLAW_REQUIRE_POSTGRES` is unset; a malformed URL or pool is a
/// failure, never a silent skip.
fn postgres_url() -> Option<String> {
    match std::env::var("IRONCLAW_FILESYSTEM_POSTGRES_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
    {
        Ok(url) => Some(url),
        Err(_) => {
            assert!(
                std::env::var_os("IRONCLAW_REQUIRE_POSTGRES").is_none(),
                "IRONCLAW_REQUIRE_POSTGRES is set but no PostgreSQL URL is configured"
            );
            None
        }
    }
}

fn postgres_pool(url: &str, application_name: &str) -> deadpool_postgres::Pool {
    let mut config = url
        .parse::<tokio_postgres::Config>()
        .expect("the configured PostgreSQL URL must parse");
    config.application_name(application_name);
    let manager = deadpool_postgres::Manager::new(config, tokio_postgres::NoTls);
    deadpool_postgres::Pool::builder(manager)
        .max_size(4)
        .build()
        .expect("the PostgreSQL pool must build")
}

async fn postgres_root(url: &str, application_name: &str) -> PostgresRootFilesystem {
    let fs = PostgresRootFilesystem::new(postgres_pool(url, application_name));
    fs.run_migrations().await.expect("migrations");
    fs
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
    let Some(url) = postgres_url() else {
        return;
    };
    let racer_name = format!("ironclaw_recreate_race_{}", uuid::Uuid::new_v4().simple());
    let observer = postgres_pool(&url, "ironclaw_recreate_race_observer");
    let fs = Arc::new(postgres_root(&url, &racer_name).await);
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

/// `append_file` checks for a directory before its INSERT. A
/// `create_dir_all` landing between the two used to make the INSERT's
/// conflict arm convert that directory row (created at the default version
/// 1, bypassing the path lock) into a file at `1 + 1 = 2` — the version the
/// path's deleted incarnation had already handed out.
///
/// A third connection holds the path lock so B's `append_file` passes its
/// precheck and parks at its version allocation; D's `create_dir_all` runs
/// in that window; then the lock is released. Whichever of B and D wins,
/// a file at the path is above the deleted v2 and v2 authorizes nothing.
#[tokio::test]
async fn a_directory_created_inside_append_file_never_becomes_a_reused_version() {
    let Some(url) = postgres_url() else {
        return;
    };
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let b_name = format!("ironclaw_append_race_b_{suffix}");
    let d_name = format!("ironclaw_append_race_d_{suffix}");
    let observer = postgres_pool(&url, "ironclaw_append_race_observer");
    let fs = Arc::new(postgres_root(&url, &format!("ironclaw_append_race_{suffix}")).await);
    let fs_b = Arc::new(postgres_root(&url, &b_name).await);
    let fs_d = Arc::new(postgres_root(&url, &d_name).await);
    let path = VirtualPath::new(format!("/secrets/leases/pgtest_{suffix}/record")).unwrap();

    // The path held v1 then v2 and was deleted: its mark is v2.
    let v1 = fs
        .put(&path, Entry::bytes(vec![1]), CasExpectation::Absent)
        .await
        .unwrap();
    let v2 = fs
        .put(&path, Entry::bytes(vec![2]), CasExpectation::Version(v1))
        .await
        .unwrap();
    fs.delete(&path).await.unwrap();

    // C holds the path lock.
    let holder = observer.get().await.expect("lock holder connection");
    holder.batch_execute("BEGIN").await.unwrap();
    holder
        .execute(
            "SELECT root_filesystem_lock_record_path($1)",
            &[&path.as_str()],
        )
        .await
        .expect("C takes the path lock");

    // B: precheck passes (nothing at the path), then parks on the lock.
    let b_path = path.clone();
    let b_fs = Arc::clone(&fs_b);
    let b = tokio::spawn(async move { b_fs.append_file(&b_path, b"appended").await });
    wait_for_lock_wait(&observer, &b_name).await;

    // D creates the directory inside B's window (or parks on the lock, if
    // it takes it).
    let d_path = path.clone();
    let d_fs = Arc::clone(&fs_d);
    let d = tokio::spawn(async move { d_fs.create_dir_all(&d_path).await });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !d.is_finished() && !lock_waiting(&observer, &d_name).await {
        assert!(
            tokio::time::Instant::now() < deadline,
            "create_dir_all neither finished nor waited on the path lock"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    holder.batch_execute("COMMIT").await.unwrap();
    let appended = b.await.expect("B must not panic");
    let created = d.await.expect("D must not panic");

    match fs.stat(&path).await.expect("the path exists").file_type {
        ironclaw_filesystem::FileType::File => {
            appended.expect("B wrote the file");
            let file = fs.get(&path).await.unwrap().expect("the file");
            assert!(
                file.version > v2,
                "the file reused a version the deleted incarnation handed out: \
                 mark {v2}, file {}",
                file.version
            );
            assert!(
                created.is_err(),
                "D cannot create a directory over B's file"
            );
        }
        ironclaw_filesystem::FileType::Directory => {
            assert!(appended.is_err(), "B must not append onto D's directory");
        }
        other => panic!("unexpected file type {other:?}"),
    }
    assert!(
        fs.put(&path, Entry::bytes(vec![9]), CasExpectation::Version(v2))
            .await
            .is_err(),
        "the deleted incarnation's v2 must authorize nothing"
    );
}

async fn lock_waiting(observer: &deadpool_postgres::Pool, application_name: &str) -> bool {
    let client = observer.get().await.expect("observer connection");
    client
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
        .get(0)
}
