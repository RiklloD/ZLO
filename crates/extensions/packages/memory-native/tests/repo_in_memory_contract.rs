//! Wires [`InMemoryMemoryDocumentRepository`] to the shared
//! [`MemoryDocumentRepository`] contract suite.
//!
//! See `crates/extensions/packages/memory-native/src/contract_tests.rs` for the suite
//! itself and the rationale (#3890 / .claude/rules/testing.md). One
//! `contract_test!` invocation expands to one `#[tokio::test]` per
//! contract, named `in_memory::<contract_name>` so failures attribute
//! cleanly to this impl.

use ironclaw_memory::{MemoryDocumentPath, MemoryDocumentScope};
use ironclaw_memory_native::{
    InMemoryMemoryDocumentRepository, MemoryDocumentRepository, contract_test,
};

contract_test!(in_memory, InMemoryMemoryDocumentRepository::new);

#[test]
fn memory_scope_rejects_segments_that_cannot_round_trip_or_collide_in_owner_keys() {
    assert!(MemoryDocumentScope::new("tenant:admin", "alice", None).is_err());
    assert!(MemoryDocumentScope::new(".", "alice", None).is_err());
    assert!(MemoryDocumentScope::new("tenant-a", "..", None).is_err());
}

#[tokio::test]
async fn in_memory_repository_rejects_file_directory_prefix_conflicts() {
    let repo = InMemoryMemoryDocumentRepository::new();
    let file = MemoryDocumentPath::new("tenant-a", "alice", None, "notes").unwrap();
    let child = MemoryDocumentPath::new("tenant-a", "alice", None, "notes/a.md").unwrap();

    repo.write_document(&file, b"plain file").await.unwrap();
    let err = repo.write_document(&child, b"child").await.unwrap_err();
    assert!(err.to_string().contains("existing file ancestor"));

    let repo = InMemoryMemoryDocumentRepository::new();
    repo.write_document(&child, b"child").await.unwrap();
    let err = repo.write_document(&file, b"plain file").await.unwrap_err();
    assert!(err.to_string().contains("existing directory"));
}
