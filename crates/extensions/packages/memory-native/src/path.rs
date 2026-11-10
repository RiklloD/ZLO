//! Memory path grammar, scope, and validation.
//!
//! The public scope/path value types and segment/relative-path validators moved
//! to `ironclaw_memory` and are re-exported below. The repository-facing
//! virtual-path parser and the sanitized backend-error helpers stay here because
//! they depend on `ironclaw_filesystem`.

use std::sync::OnceLock;

use ironclaw_filesystem::{FilesystemError, FilesystemOperation};
use ironclaw_host_api::path::VirtualPath;
use ironclaw_memory::MemoryDocumentScope;

pub(crate) fn memory_backend_unsupported(
    scope: &MemoryDocumentScope,
    operation: FilesystemOperation,
    reason: impl Into<String>,
) -> FilesystemError {
    memory_error(
        scope
            .virtual_prefix()
            .unwrap_or_else(|_| valid_memory_path()),
        operation,
        reason,
    )
}

pub(crate) fn memory_error(
    path: VirtualPath,
    operation: FilesystemOperation,
    reason: impl Into<String>,
) -> FilesystemError {
    let reason = sanitize_memory_backend_reason(reason.into());
    FilesystemError::Backend {
        path,
        operation,
        reason,
    }
}

const MEMORY_BACKEND_DETAIL_MARKERS: &[&str] = &[
    "no such table",
    "drop table",
    "sql",
    "sqlite",
    "libsql",
    "postgres error",
    "database error",
    "connection refused",
    "timeout",
    "host=",
    "port=",
    "reborn_memory_",
    "/tmp/",
    "/var/folders/",
    "/private/",
    "/workspace/",
    "/home/",
    "\\appdata\\",
];

fn sanitize_memory_backend_reason(reason: String) -> String {
    let lower = reason.to_ascii_lowercase();
    if MEMORY_BACKEND_DETAIL_MARKERS
        .iter()
        .any(|marker| lower.as_str().contains(marker))
    {
        "memory backend operation failed".to_string()
    } else {
        reason
    }
}

pub(crate) fn valid_memory_path() -> VirtualPath {
    static MEMORY_PATH: OnceLock<VirtualPath> = OnceLock::new();
    // safety: `/memory` is a registered VIRTUAL_ROOT in ironclaw_host_api::path.
    // If construction fails, host_api's VIRTUAL_ROOTS list is out of sync with
    // this crate at build time, which is a build-system invariant violation.
    MEMORY_PATH
        .get_or_init(|| VirtualPath::new("/memory").expect("/memory is a registered VIRTUAL_ROOT")) // safety: `/memory` is a registered VIRTUAL_ROOT.
        .clone()
}

#[cfg(test)]
mod path_validation_tests {
    use ironclaw_memory::validated_memory_relative_path;

    /// Legal user document paths must
    /// not collide with the repository's sidecar suffix namespace.
    #[test]
    fn rejects_path_segments_ending_in_reserved_sidecar_suffixes() {
        for reserved in [
            "foo.meta",
            "subdir/foo.meta",
            "data.chunks",
            "data.chunks/inner",
            "history.versions",
            "history.versions/2",
        ] {
            let err = validated_memory_relative_path(reserved.to_string()).expect_err(reserved);
            let msg = format!("{err}");
            assert!(
                msg.contains(".meta") || msg.contains(".chunks") || msg.contains(".versions"),
                "expected reserved-suffix rejection in error: {msg}"
            );
        }
    }

    #[test]
    fn accepts_non_reserved_paths_with_dots_in_names() {
        for ok in [
            "foo.md",
            "subdir/foo.txt",
            "metadata-foo",
            "chunks-of-bread",
            "version-1.txt",
        ] {
            validated_memory_relative_path(ok.to_string())
                .expect("non-reserved path must be accepted");
        }
    }
}
