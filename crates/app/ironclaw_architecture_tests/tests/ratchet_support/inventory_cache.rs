//! Memoized crate-directory inventory for the live workspace.
//!
//! Per-file ownership lookups (`owning_crate_name`) query the workspace crate
//! inventory once per scanned source file. Re-walking every `Cargo.toml` under
//! `crates/` per file made the whole-tree location scans quadratic (~180s per
//! scan). The real checkout does not mutate while an architecture test process
//! runs, so its inventory is cached once per process via `OnceLock`. Any other
//! root (e.g. tempdir trees built and mutated by sabotage self-tests) is walked
//! fresh on every call so self-tests can never observe stale state.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::{CRATES_ROOT, MIN_CRATE_DIRECTORIES, collect_crate_manifest_dirs, workspace_root};

/// Every crate directory under `crates/`, repo-relative, POSIX-separated,
/// sorted. Errors carry the reason so the caller can panic with it.
#[allow(dead_code)]
pub(crate) fn try_crate_directories(root: &Path) -> Result<Vec<String>, String> {
    static WORKSPACE_INVENTORY: OnceLock<(PathBuf, Result<Vec<String>, String>)> = OnceLock::new();
    let (workspace, inventory) = WORKSPACE_INVENTORY.get_or_init(|| {
        let workspace = workspace_root();
        let inventory = walk_crate_directories(&workspace);
        (workspace, inventory)
    });
    if root == workspace.as_path() {
        return inventory.clone();
    }
    walk_crate_directories(root)
}

fn walk_crate_directories(root: &Path) -> Result<Vec<String>, String> {
    let crates_root = root.join(CRATES_ROOT);
    if !crates_root.is_dir() {
        return Err(format!(
            "no {CRATES_ROOT}/ directory under {} — crate discovery cannot run. Every \
             path-keyed gate resolves its scope through this inventory; refusing rather \
             than scanning nothing is deliberate \
             (docs/internal/reborn/target-architecture/CHECKLIST.md WS10).",
            root.display()
        ));
    }

    let mut manifests = Vec::new();
    collect_crate_manifest_dirs(root, &crates_root, &mut manifests)?;
    // Shallowest first so the outermost owner of a path is always seen before
    // any manifest nested inside it.
    manifests.sort_by(|left, right| {
        (left.matches('/').count(), left.as_str())
            .cmp(&(right.matches('/').count(), right.as_str()))
    });

    let mut directories: Vec<String> = Vec::new();
    for candidate in manifests {
        if directories
            .iter()
            .any(|kept| candidate == *kept || candidate.starts_with(&format!("{kept}/")))
        {
            continue;
        }
        directories.push(candidate);
    }

    if directories.len() < MIN_CRATE_DIRECTORIES {
        return Err(format!(
            "crate discovery found only {} crate director(ies) under {} (floor is {}). \
             Either the crate tree moved out from under this gate or the workspace root is \
             wrong. Failing closed: a gate that scans nothing must never report success \
             (docs/internal/reborn/target-architecture/CHECKLIST.md WS10).",
            directories.len(),
            crates_root.display(),
            MIN_CRATE_DIRECTORIES
        ));
    }

    directories.sort();
    Ok(directories)
}
