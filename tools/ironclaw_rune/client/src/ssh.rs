//! The SSH route (for people without Tailscale): the computer's backend
//! stays on its loopback, and ZLO reaches it through the user's own SSH
//! key with the system `ssh`.
//!
//! - Pairing: `ssh user@host zlo pair --json` (then `~/.zlo/bin/zlo`, where
//!   installs put it, then the pre-ZLO `ironclaw` names for computers that
//!   have not updated yet) prints a one-time code; it is redeemed through
//!   the tunnel like any other code.
//! - The tunnel: `ssh -N -L 127.0.0.1:<free port>:127.0.0.1:<backend port>
//!   user@host`, and the client talks to `http://127.0.0.1:<free port>`
//!   (loopback http passes the transport rule).
//!
//! Every invocation is `BatchMode=yes` (no password or passphrase prompt,
//! no unknown host key accepted) and its arguments are a list, never a
//! shell string: the user and host typed into ZLO are checked by
//! [`parse_target`] and passed as one argument after `--`. The only text
//! the remote shell sees is [`REMOTE_PAIR_COMMANDS`], fixed here.

use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::environments::{host_of, normalize_code};

/// The system client (never one found on `PATH`).
pub const SSH: &str = "/usr/bin/ssh";
/// `zlo serve`'s default port, on the computer's loopback.
pub const DEFAULT_BACKEND_PORT: u16 = 3000;
const DEFAULT_SSH_PORT: u16 = 22;
const CONNECT_TIMEOUT_SECS: u32 = 8;
/// How long a new tunnel gets to start listening.
pub const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// What runs on the computer to make a pairing code, in order: on `PATH`,
/// then where the installer puts it (a non-interactive SSH session often
/// has no `~/.zlo/bin` on its `PATH`), then the same two under the pre-ZLO
/// `ironclaw` name, for a computer that has not updated yet.
pub const REMOTE_PAIR_COMMANDS: [&str; 4] = [
    "zlo pair --json",
    "~/.zlo/bin/zlo pair --json",
    "ironclaw pair --json",
    "~/.ironclaw/bin/ironclaw pair --json",
];

/// `user@host[:port]`, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshTarget {
    pub user: Option<String>,
    /// No brackets, also for IPv6.
    pub host: String,
    pub port: u16,
}

impl SshTarget {
    /// What `ssh` is given as its destination: `user@host`, or `host`.
    pub fn destination(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// `ssh://user@host:22`, as saved in `environments.json`.
    pub fn to_url(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match &self.user {
            Some(user) => format!("ssh://{user}@{host}:{}", self.port),
            None => format!("ssh://{host}:{}", self.port),
        }
    }

    pub fn from_url(url: &str) -> Result<Self, SshError> {
        let rest = url
            .trim()
            .strip_prefix("ssh://")
            .ok_or(SshError::BadTarget)?;
        parse_target(rest.trim_end_matches('/'))
    }
}

/// Why an SSH step did not work, in words for the person.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SshError {
    #[error("Type the computer as user@host, or user@host:port for another SSH port.")]
    BadTarget,
    #[error("Set up an SSH key for {0} first (ZLO never asks for a password).")]
    AuthFailed(String),
    #[error(
        "This Mac doesn't know {host}'s SSH key yet. Run `ssh {destination}` once in Terminal to trust it, then try again."
    )]
    UnknownHostKey { host: String, destination: String },
    #[error("Couldn't reach {0} over SSH. Check the address and that it is awake.")]
    Unreachable(String),
    #[error("IronClaw isn't installed on {0}")]
    NotInstalled(String),
    #[error("IronClaw on {host} didn't make a pairing code: {line}")]
    PairFailed { host: String, line: String },
    #[error("Couldn't open a tunnel to {0}")]
    TunnelFailed(String),
    #[error("ZLO couldn't run ssh: {0}")]
    NoSsh(String),
}

