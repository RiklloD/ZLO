//! The storage half of moving a standalone install to another computer
//! (`zlo export` / `zlo import` in the binary).
//!
//! Composition owns the standalone database file (`<storage root>/
//! reborn-local-dev.db`) and its secrets master key, so the operations that
//! touch either live here; the archive format, passphrase and the walk over
//! the rest of the data folder live in the binary.
//!
//! - [`snapshot_standalone_database`] copies the live database with SQLite's
//!   `VACUUM INTO`, a transaction-consistent copy that is safe while `zlo
//!   serve` keeps writing, then drops this installation's environment
//!   identity, pairing codes and device grants from the *copy* (the live
//!   database is only read) and vacuums the copy again so no deleted page
//!   keeps a token hash.
//! - [`rewrite_standalone_database_paths`] applies the importer's home-path
//!   rewrite to the record columns that hold absolute folders.
//! - [`read_standalone_secret_master_key`] resolves the key the backend
//!   would use (dotfile, then `SECRETS_MASTER_KEY`, then the OS keychain)
//!   without ever generating or writing one; [`install_standalone_secret_master_key`]
//!   writes it as the 0600 dotfile, which the backend reads first on every
//!   platform.

use std::path::{Path, PathBuf};

use secrecy::{ExposeSecret, SecretString};

use crate::RebornBuildError;
use crate::factory::{
    MasterKeySource, STANDALONE_SECRETS_MASTER_KEY_PATH, validate_resolved_master_key,
    write_standalone_secret_master_key,
};
use crate::filesystem_assembly::standalone_db_path;

/// File name of the standalone database inside a storage root.
pub fn standalone_database_file(storage_root: &Path) -> PathBuf {
    standalone_db_path(storage_root)
}

/// File name of the cached secrets master key inside a storage root.
pub const SECRETS_MASTER_KEY_FILE: &str = STANDALONE_SECRETS_MASTER_KEY_PATH;

/// What [`snapshot_standalone_database`] left out of the copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabaseSnapshot {
    /// Environment identity, pairing-code and device rows removed.
    pub environment_records_left_out: u64,
}

/// Counts read from a restored database for the import summary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DatabaseSummary {
    pub threads: u64,
    pub profiles: u64,
    pub automations: u64,
    pub memory_documents: u64,
    /// Stored secret records (provider keys, tokens).
    pub secrets: u64,
    /// Absolute folders the records point at: profile working folders and
    /// the repositories worktrees were made from. Deduplicated, sorted.
    pub referenced_folders: Vec<String>,
}

/// Columns holding an absolute path (or JSON with one) that a move to
/// another home must rewrite.
const PATH_COLUMNS: &[(&str, &str)] = &[
    ("orch_worktrees", "path"),
    ("orch_worktrees", "repo_root"),
    ("orch_worktrees", "source_dir"),
    ("orch_worktree_hook_approvals", "repo_root"),
    ("orch_session_imports", "path"),
    ("orch_profiles", "settings"),
];

/// Tables of the libSQL root filesystem keyed by a virtual `path`.
const ROOT_FILESYSTEM_PATH_TABLES: &[&str] = &[
    "root_filesystem_entries",
    "root_filesystem_ordered_index_rows",
    "root_filesystem_events",
    "root_filesystem_sequences",
];

fn db_error(operation: &str, error: impl std::fmt::Display) -> RebornBuildError {
    RebornBuildError::InvalidConfig {
        reason: format!("standalone database {operation} failed: {error}"),
    }
}

async fn open(path: &Path) -> Result<libsql::Connection, RebornBuildError> {
    let db = libsql::Builder::new_local(path)
        .build()
        .await
        .map_err(|error| db_error("open", error))?;
    let conn = db.connect().map_err(|error| db_error("connect", error))?;
    conn.busy_timeout(std::time::Duration::from_secs(30))
        .map_err(|error| db_error("busy timeout", error))?;
    Ok(conn)
}

async fn table_exists(conn: &libsql::Connection, table: &str) -> Result<bool, RebornBuildError> {
    let mut rows = conn
        .query(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
        )
        .await
        .map_err(|error| db_error("schema read", error))?;
    Ok(rows
        .next()
        .await
        .map_err(|error| db_error("schema read", error))?
        .is_some())
}

