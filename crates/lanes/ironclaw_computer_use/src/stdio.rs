//! One `OpenComputerUse mcp` process spoken to over newline-delimited
//! JSON-RPC on stdio. Dropping a [`StdioServer`] kills the process, so a
//! cancelled, timed-out or failed call can never leave a late reply behind
//! for the next call to read.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::config::{ComputerUseConfig, INHERITED_ENV};
use crate::error::ComputerUseError;

const PROTOCOL_VERSION: &str = "2025-03-26";
const SERVER_ERROR_CHARS: usize = 300;

pub(crate) struct StdioServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    max_message_bytes: usize,
}

impl StdioServer {
    /// Spawn the server and run the MCP handshake.
    pub(crate) async fn start(config: &ComputerUseConfig) -> Result<Self, ComputerUseError> {
        let mut command = Command::new(&config.binary);
        command.args(&config.args).env_clear();
        for name in INHERITED_ENV {
            if let Ok(value) = std::env::var(name) {
                command.env(name, value);
            }
        }
        command
            .envs(config.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| ComputerUseError::Spawn { kind: error.kind() })?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(ComputerUseError::Exited);
        };
        let mut server = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
            max_message_bytes: config.max_message_bytes,
        };
        server
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "ironclaw-computer-use", "version": env!("CARGO_PKG_VERSION")},
                }),
                config.start_timeout,
            )
            .await?;
        server.notify("notifications/initialized").await?;
        Ok(server)
    }

    pub(crate) fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// One request; replies with another id and notifications are skipped.
    pub(crate) async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ComputerUseError> {
        self.next_id += 1;
        let id = self.next_id;
        let exchange = async {
            self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
                .await?;
            loop {
                let line = self.read_line().await?;
                let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                    continue; // silent-ok: stray non-JSON output is not a reply
                };
                if message.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(error) = message.get("error") {
                    let text = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("request failed");
                    return Err(ComputerUseError::Server {
                        message: text.chars().take(SERVER_ERROR_CHARS).collect(),
                    });
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or(ComputerUseError::Protocol);
            }
        };
        tokio::time::timeout(timeout, exchange)
            .await
            .map_err(|_elapsed| ComputerUseError::Timeout)?
    }

    pub(crate) async fn notify(&mut self, method: &str) -> Result<(), ComputerUseError> {
        self.send(&json!({"jsonrpc": "2.0", "method": method}))
            .await
    }

    /// Close stdin and kill; waits briefly so the child is reaped.
    pub(crate) async fn shutdown(mut self) {
        let _ = self.stdin.shutdown().await; // silent-ok: the process is being killed anyway
        let _ = self.child.start_kill(); // silent-ok: it may have exited already
        let _ = tokio::time::timeout(Duration::from_secs(2), self.child.wait()).await; // silent-ok: kill_on_drop backs this up
    }

    async fn send(&mut self, message: &Value) -> Result<(), ComputerUseError> {
        let mut line = serde_json::to_vec(message).map_err(|_error| ComputerUseError::Protocol)?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .map_err(ComputerUseError::io)?;
        self.stdin.flush().await.map_err(ComputerUseError::io)
    }

    /// One line, bounded before it is buffered.
    async fn read_line(&mut self) -> Result<Vec<u8>, ComputerUseError> {
        let limit = self.max_message_bytes;
        let mut line = Vec::new();
        loop {
            let buffer = self.stdout.fill_buf().await.map_err(ComputerUseError::io)?;
            if buffer.is_empty() {
                return Err(ComputerUseError::Exited);
            }
            let (take, done) = match buffer.iter().position(|byte| *byte == b'\n') {
                Some(end) => (end, true),
                None => (buffer.len(), false),
            };
            if line.len() + take > limit {
                return Err(ComputerUseError::MessageTooLarge { limit });
            }
            line.extend_from_slice(&buffer[..take]);
            self.stdout.consume(if done { take + 1 } else { take });
            if done {
                return Ok(line);
            }
        }
    }
}