fn user_ok(user: &str) -> bool {
    !user.is_empty()
        && user.len() <= 64
        && !user.starts_with('-')
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn host_ok(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && !host.starts_with('-')
        && !host.starts_with('.')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn ipv6_ok(host: &str) -> bool {
    host.parse::<std::net::Ipv6Addr>().is_ok()
}

/// `user@host[:port]` (IPv6 as `user@[::1]:22`). Anything that could be
/// read as an option or by a shell (whitespace, a leading `-`, `;`, `$`,
/// quotes, …) is refused.
pub fn parse_target(raw: &str) -> Result<SshTarget, SshError> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(char::is_whitespace) {
        return Err(SshError::BadTarget);
    }
    let (user, rest) = match raw.rsplit_once('@') {
        Some((user, rest)) => {
            if !user_ok(user) {
                return Err(SshError::BadTarget);
            }
            (Some(user.to_string()), rest)
        }
        None => (None, raw),
    };
    let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
        let (host, after) = v6.split_once(']').ok_or(SshError::BadTarget)?;
        if !ipv6_ok(host) {
            return Err(SshError::BadTarget);
        }
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or(SshError::BadTarget)?),
        };
        (host, port)
    } else {
        match rest.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        }
    };
    if !host.contains(':') && !host_ok(host) {
        return Err(SshError::BadTarget);
    }
    let port = match port {
        None => DEFAULT_SSH_PORT,
        Some(p) => p
            .parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .ok_or(SshError::BadTarget)?,
    };
    Ok(SshTarget {
        user,
        host: host.to_ascii_lowercase(),
        port,
    })
}

fn common_args(target: &SshTarget) -> Vec<String> {
    [
        "-o",
        "BatchMode=yes",
        "-o",
        &format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
        // No agent or X11 forwarding to the computer, ever.
        "-a",
        "-x",
        "-p",
        &target.port.to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// `ssh … -- user@host <remote command>` for one of
/// [`REMOTE_PAIR_COMMANDS`].
pub fn pair_args(target: &SshTarget, remote_command: &str) -> Vec<String> {
    let mut args = common_args(target);
    args.push("-T".into());
    args.push("--".into());
    args.push(target.destination());
    args.push(remote_command.to_string());
    args
}

/// `ssh -N -L 127.0.0.1:<local>:127.0.0.1:<remote> … -- user@host`.
pub fn tunnel_args(target: &SshTarget, local_port: u16, remote_port: u16) -> Vec<String> {
    let mut args = vec!["-N".to_string()];
    args.extend(common_args(target));
    for option in [
        "ExitOnForwardFailure=yes",
        // A dead link is noticed in under a minute and the tunnel restarts.
        "ServerAliveInterval=15",
        "ServerAliveCountMax=3",
    ] {
        args.push("-o".into());
        args.push(option.into());
    }
    args.push("-L".into());
    args.push(format!("127.0.0.1:{local_port}:127.0.0.1:{remote_port}"));
    args.push("--".into());
    args.push(target.destination());
    args
}

/// A finished `ssh` run.
#[derive(Clone, Debug, Default)]
pub struct Ran {
    /// `None` when killed by a signal.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// What `zlo pair --json` printed.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteCode {
    /// `XXXX-XXXX-XXXX`.
    pub code: String,
    /// The backend's port on the computer, when its link names its own
    /// loopback listener; `None`: [`DEFAULT_BACKEND_PORT`].
    pub backend_port: Option<u16>,
}

impl std::fmt::Debug for RemoteCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteCode")
            .field("code", &"<redacted>")
            .field("backend_port", &self.backend_port)
            .finish()
    }
}

#[derive(Deserialize)]
struct PairJson {
    code: String,
    #[allow(dead_code)]
    expires_at: String,
    #[serde(default)]
    link: String,
}

/// The JSON object `pair --json` prints, compact or pretty (it prints it
/// pretty, over several lines). A login banner before it does no harm: the
/// last line that opens an object and parses from there to the end wins.
pub fn parse_pair_output(stdout: &[u8]) -> Option<RemoteCode> {
    let text = String::from_utf8_lossy(stdout);
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with('{') {
            starts.push(offset + (line.len() - line.trim_start().len()));
        }
        offset += line.len();
    }
    let json: PairJson = starts
        .into_iter()
        .rev()
        .find_map(|start| serde_json::from_str(text[start..].trim_end()).ok())?;
    let code = normalize_code(&json.code)?;
    Some(RemoteCode {
        code,
        backend_port: loopback_port(&json.link),
    })
}