/// The virtual-path GLOBs of the environment-access records, derived from
/// the identity crate's scoped root: `/tenant-shared` resolves to
/// `/tenants/<tenant>/shared` (see `invocation_mount_view`).
fn environment_record_globs() -> [String; 2] {
    let suffix = ironclaw_identity::environment_access::ENVIRONMENT_ACCESS_ROOT
        .trim_start_matches("/tenant-shared");
    let root = format!("/tenants/*/shared{suffix}");
    [root.clone(), format!("{root}/*")]
}

/// Copy the live standalone database at `storage_root` to `destination`
/// (which must not exist), leaving out environment-access records.
pub async fn snapshot_standalone_database(
    storage_root: &Path,
    destination: &Path,
) -> Result<DatabaseSnapshot, RebornBuildError> {
    let source = standalone_db_path(storage_root);
    if !source.is_file() {
        return Err(RebornBuildError::InvalidConfig {
            reason: format!("no standalone database at {}", source.display()),
        });
    }
    if destination.exists() {
        return Err(RebornBuildError::InvalidConfig {
            reason: format!("snapshot target {} already exists", destination.display()),
        });
    }
    let destination_text = destination
        .to_str()
        .ok_or_else(|| RebornBuildError::InvalidConfig {
            reason: "snapshot target path is not valid UTF-8".to_string(),
        })?;
    {
        let live = open(&source).await?;
        live.execute("VACUUM INTO ?1", [destination_text])
            .await
            .map_err(|error| db_error("snapshot (VACUUM INTO)", error))?;
    }

    let copy = open(destination).await?;
    let [root, below] = environment_record_globs();
    let mut removed = 0u64;
    for table in ROOT_FILESYSTEM_PATH_TABLES {
        if !table_exists(&copy, table).await? {
            continue;
        }
        removed += copy
            .execute(
                &format!("DELETE FROM {table} WHERE path GLOB ?1 OR path GLOB ?2"),
                [root.as_str(), below.as_str()],
            )
            .await
            .map_err(|error| db_error("environment record removal", error))?;
    }
    // Free pages would still hold the removed rows' bytes (device token
    // hashes); rebuild the copy so they are gone from the file.
    copy.execute("VACUUM", ())
        .await
        .map_err(|error| db_error("vacuum", error))?;
    Ok(DatabaseSnapshot {
        environment_records_left_out: removed,
    })
}

/// Copy this machine's own environment identity and device grants from
/// `previous` (the database an import is replacing) into `imported`, so
/// devices already paired with this machine stay paired. Returns the rows
/// copied; a missing table on either side copies nothing.
pub async fn carry_environment_records(
    previous: &Path,
    imported: &Path,
) -> Result<u64, RebornBuildError> {
    let previous_text = previous
        .to_str()
        .ok_or_else(|| RebornBuildError::InvalidConfig {
            reason: "previous database path is not valid UTF-8".to_string(),
        })?;
    let conn = open(imported).await?;
    if !table_exists(&conn, "root_filesystem_entries").await? {
        return Ok(0);
    }
    conn.execute("ATTACH DATABASE ?1 AS previous", [previous_text])
        .await
        .map_err(|error| db_error("attach previous", error))?;
    let has_previous = {
        let mut rows = conn
            .query(
                "SELECT 1 FROM previous.sqlite_master \
                 WHERE type = 'table' AND name = 'root_filesystem_entries'",
                (),
            )
            .await
            .map_err(|error| db_error("schema read", error))?;
        rows.next()
            .await
            .map_err(|error| db_error("schema read", error))?
            .is_some()
    };
    let mut copied = 0;
    if has_previous {
        let [root, below] = environment_record_globs();
        copied = conn
            .execute(
                "INSERT OR REPLACE INTO main.root_filesystem_entries \
                 (path, contents, is_dir, created_at, updated_at) \
                 SELECT path, contents, is_dir, created_at, updated_at \
                 FROM previous.root_filesystem_entries WHERE path GLOB ?1 OR path GLOB ?2",
                [root.as_str(), below.as_str()],
            )
            .await
            .map_err(|error| db_error("environment record copy", error))?;
        // The ancestors (`/tenants`, `/tenants/<t>`, `/tenants/<t>/shared`)
        // as directories, where the imported data lacks them.
        conn.execute(
            "INSERT OR IGNORE INTO main.root_filesystem_entries \
             (path, contents, is_dir, created_at, updated_at) \
             SELECT path, contents, is_dir, created_at, updated_at \
             FROM previous.root_filesystem_entries WHERE is_dir = 1 AND ( \
               path = '/tenants' \
               OR (path GLOB '/tenants/*' AND path NOT GLOB '/tenants/*/*') \
               OR (path GLOB '/tenants/*/shared' AND path NOT GLOB '/tenants/*/*/*'))",
            (),
        )
        .await
        .map_err(|error| db_error("environment ancestor copy", error))?;
    }
    conn.execute("DETACH DATABASE previous", ())
        .await
        .map_err(|error| db_error("detach previous", error))?;
    Ok(copied)
}

