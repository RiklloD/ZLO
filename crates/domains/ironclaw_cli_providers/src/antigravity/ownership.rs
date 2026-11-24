//! Which session a set of agy files belongs to, proven by its workspace
//! (t3code c8b214158e). The model-list probe runs its session in a folder
//! made for it alone; a session whose record names that folder, and only
//! it, is the probe's. agy 1.5 keeps the record in the session log's
//! `trajectory_metadata_blob` (a `TrajectoryMetadata` protobuf: workspaces
//! in field 1, each with its `file://` URI in field 1; the session id in
//! field 6; more workspace URIs in field 7); the legacy runtime writes a
//! `<session>.meta` sidecar (`{"cwd": ...}`).

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::launch::APP_DATA_DIR;
use super::paths::{self, Kind, Managed};
use super::sqlite;

const TRAJECTORY_WORKSPACES: u64 = 1;
const WORKSPACE_URI: u64 = 1;
const TRAJECTORY_SESSION_ID: u64 = 6;
const TRAJECTORY_WORKSPACE_URIS: u64 = 7;
const MAX_METADATA_BYTES: usize = 1024 * 1024;
const MAX_META_FILE_BYTES: u64 = 64 * 1024;

/// Whether the session `id` under `profile` is the one that ran in `cwd`.
/// Anything unreadable, linked, ambiguous or naming another folder is
/// "not proven".
pub(super) async fn owned_by(profile: &Path, id: &str, cwd: &Path) -> bool {
    let want = canonical(cwd).await;
    let meta = format!("{id}.meta");
    match paths::managed(profile, &[APP_DATA_DIR, "conversations", &meta], Kind::File).await {
        Some(Managed::Found(path)) => return meta_cwd(&path).await.is_some_and(|c| c == want),
        Some(Managed::Missing(_)) => {}
        None => return false,
    }
    let db = format!("{id}.db");
    let Some(path) = paths::managed(profile, &[APP_DATA_DIR, "conversations", &db], Kind::File)
        .await
        .and_then(Managed::found)
    else {
        return false;
    };
    let sql = format!(
        "SELECT hex(data) FROM trajectory_metadata_blob \
         WHERE length(data) <= {MAX_METADATA_BYTES} LIMIT 2;"
    );
    let rows = match sqlite::query(&path, &sql).await {
        Ok(out) => out,
        Err(error) => {
            tracing::debug!(%error, "antigravity: probe session record unreadable");
            return false;
        }
    };
    let rows: Vec<&str> = rows.lines().filter(|l| !l.trim().is_empty()).collect();
    let [row] = rows.as_slice() else {
        return false;
    };
    let Some(record) = unhex(row.trim()).and_then(|blob| trajectory(&blob)) else {
        return false;
    };
    if record.workspaces.is_empty() || record.session_id.as_deref().is_some_and(|s| s != id) {
        return false;
    }
    for uri in &record.workspaces {
        let Some(path) = file_uri_path(uri) else {
            return false;
        };
        if canonical(&path).await != want {
            return false;
        }
    }
    true
}

/// The resolved path (macOS `/var` is `/private/var`), else as given.
async fn canonical(path: &Path) -> PathBuf {
    tokio::fs::canonicalize(path)
        .await
        .unwrap_or_else(|_| path.to_path_buf()) // silent-ok: a folder already gone compares as given
}

async fn meta_cwd(path: &Path) -> Option<PathBuf> {
    let meta = tokio::fs::symlink_metadata(path).await.ok()?; // silent-ok: gone since the check
    if meta.len() > MAX_META_FILE_BYTES {
        return None;
    }
    let raw = tokio::fs::read(path).await.ok()?; // silent-ok: unreadable proves nothing
    let doc: Value = serde_json::from_slice(&raw).ok()?; // silent-ok: torn proves nothing
    let cwd = PathBuf::from(doc.get("cwd")?.as_str()?);
    Some(canonical(&cwd).await)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Trajectory {
    session_id: Option<String>,
    workspaces: Vec<String>,
}

fn trajectory(blob: &[u8]) -> Option<Trajectory> {
    let mut out = Trajectory::default();
    for (field, value) in fields(blob)? {
        match field {
            TRAJECTORY_SESSION_ID if out.session_id.is_none() => {
                let id = std::str::from_utf8(value).ok()?.trim();
                if !id.is_empty() {
                    out.session_id = Some(id.to_string());
                }
            }
            TRAJECTORY_WORKSPACES => {
                for (nested, uri) in fields(value)? {
                    if nested == WORKSPACE_URI {
                        let uri = std::str::from_utf8(uri).ok()?.trim();
                        if !uri.is_empty() {
                            out.workspaces.push(uri.to_string());
                        }
                    }
                }
            }
            TRAJECTORY_WORKSPACE_URIS => {
                let uri = std::str::from_utf8(value).ok()?.trim();
                if !uri.is_empty() {
                    out.workspaces.push(uri.to_string());
                }
            }
            _ => {}
        }
    }
    Some(out)
}

fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The length-delimited fields of one protobuf message; varints and fixed
/// values are skipped. `None` for a malformed message.
fn fields(bytes: &[u8]) -> Option<Vec<(u64, &[u8])>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let key = varint(bytes, &mut at)?;
        match key & 7 {
            0 => {
                varint(bytes, &mut at)?;
            }
            1 => at = at.checked_add(8)?,
            2 => {
                let len = usize::try_from(varint(bytes, &mut at)?).ok()?;
                let end = at.checked_add(len)?;
                out.push((key >> 3, bytes.get(at..end)?));
                at = end;
            }
            5 => at = at.checked_add(4)?,
            _ => return None,
        }
        if at > bytes.len() {
            return None;
        }
    }
    Some(out)
}

/// `file:///a%20b` as `/a b`; `None` for any other URI.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

fn unhex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

/// A `TrajectoryMetadata` blob naming `uris` (field 1 workspaces) and an
/// optional session id, for tests.
#[cfg(test)]
pub(super) fn test_blob(uris: &[&str], session_id: Option<&str>) -> Vec<u8> {
    fn field(out: &mut Vec<u8>, number: u64, value: &[u8]) {
        let mut put = |mut v: u64| loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        };
        put((number << 3) | 2);
        put(value.len() as u64);
        out.extend_from_slice(value);
    }
    let mut blob = vec![0x10, 0x01]; // a varint field, skipped
    for uri in uris {
        let mut workspace = Vec::new();
        field(&mut workspace, WORKSPACE_URI, uri.as_bytes());
        field(&mut blob, TRAJECTORY_WORKSPACES, &workspace);
    }
    if let Some(id) = session_id {
        field(&mut blob, TRAJECTORY_SESSION_ID, id.as_bytes());
    }
    blob
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_names_its_workspaces() {
        let blob = test_blob(&["file:///tmp/a%20b", "file:///tmp/c"], Some("s-1"));
        assert_eq!(
            trajectory(&blob),
            Some(Trajectory {
                session_id: Some("s-1".into()),
                workspaces: vec!["file:///tmp/a%20b".into(), "file:///tmp/c".into()],
            })
        );
        assert_eq!(
            file_uri_path("file:///tmp/a%20b"),
            Some(PathBuf::from("/tmp/a b"))
        );
        assert_eq!(file_uri_path("https://x/y"), None);
        assert_eq!(file_uri_path("file://host/x"), None);
        // Truncated: malformed, never "no workspaces".
        assert_eq!(trajectory(&blob[..blob.len() - 2]), None);
    }
}