/// `http://127.0.0.1:3005/pair#…` → `3005`; any other host → `None`.
fn loopback_port(link: &str) -> Option<u16> {
    let host = host_of(link);
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !loopback {
        return None;
    }
    let authority = link.split_once("://")?.1.split(['/', '?', '#']).next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}

fn last_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// An `ssh` failure of its own (exit 255, or the tunnel ending early), by
/// what it printed.
pub fn classify_ssh_failure(target: &SshTarget, stderr: &[u8]) -> Option<SshError> {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if text.contains("permission denied") || text.contains("too many authentication failures") {
        return Some(SshError::AuthFailed(target.destination()));
    }
    if text.contains("host key verification failed")
        || text.contains("no matching host key")
        || text.contains("remote host identification has changed")
    {
        return Some(SshError::UnknownHostKey {
            host: target.host.clone(),
            destination: target.destination(),
        });
    }
    if [
        "could not resolve hostname",
        "connection refused",
        "timed out",
        "no route to host",
        "network is unreachable",
        "connection closed",
        "connection reset",
    ]
    .iter()
    .any(|m| text.contains(m))
    {
        return Some(SshError::Unreachable(target.host.clone()));
    }
    None
}

fn not_found(ran: &Ran) -> bool {
    let text = String::from_utf8_lossy(&ran.stderr).to_ascii_lowercase();
    ran.code == Some(127)
        || (ran.code != Some(0)
            && (text.contains("command not found") || text.contains("no such file")))
}

/// Get a pairing code from the computer, running `ssh` with `run` (each
/// call one `ssh` with these arguments). Blocking.
pub fn remote_pair_code<F>(target: &SshTarget, mut run: F) -> Result<RemoteCode, SshError>
where
    F: FnMut(&[String]) -> std::io::Result<Ran>,
{
    for command in REMOTE_PAIR_COMMANDS {
        let ran = run(&pair_args(target, command)).map_err(|e| SshError::NoSsh(e.to_string()))?;
        if ran.code == Some(0) {
            return parse_pair_output(&ran.stdout).ok_or_else(|| SshError::PairFailed {
                host: target.host.clone(),
                line: "it answered in an unexpected way. Update IronClaw there.".into(),
            });
        }
        if ran.code == Some(255) {
            return Err(classify_ssh_failure(target, &ran.stderr)
                .unwrap_or_else(|| SshError::Unreachable(target.host.clone())));
        }
        if not_found(&ran) {
            continue;
        }
        let line = last_line(&ran.stderr);
        let line = if line.contains("unrecognized subcommand") {
            "this IronClaw has no `pair` command yet. Update IronClaw there.".to_string()
        } else if line.is_empty() {
            format!(
                "it exited with {:?}. Is `zlo serve` running there?",
                ran.code
            )
        } else {
            line
        };
        return Err(SshError::PairFailed {
            host: target.host.clone(),
            line,
        });
    }
    Err(SshError::NotInstalled(target.host.clone()))
}

/// Run the system `ssh` to completion (stdin closed: nothing can prompt).
pub fn run_ssh(args: &[String]) -> std::io::Result<Ran> {
    let out = Command::new(SSH).args(args).stdin(Stdio::null()).output()?;
    Ok(Ran {
        code: out.status.code(),
        stdout: out.stdout,
        stderr: out.stderr,
    })
}

/// A loopback port nothing listens on right now.
pub fn free_local_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
    Ok(listener.local_addr()?.port())
}

/// Start a tunnel process (stderr piped, read by [`wait_ready`]).
pub fn spawn_tunnel(
    target: &SshTarget,
    local_port: u16,
    remote_port: u16,
) -> std::io::Result<Child> {
    Command::new(SSH)
        .args(tunnel_args(target, local_port, remote_port))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
}

