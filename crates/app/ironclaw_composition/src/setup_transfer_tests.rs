//! Tests for `setup_transfer` (kept out of the production file for the
//! composition mass budget).

use super::*;

async fn seed(root: &Path, statements: &[&str]) {
    std::fs::create_dir_all(root).expect("root");
    let conn = open(&standalone_db_path(root)).await.expect("open");
    for sql in statements {
        conn.execute(sql, ()).await.expect(sql);
    }
}

#[tokio::test]
async fn snapshot_copies_records_and_leaves_environment_access_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("local-dev");
    seed(
        &root,
        &[
            "CREATE TABLE root_filesystem_entries (path TEXT PRIMARY KEY, contents BLOB, is_dir INTEGER NOT NULL DEFAULT 0)",
            "INSERT INTO root_filesystem_entries (path, contents) VALUES ('/tenants/t1/shared/reborn-environment/environment.json', 'env')",
            "INSERT INTO root_filesystem_entries (path, contents) VALUES ('/tenants/t1/shared/reborn-environment/devices/d1.json', 'device-token-hash')",
            "INSERT INTO root_filesystem_entries (path, contents) VALUES ('/tenants/t1/shared/reborn-environmental/keep.json', 'keep')",
            "INSERT INTO root_filesystem_entries (path, contents) VALUES ('/memory/t1/u1/notes.md', 'remember')",
        ],
    )
    .await;
    let copy = dir.path().join("copy.db");
    let report = snapshot_standalone_database(&root, &copy)
        .await
        .expect("snapshot");
    assert_eq!(report.environment_records_left_out, 2);

    let conn = open(&copy).await.expect("open copy");
    let paths = strings(
        &conn,
        "root_filesystem_entries",
        "SELECT path FROM root_filesystem_entries ORDER BY path",
    )
    .await
    .expect("paths");
    assert_eq!(
        paths,
        [
            "/memory/t1/u1/notes.md",
            "/tenants/t1/shared/reborn-environmental/keep.json"
        ]
    );
    let bytes = std::fs::read(&copy).expect("read copy");
    assert!(
        !bytes
            .windows(b"device-token-hash".len())
            .any(|w| w == b"device-token-hash"),
        "removed rows must not survive in free pages"
    );
    // The live database is untouched.
    let live = open(&standalone_db_path(&root)).await.expect("live");
    assert_eq!(
        count(
            &live,
            "root_filesystem_entries",
            "SELECT COUNT(*) FROM root_filesystem_entries"
        )
        .await
        .expect("count"),
        4
    );
}

#[tokio::test]
async fn the_replaced_machines_own_environment_records_are_carried_over() {
    let dir = tempfile::tempdir().expect("tempdir");
    let schema = "CREATE TABLE root_filesystem_entries (path TEXT PRIMARY KEY, contents BLOB, is_dir INTEGER NOT NULL DEFAULT 0, created_at TEXT, updated_at TEXT)";
    let previous = dir.path().join("previous");
    seed(
        &previous,
        &[
            schema,
            "INSERT INTO root_filesystem_entries (path, contents, is_dir) VALUES ('/tenants/t1/shared', '', 1)",
            "INSERT INTO root_filesystem_entries (path, contents) VALUES ('/tenants/t1/shared/reborn-environment/environment.json', 'server-id')",
            "INSERT INTO root_filesystem_entries (path, contents) VALUES ('/tenants/t1/shared/other.json', 'old data')",
        ],
    )
    .await;
    let imported = dir.path().join("imported");
    seed(
        &imported,
        &[
            schema,
            "INSERT INTO root_filesystem_entries (path, contents) VALUES ('/memory/m.md', 'mac data')",
        ],
    )
    .await;
    let copied = carry_environment_records(
        &standalone_db_path(&previous),
        &standalone_db_path(&imported),
    )
    .await
    .expect("carry");
    assert_eq!(copied, 1);
    let conn = open(&standalone_db_path(&imported)).await.expect("open");
    let paths = strings(
        &conn,
        "root_filesystem_entries",
        "SELECT path FROM root_filesystem_entries ORDER BY path",
    )
    .await
    .expect("paths");
    assert_eq!(
        paths,
        [
            "/memory/m.md",
            "/tenants/t1/shared",
            "/tenants/t1/shared/reborn-environment/environment.json"
        ]
    );
}

#[tokio::test]
async fn snapshot_refuses_an_existing_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("local-dev");
    seed(&root, &["CREATE TABLE t (x)"]).await;
    let copy = dir.path().join("copy.db");
    std::fs::write(&copy, b"x").expect("occupy");
    assert!(snapshot_standalone_database(&root, &copy).await.is_err());
}

#[tokio::test]
async fn path_columns_are_rewritten_and_summarized() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("local-dev");
    seed(
        &root,
        &[
            "CREATE TABLE orch_worktrees (id INTEGER PRIMARY KEY, path TEXT, repo_root TEXT, source_dir TEXT)",
            "INSERT INTO orch_worktrees (path, repo_root, source_dir) VALUES ('/Users/a/.ironclaw/worktrees/x', '/Users/a/src/app', '/Users/a/src/app')",
            "CREATE TABLE orch_profiles (profile_id TEXT, settings TEXT, archived_at INTEGER)",
            "INSERT INTO orch_profiles VALUES ('p', '{\"working_folder\":\"/Users/a/work\"}', NULL)",
            "INSERT INTO orch_profiles VALUES ('q', '{\"working_folder\":\"/Users/a/old\"}', 5)",
            "CREATE TABLE orch_threads (thread_id TEXT)",
            "INSERT INTO orch_threads VALUES ('t1')",
        ],
    )
    .await;
    let db = standalone_db_path(&root);
    let changed = rewrite_standalone_database_paths(&db, &|value: &str| {
        value
            .contains("/Users/a")
            .then(|| value.replace("/Users/a", "/home/a"))
    })
    .await
    .expect("rewrite");
    assert_eq!(changed, 5);
    let summary = summarize_standalone_database(&db).await.expect("summary");
    assert_eq!(summary.threads, 1);
    assert_eq!(summary.profiles, 1);
    assert_eq!(
        summary.referenced_folders,
        ["/home/a/src/app", "/home/a/work"]
    );
}

#[tokio::test]
async fn master_key_is_read_from_the_dotfile_and_installed_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("source");
    let target = dir.path().join("target");
    std::fs::create_dir_all(&source).expect("source");
    std::fs::create_dir_all(&target).expect("target");
    // No dotfile: the keychain is suppressed under cfg(test), so nothing.
    if std::env::var_os(ironclaw_secrets::keychain::SECRETS_MASTER_KEY_ENV).is_none() {
        assert!(
            read_standalone_secret_master_key(&source)
                .await
                .expect("read")
                .is_none()
        );
    }
    let key = ironclaw_secrets::keychain::generate_master_key_hex();
    std::fs::write(source.join(SECRETS_MASTER_KEY_FILE), format!("{key}\n")).expect("seed");
    let read = read_standalone_secret_master_key(&source)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(read.expose_secret(), key);
    install_standalone_secret_master_key(&target, &read).expect("install");
    assert!(install_standalone_secret_master_key(&target, &read).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(target.join(SECRETS_MASTER_KEY_FILE))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
