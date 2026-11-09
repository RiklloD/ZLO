//! The thread workspace (`/workspace`) for clients that cannot see this
//! machine's disk: ZLO with a remote backend lists and mentions a thread's
//! files by their workspace path.
//!
//! - [`file_index`] answers `project_fs_index`: a bounded walk built from the
//!   thread's own `project_fs_list` listings on the inner surface, so thread
//!   ownership, `/workspace` confinement and the sensitive-name filter are
//!   that view's and are not re-implemented here. Paths come back relative
//!   to the workspace root; hidden entries and build/dependency folders are
//!   skipped the way ZLO's local index skips them.
//! - [`WorkspaceFolders`] names the host folder `/workspace` resolves to for
//!   a caller, so `@`-mention expansion can read what a remote client named
//!   (`mentions::fs` does the mapping, once). Composition implements it over
//!   the same mount policy the workspace views use.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceQueryRequest,
};
use ironclaw_product_contracts::workspace_views::{
    ProjectFsEntryKind, ProjectFsIndexEntry, RebornProjectFsIndexRequest,
    RebornProjectFsIndexResponse, RebornProjectFsListRequest, RebornProjectFsListResponse,
};

use crate::reborn_services::PROJECT_FS_LIST_VIEW;

/// The host folder the workspace alias names for a caller. `None` when this
/// deployment has none on local disk, or it cannot be resolved.
pub trait WorkspaceFolders: Send + Sync {
    fn host_folder(&self, caller: &ProductSurfaceCaller) -> Option<PathBuf>;
}

/// No workspace on local disk: workspace mentions stay links.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoWorkspaceFolders;

impl WorkspaceFolders for NoWorkspaceFolders {
    fn host_folder(&self, _caller: &ProductSurfaceCaller) -> Option<PathBuf> {
        None
    }
}

/// The workspace alias, as the listing views name it.
const ROOT: &str = ironclaw_attachments::WORKSPACE_ALIAS;

/// Bounds of one index walk.
#[derive(Debug, Clone, Copy)]
pub(crate) struct IndexLimits {
    /// Paths returned, files and folders together.
    pub(crate) max_entries: usize,
    /// Folders listed (one listing each).
    pub(crate) max_dirs: usize,
    /// Folder levels below the root.
    pub(crate) max_depth: usize,
    pub(crate) budget: Duration,
}

impl Default for IndexLimits {
    fn default() -> Self {
        // ZLO's local index walks at most 5,000 files, 12 levels deep.
        Self {
            max_entries: 5_000,
            max_dirs: 600,
            max_depth: 12,
            budget: Duration::from_secs(3),
        }
    }
}

/// Folders an index never enters: ZLO's local index's heavy folders (its
/// `.git` and `.venv` are covered by the hidden-name rule). `.gitignore`
/// files are not read here: that would mean parsing repository-controlled
/// files on the backend for a completion list.
const SKIPPED_DIRS: &[&str] = &["node_modules", "target", "dist", "build", "__pycache__"];

fn skipped(name: &str, kind: ProjectFsEntryKind) -> bool {
    name.starts_with('.')
        || (kind == ProjectFsEntryKind::Directory && SKIPPED_DIRS.contains(&name))
        || !matches!(
            kind,
            ProjectFsEntryKind::File | ProjectFsEntryKind::Directory
        )
}

