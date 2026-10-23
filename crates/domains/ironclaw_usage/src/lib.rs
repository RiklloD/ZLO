//! Subscription usage windows for the Usage page: CLI-login 5-hour and weekly
//! windows and coding-plan quotas, fetched on demand through
//! `ironclaw_network` and cached per provider.
//!
//! Provider logic is ported from CodexBar (<https://github.com/steipete/CodexBar>,
//! MIT, Peter Steinberger): endpoints, headers, response fields and the
//! percentage/reset rules. Every endpoint is undocumented, so parsing treats
//! every field as optional and drops a window rather than failing a provider.

mod claude;
mod http;
mod monitor;
mod zai;

#[cfg(test)]
mod tests;

pub use claude::ClaudeAccount;
pub use monitor::{UsageMonitor, UsageSource};
pub use zai::ZaiKey;