/// Wait until the tunnel listens on `local_port`. A tunnel that exits or
/// does not listen in time is killed and reported by what `ssh` printed.
pub fn wait_ready(
    child: &mut Child,
    target: &SshTarget,
    local_port: u16,
    timeout: Duration,
) -> Result<(), SshError> {
    let deadline = Instant::now() + timeout;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, local_port));
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                let mut stderr = Vec::new();
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_end(&mut stderr);
                }
                tracing::debug!(stderr = %last_line(&stderr), "ssh tunnel ended early");
                return Err(classify_ssh_failure(target, &stderr)
                    .unwrap_or_else(|| SshError::TunnelFailed(target.host.clone())));
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e, "ssh tunnel state unknown");
                let _ = child.kill();
                return Err(SshError::TunnelFailed(target.host.clone()));
            }
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(SshError::TunnelFailed(target.host.clone()));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// The origin the client uses through a tunnel on `local_port`.
pub fn tunnel_origin(local_port: u16) -> String {
    format!("http://127.0.0.1:{local_port}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(raw: &str) -> SshTarget {
        parse_target(raw).unwrap()
    }

    #[test]
    fn targets_parse_and_anything_shell_like_is_refused() {
        assert_eq!(
            target(" me@Studio.local "),
            SshTarget {
                user: Some("me".into()),
                host: "studio.local".into(),
                port: 22
            }
        );
        assert_eq!(target("me@box:2222").port, 2222);
        assert_eq!(target("box").user, None);
        assert_eq!(target("me@[::1]:2200").host, "::1");
        assert_eq!(target("me@[::1]:2200").to_url(), "ssh://me@[::1]:2200");
        assert_eq!(
            target("first.last@10.0.0.4").destination(),
            "first.last@10.0.0.4"
        );
        for bad in [
            "",
            "me@",
            "@host",
            "-oProxyCommand=x",
            "me@-oProxyCommand=x",
            "-me@host",
            "me@host:0",
            "me@host:99999",
            "me@host:22x",
            "me@ho st",
            "me @host",
            "me@host;rm",
            "me@host&&x",
            "me@$(id)",
            "me@`id`",
            "me@host|x",
            "me'@host",
            "me\"@host",
            "me@host>x",
            "me@ho\nst",
            "me@[nothex]:22",
            "me@.hidden",
            "a@b@c",
        ] {
            assert_eq!(parse_target(bad), Err(SshError::BadTarget), "{bad:?}");
        }
        let t = target("me@box:2222");
        assert_eq!(SshTarget::from_url(&t.to_url()), Ok(t));
        assert_eq!(SshTarget::from_url("https://box"), Err(SshError::BadTarget));
    }

    #[test]
    fn the_pair_invocation_is_a_list_with_the_destination_after_the_options() {
        let args = pair_args(&target("me@box:2222"), REMOTE_PAIR_COMMANDS[0]);
        assert_eq!(
            args,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-a",
                "-x",
                "-p",
                "2222",
                "-T",
                "--",
                "me@box",
                "zlo pair --json",
            ]
        );
    }

    #[test]
    fn the_tunnel_forwards_loopback_to_loopback_and_exits_on_failure() {
        let args = tunnel_args(&target("me@box"), 49152, 3005);
        assert_eq!(
            args,
            [
                "-N",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-a",
                "-x",
                "-p",
                "22",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-L",
                "127.0.0.1:49152:127.0.0.1:3005",
                "--",
                "me@box",
            ]
        );
        assert_eq!(tunnel_origin(49152), "http://127.0.0.1:49152");
    }

    #[test]
    fn pair_json_is_read_past_a_banner() {
        let out = b"Welcome to Studio\n{\"code\":\"7kq2-m9xd-4tra\",\"expires_at\":\"2026-11-08T21:05:00Z\",\"link\":\"http://127.0.0.1:3005/pair#code=7KQ2-M9XD-4TRA\"}\n";
        let code = parse_pair_output(out).unwrap();
        assert_eq!(code.code, "7KQ2-M9XD-4TRA");
        assert_eq!(code.backend_port, Some(3005));
        assert!(!format!("{code:?}").contains("7KQ2"));

        let tailnet = br#"{"code":"7KQ2M9XD4TRA","expires_at":"2026-11-08T21:05:00Z","link":"https://studio.tail9915dd.ts.net/pair#code=7KQ2-M9XD-4TRA"}"#;
        assert_eq!(parse_pair_output(tailnet).unwrap().backend_port, None);
        assert!(parse_pair_output(b"not json").is_none());
        assert!(parse_pair_output(br#"{"code":"short","expires_at":"x"}"#).is_none());
    }

    #[test]
    fn pair_json_is_read_as_the_cli_prints_it() {
        // `zlo pair --json` is `serde_json::to_string_pretty` of
        // `{code, expires_at, link}`: one key per line.
        let printed = serde_json::to_string_pretty(&serde_json::json!({
            "code": "7KQ2-M9XD-4TRA",
            "expires_at": "2026-11-08T21:05:00Z",
            "link": "http://127.0.0.1:3005/pair#code=7KQ2-M9XD-4TRA",
        }))
        .unwrap();
        let out = format!("Last login: Sun Nov  8 on ttys001\n{{ banner braces }}\n{printed}\n");
        let code = parse_pair_output(out.as_bytes()).unwrap();
        assert_eq!(code.code, "7KQ2-M9XD-4TRA");
        assert_eq!(code.backend_port, Some(3005));
    }

    fn ran(code: i32, stdout: &str, stderr: &str) -> Ran {
        Ran {
            code: Some(code),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    const PAIR: &str = r#"{"code":"7KQ2-M9XD-4TRA","expires_at":"2026-11-08T21:05:00Z","link":"http://127.0.0.1:3000/pair#code=7KQ2-M9XD-4TRA"}"#;

    #[test]
    fn the_installed_binary_is_tried_when_zlo_is_not_on_path() {
        let t = target("me@box");
        let mut calls = Vec::new();
        let code = remote_pair_code(&t, |args| {
            calls.push(args.last().cloned().unwrap_or_default());
            Ok(if calls.len() == 1 {
                ran(127, "", "zsh:1: command not found: zlo")
            } else {
                ran(0, PAIR, "")
            })
        })
        .unwrap();
        assert_eq!(code.code, "7KQ2-M9XD-4TRA");
        assert_eq!(calls, REMOTE_PAIR_COMMANDS[..2]);
    }

    #[test]
    fn an_older_remote_still_pairs_through_its_ironclaw_command() {
        let t = target("me@box");
        let mut calls = Vec::new();
        let code = remote_pair_code(&t, |args| {
            calls.push(args.last().cloned().unwrap_or_default());
            Ok(if calls.len() < 4 {
                ran(127, "", "zsh:1: command not found")
            } else {
                ran(0, PAIR, "")
            })
        })
        .unwrap();
        assert_eq!(code.code, "7KQ2-M9XD-4TRA");
        assert_eq!(calls, REMOTE_PAIR_COMMANDS);
        assert_eq!(calls[3], "~/.ironclaw/bin/ironclaw pair --json");

        let err = remote_pair_code(&t, |_| Ok(ran(127, "", "command not found"))).unwrap_err();
        assert_eq!(err, SshError::NotInstalled("box".into()));
        assert_eq!(err.to_string(), "IronClaw isn't installed on box");
    }

    #[test]
    fn ssh_failures_are_named_in_plain_words() {
        let t = target("me@box");
        let err = remote_pair_code(&t, |_| {
            Ok(ran(
                255,
                "",
                "me@box: Permission denied (publickey,password).",
            ))
        })
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Set up an SSH key for me@box first (ZLO never asks for a password)."
        );
        let err = remote_pair_code(&t, |_| Ok(ran(255, "", "Host key verification failed.")))
            .unwrap_err();
        assert!(matches!(err, SshError::UnknownHostKey { .. }), "{err:?}");
        let err = remote_pair_code(&t, |_| {
            Ok(ran(
                255,
                "",
                "ssh: Could not resolve hostname box: nodename nor servname",
            ))
        })
        .unwrap_err();
        assert_eq!(err, SshError::Unreachable("box".into()));
        let err =
            remote_pair_code(&t, |_| Ok(ran(1, "", "Error: backend not reachable"))).unwrap_err();
        assert_eq!(
            err.to_string(),
            "IronClaw on box didn't make a pairing code: Error: backend not reachable"
        );
        let err = remote_pair_code(&t, |_| Err(std::io::Error::other("no ssh"))).unwrap_err();
        assert!(matches!(err, SshError::NoSsh(_)));
        assert_eq!(
            SshError::TunnelFailed("box".into()).to_string(),
            "Couldn't open a tunnel to box"
        );
    }

    #[test]
    fn a_free_port_is_on_loopback() {
        let port = free_local_port().unwrap();
        assert_ne!(port, 0);
        TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).unwrap();
    }
}