/// Walk the thread's workspace breadth-first through its listing view.
pub(crate) async fn file_index(
    inner: &dyn ProductSurface,
    caller: &ProductSurfaceCaller,
    request: RebornProjectFsIndexRequest,
    limits: IndexLimits,
) -> Result<RebornProjectFsIndexResponse, ProductSurfaceError> {
    let deadline = Instant::now() + limits.budget;
    let mut entries = Vec::new();
    let mut complete = true;
    let mut listed = 0usize;
    // (scoped path, path relative to the root, depth)
    let mut queue: VecDeque<(String, String, usize)> = VecDeque::new();
    queue.push_back((ROOT.to_string(), String::new(), 0));
    while let Some((dir, rel, depth)) = queue.pop_front() {
        if listed >= limits.max_dirs || Instant::now() >= deadline {
            complete = false;
            break;
        }
        listed += 1;
        let listing = match list(inner, caller, &request.thread_id, &dir).await {
            Ok(listing) => listing,
            // The root's own failure (not the caller's thread, no
            // workspace) is the answer; a sub-folder that vanished mid-walk
            // only makes the index incomplete.
            Err(error) if rel.is_empty() => return Err(error),
            Err(error) => {
                tracing::debug!(?error, "workspace index skipped a folder");
                complete = false;
                continue;
            }
        };
        let mut names = listing.entries;
        names.sort_by(|a, b| a.name.cmp(&b.name));
        for entry in names {
            if skipped(&entry.name, entry.kind) || entry.name.contains('/') {
                continue;
            }
            if entries.len() >= limits.max_entries {
                complete = false;
                break;
            }
            let path = if rel.is_empty() {
                entry.name.clone()
            } else {
                format!("{rel}/{}", entry.name)
            };
            if entry.kind == ProjectFsEntryKind::Directory {
                if depth + 1 < limits.max_depth {
                    queue.push_back((format!("{dir}/{}", entry.name), path.clone(), depth + 1));
                } else {
                    complete = false;
                }
            }
            entries.push(ProjectFsIndexEntry {
                path,
                kind: entry.kind,
            });
        }
        if entries.len() >= limits.max_entries {
            complete = complete && queue.is_empty();
            break;
        }
    }
    Ok(RebornProjectFsIndexResponse {
        root: ROOT.to_string(),
        entries,
        complete: complete && queue.is_empty(),
    })
}

