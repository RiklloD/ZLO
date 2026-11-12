//! The bundled memory provider for IronClaw Reborn.
//!
//! This crate owns memory-specific path grammar and repository seams. The
//! generic filesystem crate owns only virtual path authority, scoped mounts,
//! backend cataloging, and backend routing.

mod backend;
mod chunking;
#[cfg(any(test, feature = "test-support"))]
pub mod contract_tests;
mod embedding;
mod events;
mod indexer;
mod metadata;
mod path;
mod repo;
mod safety;
mod schema;
mod search;
mod service;
mod write_metadata;

/// This package's bundled `reborn.extension_manifest.v3` manifest, exposed
/// through the owning package's public API so the host registers it without
/// compiling this package's tree into itself (§11.2.7 — a relative
/// `include_str!` that escapes its own crate root bypasses the dependency
/// graph and breaks on every package move).
pub const MANIFEST_TOML: &str = include_str!("../manifest.toml");

/// The input schemas this package's manifest references, keyed by the exact
/// schema ref the manifest declares. Native memory rides the always-on lane, so
/// the host serves these inline rather than materializing them to a package
/// root; the table is the single source of truth for that lookup.
pub const MEMORY_INPUT_SCHEMAS: &[(&str, &str)] = &[
    (
        "schemas/memory/document-read.input.v1.json",
        include_str!("../schemas/memory/document-read.input.v1.json"),
    ),
    (
        "schemas/memory/document-write.input.v1.json",
        include_str!("../schemas/memory/document-write.input.v1.json"),
    ),
    (
        "schemas/memory/search.input.v1.json",
        include_str!("../schemas/memory/search.input.v1.json"),
    ),
    (
        "schemas/memory/tree.input.v1.json",
        include_str!("../schemas/memory/tree.input.v1.json"),
    ),
    (
        "schemas/memory/profile-set.input.v1.json",
        include_str!("../schemas/memory/profile-set.input.v1.json"),
    ),
];

pub use backend::{MemoryBackend, MemoryBackendCapabilities, RepositoryMemoryBackend};
pub use chunking::{ChunkConfig, MemoryChunkWrite, chunk_document};
pub use embedding::{
    MemoryChunkEmbedding, MemoryEmbedder, MemoryEmbeddingError, MemoryEmbeddingFingerprint,
};
pub use indexer::{
    ChunkingMemoryDocumentIndexer, MemoryChunkReplaceOutcome, MemoryDocumentIndexRepository,
    MemoryDocumentIndexer,
};
pub use metadata::{MemoryBackendWriteOptions, MemoryWriteOptions};
pub use repo::{
    FilesystemMemoryDocumentRepository, InMemoryMemoryDocumentRepository, MemoryAppendOutcome,
    MemoryDocumentHistory, MemoryDocumentRepository, MemoryWriteOutcome,
};
pub use safety::DefaultPromptWriteSafetyPolicy;
pub use search::{DEFAULT_MIN_VECTOR_SIMILARITY, MemorySearchRequest, MemorySearchResult};
pub use service::{
    MEMORY_ASSETS, MEMORY_CURATION_PASS_PROMPT, MEMORY_CURATION_PASS_PROMPT_REF, MEMORY_GUIDANCE,
    MEMORY_GUIDANCE_DOC_REF, NativeMemoryService, OwnerWriteOutcome, resolve_target_path,
};
