//! Local servers for the thread details card (t3code's `ThreadServersPanel`):
//! the TCP ports listening on this Mac whose process runs inside the
//! thread's project folder. t3code knows which terminal or agent started a
//! port; ZLO has no port registry, so the folder a process runs in stands
//! for the thread. Blocking: callers run [`discover`] with `jobs::blocking`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    pub port: u16,
    pub pid: u32,
    /// The process name, as `lsof` shows it.
    pub process: String,
}

fn lsof(args: &[&str]) -> Result<String, String> {
    let out = Command::new("/usr/sbin/lsof")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Couldn't run lsof: {e}"))?;
    // lsof exits 1 when nothing matched; its output is still the answer.
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The listening servers whose process runs in `folder` or below it, by port.
pub fn discover(folder: &Path) -> Result<Vec<Server>, String> {
    let listening = parse_listening(&lsof(&["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcn"])?);
    if listening.is_empty() {
        return Ok(Vec::new());
    }
    let pids = listening
        .iter()
        .map(|s| s.pid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let cwds = parse_cwds(&lsof(&["-a", "-p", &pids, "-d", "cwd", "-Fn"])?);
    Ok(listening
        .into_iter()
        .filter(|s| cwds.get(&s.pid).is_some_and(|cwd| cwd.starts_with(folder)))
        .collect())
}

/// `lsof -Fpcn` of listening sockets: one server per port, lowest first.
fn parse_listening(out: &str) -> Vec<Server> {
    let mut by_port = BTreeMap::new();
    let (mut pid, mut process) = (0u32, String::new());
    for line in out.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => pid = value.parse().unwrap_or(0),
            "c" => process = value.to_string(),
            "n" => {
                let port = value.rsplit(':').next().and_then(|p| p.parse::<u16>().ok());
                if let Some(port) = port.filter(|_| pid != 0) {
                    by_port.entry(port).or_insert_with(|| Server {
                        port,
                        pid,
                        process: process.clone(),
                    });
                }
            }
            _ => {}
        }
    }
    by_port.into_values().collect()
}

/// `lsof -d cwd -Fn`: each process's working folder.
fn parse_cwds(out: &str) -> HashMap<u32, PathBuf> {
    let mut cwds = HashMap::new();
    let mut pid = 0u32;
    for line in out.lines() {
        let (tag, value) = line.split_at(line.len().min(1));
        match tag {
            "p" => pid = value.parse().unwrap_or(0),
            "n" if pid != 0 => {
                cwds.insert(pid, PathBuf::from(value));
            }
            _ => {}
        }
    }
    cwds
}

/// Ask the server's process to end (t3code's "Stop server").
pub fn stop(pid: u32) -> Result<(), String> {
    let status = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .stdin(Stdio::null())
        .status()
        .map_err(|e| format!("Couldn't stop the server: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("Couldn't stop the server (process {pid})."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listening_sockets_read_one_server_per_port() {
        let out = "p501\nf20\ncnode\nf21\nn*:5173\nf22\nn[::1]:5173\np77\ncpython3\nf3\nn127.0.0.1:8000\np9\nccupsd\nf5\nnlocalhost:631\n";
        assert_eq!(
            parse_listening(out),
            vec![
                Server {
                    port: 631,
                    pid: 9,
                    process: "cupsd".into()
                },
                Server {
                    port: 5173,
                    pid: 501,
                    process: "node".into()
                },
                Server {
                    port: 8000,
                    pid: 77,
                    process: "python3".into()
                },
            ]
        );
    }

    #[test]
    fn working_folders_read_by_process() {
        let out = "p501\nfcwd\nn/Users/you/code/app\np77\nfcwd\nn/tmp\n";
        let cwds = parse_cwds(out);
        assert_eq!(cwds[&501], PathBuf::from("/Users/you/code/app"));
        assert_eq!(cwds[&77], PathBuf::from("/tmp"));
    }
}
