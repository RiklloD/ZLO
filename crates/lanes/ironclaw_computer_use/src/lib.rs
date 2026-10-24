//! Computer use for IronClaw: the Open Computer Use binary
//! (`OpenComputerUse mcp`) run as a stdio MCP server, one process per
//! thread, plus the layer the Hermes `computer-use-plus` plugin put on top of
//! it, ported to Rust: sessions, Stop, take over / hand back, and the latest
//! screenshot frame for a route to read. No UI lives here.
//!
//! This crate runs already-approved calls. Approval is the caller's job:
//! `ironclaw_assistant::computer_use` offers each tool as a capability that
//! raises an approval request on every call, and reaches this crate only
//! through the `ironclaw_product_contracts::computer_use::ComputerUseSessions`
//! port. Off by default; see
//! [`ComputerUseConfig::from_env`].
//!
//! | Module | Owns |
//! |---|---|
//! | `config` | Env flags, binary path, timeouts, bounds, the scrubbed server environment |
//! | `stdio` | One server process: spawn, MCP handshake, bounded JSON-RPC lines, kill on drop |
//! | `session` | Sessions keyed by `ComputerUseSession` (tenant, user, thread), Stop, take over, idle reap, outcomes |
//! | `frame` | Result parsing: text, the target app/window, the screenshot frame |
//! | `catalog` | The server's tool definitions, embedded verbatim |
//! | `client` | The `ironclaw_mcp::McpClient` and `ComputerUseSessions` port implementations |

mod catalog;
mod client;
mod config;
mod error;
mod frame;
mod session;
mod stdio;

#[cfg(test)]
mod tests;

pub use catalog::{computer_use_tool, computer_use_tools, tool_for_capability};
pub use client::COMPUTER_USE_TRANSPORT;
pub use config::ComputerUseConfig;
pub use error::ComputerUseError;
pub use frame::Frame;
pub use session::{ComputerUse, ComputerUseOutcome, RefusalCode, SessionState, SessionStatus};
