//! GPUI-free client for the IronClaw WebChat v2 gateway.
//!
//! The desktop app talks to `ironclaw serve` exactly like the browser does:
//! bearer-authenticated HTTP for reads and mutations, SSE for live thread
//! events. Nothing here knows about GPUI, so it can be tested and reused on
//! its own, and a GPUI upgrade never touches it.

mod api;
pub mod attachments;
mod backend;
pub mod commands;
mod connection;
mod error;
pub mod operator;
mod pages;
mod profile_calls;
mod settings;
mod stream;
pub mod transcript;
pub mod wire;

pub use api::{Client, GateDecision, client_action_id};
pub use backend::Backend;
pub use connection::{Connection, Token, reborn_home};
pub use error::{ApiErrorBody, ClientError};
pub use pages::{AutomationAction, FilePreview, PREVIEW_LIMIT_BYTES};
pub use stream::{EventStream, StreamMessage, StreamStatus};

pub use ironclaw_extension_contracts::hosted_mcp;
pub use ironclaw_extension_contracts::state as extension_state;
/// Contract types the app renders directly.
pub use ironclaw_product_contracts::outbound as contract;
pub use ironclaw_product_contracts::{
    notification_inbox, operator_llm, package_lifecycle, product_wire, profiles, workspace_views,
};
