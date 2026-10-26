//! Computer use for IronClaw: the Open Computer Use binary
//! (`OpenComputerUse mcp`) run as a stdio MCP server, one process per
//! thread, plus the layer the Hermes `computer-use-plus` plugin put on top of
//! it, ported to Rust: sessions, Stop, take over / hand back, the system-prompt
//! section, and the live view (activity, cursor, window pictures) for a route
//! to read. No UI lives here.
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
//! | `frame` | Result parsing: text and the screenshot frame |
//! | `target` | What a session learnt about its window: app, pid, window id, element labels, screenshot size |
//! | `windows` | CoreGraphics window list: an app's CGWindowID and bounds |
//! | `live` | The live view: activity, windows used, pointer, typing, the desktop take-over lease |
//! | `capture` | Window pictures for the viewer: `screencapture -l`, the agent's screenshot as fallback, shared producers |
//! | `cursor` | The viewer's cursor: coordinate targets and the native cursor's observation file |
//! | `prompt` | The system-prompt section: server instructions, tool names, fork guidance |
//! | `trace` | One JSON line per call (tool, ok, code, seconds) |
//! | `args` | Arguments repaired against the tool's schema (numeric and boolean strings, "null" optionals) |
//! | `catalog` | The server's tool definitions, embedded verbatim |
//! | `client` | The `ironclaw_mcp::McpClient` and `ComputerUseSessions` port implementations |

mod args;
mod capture;
mod catalog;
mod client;
mod config;
mod cursor;
mod error;
mod frame;
mod live;
mod prompt;
mod session;
mod stdio;
mod target;
mod trace;
mod windows;

#[cfg(test)]
mod tests;

pub use catalog::{computer_use_tool, computer_use_tools, tool_for_capability};
pub use client::COMPUTER_USE_TRANSPORT;
pub use config::ComputerUseConfig;
pub use error::ComputerUseError;
pub use frame::Frame;
pub use session::{ComputerUse, ComputerUseOutcome, RefusalCode, SessionState, SessionStatus};
