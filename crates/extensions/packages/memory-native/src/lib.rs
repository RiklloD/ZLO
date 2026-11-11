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

pub use backend::{MemoryBackend, MemoryBackendCapabilities, RepositoryMemoryBackend};
pub use chunking::{ChunkConfig, MemoryChunkWrite, chunk_document};
pub use embedding::{MemoryEmbedder, MemoryEmbeddingError};
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
pub use search::{MemorySearchRequest, MemorySearchResult};
pub use service::{
    MEMORY_ASSETS, MEMORY_CURATION_PASS_PROMPT, MEMORY_CURATION_PASS_PROMPT_REF, MEMORY_GUIDANCE,
    MEMORY_GUIDANCE_DOC_REF, NativeMemoryService, OwnerWriteOutcome, resolve_target_path,
};
