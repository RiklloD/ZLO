use thiserror::Error;

/// Host-side computer-use failures. None of these strings reach the model:
/// a tool call answers with a [`crate::RefusalCode`] instead.
#[derive(Debug, Error)]
pub enum ComputerUseError {
    #[error("computer use config is invalid: {reason}")]
    InvalidConfig { reason: String },
    #[error("the computer-use server could not be started: {kind}")]
    Spawn { kind: std::io::ErrorKind },
    #[error("the computer-use server pipe failed: {kind}")]
    Io { kind: std::io::ErrorKind },
    #[error("the computer-use server exited")]
    Exited,
    #[error("the computer-use server did not answer in time")]
    Timeout,
    #[error("a computer-use server message exceeded {limit} bytes")]
    MessageTooLarge { limit: usize },
    #[error("the computer-use server sent an invalid message")]
    Protocol,
    /// A JSON-RPC error object: the process is healthy, the request was not.
    #[error("the computer-use server rejected the request: {message}")]
    Server { message: String },
}

impl ComputerUseError {
    pub(crate) fn io(error: std::io::Error) -> Self {
        Self::Io { kind: error.kind() }
    }
}