/// Rewrite every path-bearing column in the database at `database` with
/// `rewrite` (which returns `None` when a value is unchanged). Returns the
/// number of values changed.
pub async fn rewrite_standalone_database_paths(
    database: &Path,
    rewrite: &(dyn Fn(&str) -> Option<String> + Sync),
) -> Result<u64, RebornBuildError> {
    let conn = open(database).await?;
    let mut changed = 0u64;
    for (table, column) in PATH_COLUMNS {
        if !table_exists(&conn, table).await? {
            continue;
        }
        let mut updates = Vec::new();
        {
            let mut rows = conn
                .query(&format!("SELECT rowid, {column} FROM {table}"), ())
                .await
                .map_err(|error| db_error("path read", error))?;
            while let Some(row) = rows
                .next()
                .await
                .map_err(|error| db_error("path read", error))?
            {
                let rowid: i64 = row.get(0).map_err(|error| db_error("path read", error))?;
                let value: Option<String> =
                    row.get(1).map_err(|error| db_error("path read", error))?;
                if let Some(new) = value.as_deref().and_then(rewrite) {
                    updates.push((rowid, new));
                }
            }
        }
        for (rowid, new) in updates {
            conn.execute(
                &format!("UPDATE {table} SET {column} = ?1 WHERE rowid = ?2"),
                libsql::params![new, rowid],
            )
            .await
            .map_err(|error| db_error("path rewrite", error))?;
            changed += 1;
        }
    }
    Ok(changed)
}

async fn count(conn: &libsql::Connection, table: &str, sql: &str) -> Result<u64, RebornBuildError> {
    if !table_exists(conn, table).await? {
        return Ok(0);
    }
    let mut rows = conn
        .query(sql, ())
        .await
        .map_err(|error| db_error("summary read", error))?;
    let Some(row) = rows
        .next()
        .await
        .map_err(|error| db_error("summary read", error))?
    else {
        return Ok(0);
    };
    let value: i64 = row
        .get(0)
        .map_err(|error| db_error("summary read", error))?;
    // silent-ok: COUNT(*) is never negative
    Ok(u64::try_from(value).unwrap_or(0))
}

async fn strings(
    conn: &libsql::Connection,
    table: &str,
    sql: &str,
) -> Result<Vec<String>, RebornBuildError> {
    if !table_exists(conn, table).await? {
        return Ok(Vec::new());
    }
    let mut rows = conn
        .query(sql, ())
        .await
        .map_err(|error| db_error("summary read", error))?;
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .await
        .map_err(|error| db_error("summary read", error))?
    {
        let value: Option<String> = row
            .get(0)
            .map_err(|error| db_error("summary read", error))?;
        out.extend(value);
    }
    Ok(out)
}

/// Read the import summary from the database at `database`.
pub async fn summarize_standalone_database(
    database: &Path,
) -> Result<DatabaseSummary, RebornBuildError> {
    let conn = open(database).await?;
    let entries = "root_filesystem_entries";
    let mut folders: Vec<String> = strings(
        &conn,
        "orch_worktrees",
        "SELECT DISTINCT repo_root FROM orch_worktrees",
    )
    .await?;
    for settings in strings(
        &conn,
        "orch_profiles",
        "SELECT settings FROM orch_profiles WHERE archived_at IS NULL",
    )
    .await?
    {
        // A profile whose settings do not parse simply names no folder.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&settings)
            && let Some(folder) = value.get("working_folder").and_then(|v| v.as_str())
        {
            folders.push(folder.to_string());
        }
    }
    folders.sort();
    folders.dedup();
    Ok(DatabaseSummary {
        threads: count(&conn, "orch_threads", "SELECT COUNT(*) FROM orch_threads").await?,
        profiles: count(
            &conn,
            "orch_profiles",
            "SELECT COUNT(*) FROM orch_profiles WHERE archived_at IS NULL",
        )
        .await?,
        automations: count(
            &conn,
            "trigger_records",
            "SELECT COUNT(*) FROM trigger_records",
        )
        .await?,
        memory_documents: count(
            &conn,
            entries,
            "SELECT COUNT(*) FROM root_filesystem_entries \
             WHERE is_dir = 0 AND path GLOB '/memory/*'",
        )
        .await?,
        secrets: count(
            &conn,
            entries,
            "SELECT COUNT(*) FROM root_filesystem_entries \
             WHERE is_dir = 0 AND path GLOB '/tenants/*/users/*/secrets/*' \
             AND path GLOB '*/secrets/*.json' \
             AND path NOT GLOB '*/secret-leases/*' \
             AND path NOT GLOB '*/credential-accounts/*' \
             AND path NOT GLOB '*/credential-sessions/*'",
        )
        .await?,
        referenced_folders: folders,
    })
}