async fn list(
    inner: &dyn ProductSurface,
    caller: &ProductSurfaceCaller,
    thread_id: &str,
    path: &str,
) -> Result<RebornProjectFsListResponse, ProductSurfaceError> {
    let query = PROJECT_FS_LIST_VIEW.query(
        RebornProjectFsListRequest {
            thread_id: thread_id.to_string(),
            path: path.to_string(),
        },
        None,
    )?;
    let page = inner
        .query(
            caller.clone(),
            ProductSurfaceQueryRequest {
                view_id: query.view_id,
                input: query.params,
                cursor: None,
                limit: None,
            },
        )
        .await?;
    let payload = page
        .items
        .into_iter()
        .next()
        .ok_or_else(ProductSurfaceError::internal)?;
    serde_json::from_value(payload).map_err(ProductSurfaceError::internal_from)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use ironclaw_host_api::ids::{TenantId, UserId};
    use ironclaw_product_contracts::surface::{
        ProductSurfaceErrorCode, ProductSurfaceInvokeRequest, ProductSurfaceInvokeResponse,
        ProductSurfaceQueryPage, ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
    };
    use ironclaw_product_contracts::workspace_views::ProjectFsEntry;
    use serde_json::json;

    use super::*;

    /// The listing view over a fixed tree, owned by `alice` only; records
    /// every folder it was asked for.
    struct Tree {
        dirs: BTreeMap<String, Vec<(&'static str, ProjectFsEntryKind)>>,
        asked: Mutex<Vec<String>>,
    }

    impl Tree {
        fn new(dirs: &[(&str, &[(&'static str, ProjectFsEntryKind)])]) -> Self {
            Self {
                dirs: dirs
                    .iter()
                    .map(|(d, e)| (d.to_string(), e.to_vec()))
                    .collect(),
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ProductSurface for Tree {
        async fn invoke(
            &self,
            _: ProductSurfaceCaller,
            _: ProductSurfaceInvokeRequest,
        ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
            Err(ProductSurfaceError::not_found())
        }

        async fn query(
            &self,
            caller: ProductSurfaceCaller,
            request: ProductSurfaceQueryRequest,
        ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
            assert_eq!(request.view_id, PROJECT_FS_LIST_VIEW.id);
            if caller.user_id.as_str() != "alice" {
                return Err(ProductSurfaceError::not_found());
            }
            let path = request.input["path"].as_str().unwrap().to_string();
            self.asked.lock().unwrap().push(path.clone());
            let entries: Vec<ProjectFsEntry> = self
                .dirs
                .get(&path)
                .ok_or_else(ProductSurfaceError::not_found)?
                .iter()
                .map(|(name, kind)| ProjectFsEntry {
                    name: name.to_string(),
                    path: format!("{path}/{name}"),
                    kind: *kind,
                })
                .collect();
            Ok(ProductSurfaceQueryPage {
                items: vec![json!({ "entries": entries })],
                next_cursor: None,
            })
        }

        async fn stream_events(
            &self,
            _: ProductSurfaceCaller,
            _: ProductSurfaceStreamRequest,
        ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
            Err(ProductSurfaceError::not_found())
        }
    }

    fn caller(user: &str) -> ProductSurfaceCaller {
        ProductSurfaceCaller::new(
            TenantId::new("t1").unwrap(),
            UserId::new(user).unwrap(),
            None,
            None,
        )
    }

    fn request() -> RebornProjectFsIndexRequest {
        RebornProjectFsIndexRequest {
            thread_id: "thread-1".into(),
        }
    }

    use ProjectFsEntryKind::{Directory as D, File as F, Symlink as L};

    fn sample() -> Tree {
        Tree::new(&[
            (
                "/workspace",
                &[
                    ("src", D),
                    ("README.md", F),
                    (".git", D),
                    (".env.example", F),
                    ("node_modules", D),
                    ("target", D),
                    ("link", L),
                ],
            ),
            ("/workspace/src", &[("main.rs", F), ("deep", D)]),
            ("/workspace/src/deep", &[("a.rs", F)]),
        ])
    }

    fn paths(index: &RebornProjectFsIndexResponse) -> Vec<&str> {
        index.entries.iter().map(|e| e.path.as_str()).collect()
    }

    #[tokio::test]
    async fn the_index_lists_relative_paths_and_skips_hidden_and_heavy_folders() {
        let tree = sample();
        let index = file_index(&tree, &caller("alice"), request(), IndexLimits::default())
            .await
            .unwrap();
        assert_eq!(index.root, "/workspace");
        assert!(index.complete);
        assert_eq!(
            paths(&index),
            [
                "README.md",
                "src",
                "src/deep",
                "src/main.rs",
                "src/deep/a.rs"
            ]
        );
        let asked = tree.asked.lock().unwrap().clone();
        assert_eq!(
            asked,
            ["/workspace", "/workspace/src", "/workspace/src/deep"]
        );
        // Every path stays relative and inside the root.
        for path in paths(&index) {
            assert!(!path.starts_with('/') && !path.split('/').any(|c| c == ".." || c.is_empty()));
        }
    }

    #[tokio::test]
    async fn the_index_is_bounded_by_count_depth_folders_and_time() {
        let tree = sample();
        let few = IndexLimits {
            max_entries: 2,
            ..IndexLimits::default()
        };
        let index = file_index(&tree, &caller("alice"), request(), few)
            .await
            .unwrap();
        assert_eq!(index.entries.len(), 2);
        assert!(!index.complete);

        let shallow = IndexLimits {
            max_depth: 1,
            ..IndexLimits::default()
        };
        let index = file_index(&tree, &caller("alice"), request(), shallow)
            .await
            .unwrap();
        assert_eq!(paths(&index), ["README.md", "src"]);
        assert!(!index.complete);

        let one_dir = IndexLimits {
            max_dirs: 1,
            ..IndexLimits::default()
        };
        let index = file_index(&tree, &caller("alice"), request(), one_dir)
            .await
            .unwrap();
        assert_eq!(paths(&index), ["README.md", "src"]);
        assert!(!index.complete);

        let no_time = IndexLimits {
            budget: Duration::ZERO,
            ..IndexLimits::default()
        };
        let index = file_index(&tree, &caller("alice"), request(), no_time)
            .await
            .unwrap();
        assert!(index.entries.is_empty() && !index.complete);
    }

    #[tokio::test]
    async fn another_users_thread_is_refused_by_the_listing_view() {
        let tree = sample();
        let refused = file_index(&tree, &caller("mallory"), request(), IndexLimits::default())
            .await
            .unwrap_err();
        assert_eq!(refused.code, ProductSurfaceErrorCode::NotFound);
    }
}
