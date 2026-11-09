//! UI-free client for the IronClaw WebChat v2 gateway.
//!
//! The desktop app talks to `zlo serve` exactly like the browser does:
//! bearer-authenticated HTTP for reads and mutations, SSE for live thread
//! events. Nothing here knows about the UI toolkit, so it can be tested and
//! reused on its own, and a Rune upgrade never touches it.

mod api;
pub mod attachments;
mod backend;
pub mod commands;
pub mod computer_use;
mod connection;
pub mod environment_calls;
pub mod environments;
mod error;
mod history_calls;
mod inbox_calls;
pub mod keychain;
mod memory_calls;
pub mod mentions;
pub mod operator;
pub mod orchestration;
mod pages;
pub mod pairing;
mod profile_calls;
pub mod repo_calls;
pub mod review;
pub mod schedules;
mod settings;
pub mod ssh;
mod stream;
pub mod tailnet;
#[cfg(test)]
mod test_http;
pub mod transcript;
pub mod update_calls;
pub mod usage;
pub mod visual;
pub mod wire;

pub use api::{Client, GateDecision, client_action_id};
pub use backend::Backend;
pub use connection::{Connection, Token, prepare_reborn_home, reborn_home};
pub use error::{ApiErrorBody, ClientError};
pub use pages::{AutomationAction, FilePreview, PREVIEW_LIMIT_BYTES};
pub use stream::{EventStream, StreamMessage, StreamStatus};

pub use ironclaw_extension_contracts::hosted_mcp;
pub use ironclaw_extension_contracts::state as extension_state;
/// Runtime-native thread actions (Codex review, revert, compact).
pub use ironclaw_product_contracts::orchestration::{
    ReviewTarget, ThreadAction, ThreadActionOutcome, ThreadActionRefusal,
};
/// Contract types the app renders directly.
pub use ironclaw_product_contracts::outbound as contract;
pub use ironclaw_product_contracts::{
    environment_access, history, hooks_catalog, inbound_requests, inbox, memory_page,
    notification_inbox, operator_llm, package_lifecycle, product_wire, profiles, self_update,
    session_import, task_models, workspace_views,
};