/// The secrets master key the backend would use for `storage_root`, or
/// `None` when it has none yet. Never generates or writes a key.
pub async fn read_standalone_secret_master_key(
    storage_root: &Path,
) -> Result<Option<SecretString>, RebornBuildError> {
    let key_path = storage_root.join(STANDALONE_SECRETS_MASTER_KEY_PATH);
    match std::fs::read_to_string(&key_path) {
        Ok(existing) => {
            let key = existing.trim().to_string();
            validate_resolved_master_key(&key, &MasterKeySource::File(key_path))?;
            return Ok(Some(SecretString::from(key)));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(RebornBuildError::InvalidConfig {
                reason: format!(
                    "standalone secrets master key at {} could not be read: {error}",
                    key_path.display()
                ),
            });
        }
    }
    match std::env::var(ironclaw_secrets::keychain::SECRETS_MASTER_KEY_ENV) {
        Ok(value) if !value.trim().is_empty() => {
            let key = value.trim().to_string();
            validate_resolved_master_key(&key, &MasterKeySource::Env)?;
            return Ok(Some(SecretString::from(key)));
        }
        Ok(_) | Err(std::env::VarError::NotPresent) => {}
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(RebornBuildError::InvalidConfig {
                reason: format!(
                    "{} is set but not valid UTF-8",
                    ironclaw_secrets::keychain::SECRETS_MASTER_KEY_ENV
                ),
            });
        }
    }
    match ironclaw_secrets::keychain::get_master_key().await {
        Ok(bytes) => {
            let key: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            validate_resolved_master_key(&key, &MasterKeySource::Keychain)?;
            Ok(Some(SecretString::from(key)))
        }
        // Same blanket fallback as the boot resolver: a missing, suppressed
        // or unreachable keychain means "no key from the keychain".
        Err(_) => Ok(None), // silent-ok: no keychain key; secrets (if any) would not decrypt at boot either
    }
}

/// Install `key` as `storage_root`'s cached master key (0600). Refuses to
/// replace an existing key file.
pub fn install_standalone_secret_master_key(
    storage_root: &Path,
    key: &SecretString,
) -> Result<(), RebornBuildError> {
    let key_text = key.expose_secret().trim();
    let key_path = storage_root.join(STANDALONE_SECRETS_MASTER_KEY_PATH);
    validate_resolved_master_key(key_text, &MasterKeySource::File(key_path.clone()))?;
    write_standalone_secret_master_key(&key_path, key_text)
}

/// Run raw SQL against a standalone database, for tests that need records
/// in the shape the backend writes them.
#[cfg(any(test, feature = "test-support"))]
pub async fn execute_sql_for_test(database: &Path, statements: &[&str]) {
    let conn = open(database).await.expect("open test database");
    for sql in statements {
        conn.execute(sql, ()).await.expect(sql);
    }
}

/// The first column of every row `sql` returns, for test assertions.
#[cfg(any(test, feature = "test-support"))]
pub async fn query_strings_for_test(database: &Path, sql: &str) -> Vec<String> {
    let conn = open(database).await.expect("open test database");
    let mut rows = conn.query(sql, ()).await.expect(sql);
    let mut out = Vec::new();
    while let Some(row) = rows.next().await.expect("row") {
        out.push(row.get::<String>(0).expect("text column"));
    }
    out
}

#[cfg(test)]
#[path = "setup_transfer_tests.rs"]
mod tests;
