//! `zlo transfer`'s SSH invocation, built as an argument list with the
//! same rules as the app's SSH route (`tools/ironclaw_rune/client/src/ssh.rs`):
//! the system `/usr/bin/ssh`, `BatchMode=yes` (no password prompt, no
//! unknown host key accepted), no agent or X11 forwarding, a validated
//! `user@host[:port]` passed as one argument after `--`, and a remote
//! command that is fixed text here — nothing typed by the person reaches
//! a shell.
//!
//! The archive and the passphrase travel over the SSH session's stdin
//! (passphrase on the first line, then the archive), so neither lands in a
//! file or on a command line on the other computer.

/// The system client (never one found on `PATH`).
pub(crate) const SSH: &str = "/usr/bin/ssh";
const DEFAULT_SSH_PORT: u16 = 22;
const CONNECT_TIMEOUT_SECS: u32 = 15;

/// `user@host[:port]`, checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SshTarget {
    pub(crate) user: Option<String>,
    pub(crate) host: String,
    pub(crate) port: u16,
}

impl SshTarget {
    pub(crate) fn destination(&self) -> String {
        match &self.user {
            Some(user) => format!("{user}@{}", self.host),
            None => self.host.clone(),
        }
    }
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

/// `user@host[:port]` (IPv6 as `user@[::1]:22`). Anything a shell or `ssh`
/// could read as more than a destination is refused.
pub(crate) fn parse_target(raw: &str) -> anyhow::Result<SshTarget> {
    let bad = || anyhow::anyhow!("give the computer as user@host, or user@host:port");
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(char::is_whitespace) {
        return Err(bad());
    }
    let (user, rest) = match raw.rsplit_once('@') {
        Some((user, rest)) if user_ok(user) => (Some(user.to_string()), rest),
        Some(_) => return Err(bad()),
        None => (None, raw),
    };
    let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
        let (host, after) = v6.split_once(']').ok_or_else(bad)?;
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(bad());
        }
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or_else(bad)?),
        };
        (host, port)
    } else {
        match rest.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (rest, None),
        }
    };
    if !host.contains(':') && !host_ok(host) {
        return Err(bad());
    }
    let port = match port {
        None => DEFAULT_SSH_PORT,
        Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(bad)?,
    };
    Ok(SshTarget {
        user,
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// The remote command: `zlo import` from `PATH`, else where the installer
/// puts it, reading the passphrase line and the archive from stdin. Run
/// through `sh -c` so the user's login shell (fish, zsh, …) only sees
/// plain words and one single-quoted script.
pub(crate) fn remote_import_command(replace: bool) -> String {
    let mut command = String::from(
        "sh -c 'if command -v zlo >/dev/null 2>&1; then exec zlo \"$@\"; \
         else exec \"$HOME/.zlo/bin/zlo\" \"$@\"; fi' zlo-transfer import --passphrase-stdin",
    );
    if replace {
        command.push_str(" --replace");
    }
    command.push_str(" -");
    command
}

/// `ssh … -T -- user@host <remote import>`.
pub(crate) fn import_args(target: &SshTarget, replace: bool) -> Vec<String> {
    vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
        "-a".into(),
        "-x".into(),
        "-p".into(),
        target.port.to_string(),
        "-T".into(),
        "--".into(),
        target.destination(),
        remote_import_command(replace),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse_and_anything_shell_like_is_refused() {
        assert_eq!(
            parse_target("ana@Server.example").expect("ok"),
            SshTarget {
                user: Some("ana".into()),
                host: "server.example".into(),
                port: 22
            }
        );
        assert_eq!(parse_target("box:2222").expect("ok").port, 2222);
        assert_eq!(parse_target("ana@[::1]:22").expect("ok").host, "::1");
        for bad in [
            "",
            "-oProxyCommand=evil",
            "ana@-host",
            "ana@host;rm -rf ~",
            "ana@host$(id)",
            "a b@host",
            "ana@host:0",
            "ana@host:99999",
            "'ana'@host",
            "ana@[not-ip]",
            "ana@.host",
        ] {
            assert!(parse_target(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn the_import_invocation_is_a_fixed_list_with_the_destination_after_the_options() {
        let target = parse_target("ana@server:2200").expect("target");
        let args = import_args(&target, true);
        assert_eq!(
            args[..10],
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=15",
                "-a",
                "-x",
                "-p",
                "2200",
                "-T",
                "--"
            ]
        );
        assert_eq!(args[10], "ana@server");
        assert_eq!(
            args[11],
            "sh -c 'if command -v zlo >/dev/null 2>&1; then exec zlo \"$@\"; else exec \
             \"$HOME/.zlo/bin/zlo\" \"$@\"; fi' zlo-transfer import --passphrase-stdin --replace -"
        );
        assert_eq!(args.len(), 12);
        assert!(!import_args(&target, false)[11].contains("--replace"));
    }
}
