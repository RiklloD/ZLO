//! Supermemory for IronClaw.
//!
//! With an API key in the secret store (`supermemory_api_key`) and a container
//! binding for the caller, Supermemory is that caller's only memory: automatic
//! recall before a turn, automatic capture after it, and the
//! `supermemory.search` / `supermemory.write` model tools. Without a key the
//! feature is off and nothing here is called.
//!
//! This crate owns the HTTP client, the binding seam, the recall/capture/tool
//! operations, and the two memory-seam decorators (prompt context and
//! after-turn writer). The synthetic tool registration and its approval gate
//! live in `ironclaw_assistant::supermemory`.
//!
//! Behaviour follows the Hermes `supermemory` plugin: profile + search recall
//! queried with the first 200 characters of the user message, one capture
//! document per thread per 4-hour window (`customId` bucket) holding the turn in
//! `[role: x]...[x:end]` form, injected context and inline images stripped
//! before capture, writes to the primary container only.

mod adapters;
mod binding;
mod capture;
mod client;
mod config;
mod error;
mod recall;
mod service;
mod tools;
mod wire;

pub use adapters::{SupermemoryInteractionRecorder, SupermemoryPromptContext};
pub use binding::{
    ContainerTag, StaticSupermemoryBindingSource, SupermemoryBinding, SupermemoryBindingSource,
    SupermemoryCaller,
};
pub use capture::CapturedTurn;
pub use client::SupermemoryClient;
pub use config::{SUPERMEMORY_API_KEY_SECRET, SupermemoryConfig};
pub use error::SupermemoryError;
pub use recall::{RecallItem, RecallKind};
pub use service::{Recall, SearchHit, Supermemory, WriteEvidence};
pub use tools::{
    SUPERMEMORY_SEARCH_CAPABILITY_ID, SUPERMEMORY_SEARCH_DESCRIPTION, SUPERMEMORY_SEARCH_TOOL_NAME,
    SUPERMEMORY_WRITE_CAPABILITY_ID, SUPERMEMORY_WRITE_DESCRIPTION, SUPERMEMORY_WRITE_TOOL_NAME,
    hides_local_memory_capability, run_search, run_write, search_schema, validate_arguments,
    write_schema,
};

#[cfg(test)]
mod tests;
