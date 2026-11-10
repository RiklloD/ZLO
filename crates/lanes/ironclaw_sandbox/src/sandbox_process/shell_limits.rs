//! Defaults and the operator timeout ceiling for `builtin.shell`, shared by
//! the sandbox transports.

/// Default shell command timeout when the model omits `timeout`.
pub(crate) const SHELL_TIMEOUT_DEFAULT_SECS: u64 = 120;
/// Operator ceiling for `timeout`.
pub(crate) const SHELL_TIMEOUT_MAX_SECS: u64 = 600;
/// Default captured-output cap (stdout+stderr) when the model omits
/// `output_limit`.
pub(crate) const SHELL_OUTPUT_LIMIT_DEFAULT_BYTES: u64 = 64 * 1024;
