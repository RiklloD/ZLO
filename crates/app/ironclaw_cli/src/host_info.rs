//! Facts about the machine `ironclaw` runs on that remote access needs: the
//! default environment label, and whether other people have login accounts
//! here (they could reach the loopback identity port and forge the
//! `Tailscale-User-Login` header, so one-click pairing is off when they do).
//!
//! The parsers are pure and tested against fixtures; the `detect_*`
//! functions are the only code that runs host commands.

use std::process::Command;

/// The label a new environment gets: the macOS computer name, else the
/// short host name, else `IronClaw`.
pub(crate) fn default_environment_label() -> String {
    #[cfg(target_os = "macos")]
    if let Some(name) = command_line("scutil", &["--get", "ComputerName"]) {
        return name;
    }
    command_line("hostname", &["-s"])
        .or_else(|| command_line("hostname", &[]))
        .unwrap_or_else(|| "IronClaw".to_string())
}

/// Login accounts on this machine other than the one running `ironclaw`.
pub(crate) fn detect_other_login_accounts() -> anyhow::Result<Vec<String>> {
    let current_uid = command_line("id", &["-u"])
        .and_then(|uid| uid.parse::<u32>().ok())
        .ok_or_else(|| anyhow::anyhow!("could not read the current user id (`id -u`)"))?;
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("dscl")
            .args([".", "list", "/Users", "UniqueID"])
            .output()
            .map_err(|error| anyhow::anyhow!("could not list local accounts with dscl: {error}"))?;
        if !output.status.success() {
            anyhow::bail!("`dscl . list /Users UniqueID` failed");
        }
        Ok(other_macos_accounts(
            &String::from_utf8_lossy(&output.stdout),
            current_uid,
        ))
    }
    #[cfg(not(target_os = "macos"))]
    {
        // Accounts a directory service (LDAP, SSSD, NIS, winbind) supplies
        // cannot be listed reliably, so their presence alone means "other
        // people may log in here": fail closed.
        let nsswitch = std::fs::read_to_string("/etc/nsswitch.conf").unwrap_or_default(); // silent-ok: no nsswitch.conf means local files only
        if let Some(source) = directory_passwd_source(&nsswitch) {
            return Ok(vec![format!("(accounts from {source})")]);
        }
        // `getent` also sees accounts from other local NSS modules.
        let passwd = match command_line("getent", &["passwd"]) {
            Some(passwd) => passwd,
            None => std::fs::read_to_string("/etc/passwd")
                .map_err(|error| anyhow::anyhow!("could not read /etc/passwd: {error}"))?,
        };
        Ok(other_linux_accounts(&passwd, current_uid))
    }
}

/// The directory service `nsswitch.conf` names for `passwd`, if any.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn directory_passwd_source(nsswitch: &str) -> Option<String> {
    const DIRECTORY: [&str; 6] = ["ldap", "sss", "nis", "nisplus", "winbind", "hesiod"];
    nsswitch
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .find_map(|line| line.strip_prefix("passwd:"))
        .and_then(|sources| {
            sources
                .split_whitespace()
                .find(|source| DIRECTORY.contains(source))
                .map(str::to_string)
        })
}

/// `dscl . list /Users UniqueID` lines are `name   uid`. Real accounts have
/// uid >= 500; `_`-prefixed names are system daemons.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn other_macos_accounts(dscl_output: &str, current_uid: u32) -> Vec<String> {
    dscl_output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            let uid = parts.next()?.parse::<i64>().ok()?;
            (uid >= 500 && uid != i64::from(current_uid) && !name.starts_with('_'))
                .then(|| name.to_string())
        })
        .collect()
}

/// `passwd` accounts with a login shell, other than root and this user.
/// No uid floor: distributions differ on where people's ids start, and a
/// login shell is what makes an account able to reach the loopback port.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn other_linux_accounts(passwd: &str, current_uid: u32) -> Vec<String> {
    const NO_LOGIN: [&str; 5] = ["nologin", "false", "sync", "halt", "shutdown"];
    passwd
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() < 7 {
                return None;
            }
            let uid = fields[2].parse::<u32>().ok()?;
            let shell = fields[6].trim();
            let shell_name = shell.rsplit('/').next().unwrap_or(shell);
            let login_shell = !shell.is_empty() && !NO_LOGIN.contains(&shell_name);
            // 65534 is `nobody` on most distributions.
            (uid != 0 && uid != 65534 && uid != current_uid && login_shell)
                .then(|| fields[0].to_string())
        })
        .collect()
}

fn command_line(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_accounts_skip_daemons_system_ids_and_self() {
        let dscl = "_amavisd                 83\n_www                     70\ndaemon                   1\nnobody                   -2\nroot                     0\nlorik                    501\nguest                    502\n_hidden                  600\n";
        assert_eq!(other_macos_accounts(dscl, 501), vec!["guest".to_string()]);
        assert!(other_macos_accounts("lorik 501\n", 501).is_empty());
    }

    #[test]
    fn linux_accounts_need_a_login_shell_and_a_user_uid() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
             daemon:x:1:1:daemon:/usr/sbin:/usr/sbin/nologin\n\
             nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n\
             me:x:1000:1000::/home/me:/bin/bash\n\
             svc:x:1001:1001::/srv:/bin/false\n\
             alice:x:1002:1002::/home/alice:/usr/bin/zsh\n\
             # comment\n\
             broken-line\n";
        assert_eq!(
            other_linux_accounts(passwd, 1000),
            vec!["alice".to_string()]
        );
        // A person with a low uid still counts.
        let passwd = "me:x:1000:1000::/home/me:/bin/bash\nold:x:500:500::/home/old:/bin/sh\n";
        assert_eq!(other_linux_accounts(passwd, 1000), vec!["old".to_string()]);
    }

    #[test]
    fn directory_accounts_are_named_from_nsswitch() {
        assert_eq!(
            directory_passwd_source("passwd: files sss # sssd\ngroup: files sss\n"),
            Some("sss".to_string())
        );
        assert_eq!(
            directory_passwd_source("passwd:         files systemd\n"),
            None
        );
        assert_eq!(directory_passwd_source("# passwd: ldap\n"), None);
    }
}
