//! The SSH tunnels ZLO keeps open: to the computer in use when it is
//! reached over its SSH route (`zlo_client::ssh`), plus, while a switch or
//! a pairing is in flight, the candidate's. Opened on demand, restarted
//! with backoff if `ssh` exits while wanted. Opening a candidate never
//! closes the tunnel in use: [`retain`] closes the others once the switch
//! succeeds or fails, and quitting closes all.
//!
//! Like `service.rs`, a supervisor thread owns the `Child` and blocks in
//! `wait` on it; [`close`] signals it by pid. The local port stays the
//! same across restarts, so the open connection's address stays valid.

use std::io::Read;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zlo_client::ssh::{self, SshError, SshTarget};

const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A run this long resets the restart backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

struct Open {
    key: String,
    local_port: u16,
    stop: Arc<AtomicBool>,
    pid: Arc<AtomicU32>,
}

static TUNNELS: Mutex<Vec<Open>> = Mutex::new(Vec::new());
/// Opens one at a time (an open blocks while `ssh` connects; [`close`]
/// never waits for it).
static OPENING: Mutex<()> = Mutex::new(());

/// Which tunnel: the SSH target and the backend port it forwards to.
pub fn key(target: &SshTarget, remote_port: u16) -> String {
    format!("{} -> {remote_port}", target.to_url())
}

/// The local port of a tunnel to `target`'s `remote_port`: the open one,
/// or a new one once it listens. Blocking (up to
/// [`ssh::TUNNEL_READY_TIMEOUT`]): call it off the UI thread.
pub fn open(target: &SshTarget, remote_port: u16) -> Result<u16, SshError> {
    // Unit tests never run a real ssh.
    if cfg!(test) {
        return Err(SshError::TunnelFailed(target.host.clone()));
    }
    let _opening = OPENING.lock().unwrap_or_else(|e| e.into_inner());
    let key = key(target, remote_port);
    if let Some(port) = current_port(&key) {
        return Ok(port);
    }
    // A stopped tunnel under this key goes; others (the one in use) stay.
    drop_stopped(&key);
    let local_port =
        ssh::free_local_port().map_err(|_| SshError::TunnelFailed(target.host.clone()))?;
    let mut child = ssh::spawn_tunnel(target, local_port, remote_port)
        .map_err(|e| SshError::NoSsh(e.to_string()))?;
    ssh::wait_ready(&mut child, target, local_port, ssh::TUNNEL_READY_TIMEOUT)?;
    let stop = Arc::new(AtomicBool::new(false));
    let pid = Arc::new(AtomicU32::new(child.id()));
    tracing::debug!(tunnel = %key, local_port, "ssh tunnel open");
    let (sup_stop, sup_pid, sup_target) = (stop.clone(), pid.clone(), target.clone());
    let spawned = std::thread::Builder::new()
        .name("zlo-ssh-tunnel".into())
        .spawn(move || {
            supervise(
                child,
                &sup_target,
                local_port,
                remote_port,
                &sup_stop,
                &sup_pid,
            )
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "ssh tunnel supervisor did not start");
        terminate(pid.load(Ordering::SeqCst));
        return Err(SshError::TunnelFailed(target.host.clone()));
    }
    TUNNELS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Open {
            key,
            local_port,
            stop,
            pid,
        });
    Ok(local_port)
}

fn current_port(key: &str) -> Option<u16> {
    let guard = TUNNELS.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .iter()
        .find(|o| o.key == key && !o.stop.load(Ordering::SeqCst))
        .map(|o| o.local_port)
}

fn drop_stopped(key: &str) {
    TUNNELS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|o| o.key != key);
}

fn stop(open: &Open) {
    open.stop.store(true, Ordering::SeqCst);
    terminate(open.pid.load(Ordering::SeqCst));
    tracing::debug!(tunnel = %open.key, "ssh tunnel closed");
}

/// Stop every tunnel (quitting). Never waits.
pub fn close() {
    retain(None);
}

/// Keep only the tunnel `keep` names (the one the connection in use goes
/// through); `None` closes all. Called once a switch or pairing settles.
pub fn retain(keep: Option<&str>) {
    let gone: Vec<Open> = {
        let mut tunnels = TUNNELS.lock().unwrap_or_else(|e| e.into_inner());
        let (kept, gone) = std::mem::take(&mut *tunnels)
            .into_iter()
            .partition(|o| Some(o.key.as_str()) == keep);
        *tunnels = kept;
        gone
    };
    gone.iter().for_each(stop);
}

fn terminate(pid: u32) {
    if pid == 0 {
        return;
    }
    let _ = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .status();
}

/// `ssh` writes little, but a full pipe would stall it: read it out.
fn drain(child: &mut Child) {
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::Builder::new()
            .name("zlo-ssh-stderr".into())
            .spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(n) = stderr.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    tracing::debug!(line = %String::from_utf8_lossy(&buf[..n]).trim(), "ssh tunnel");
                }
            })
            .ok();
    }
}

fn supervise(
    mut child: Child,
    target: &SshTarget,
    local_port: u16,
    remote_port: u16,
    stop: &AtomicBool,
    pid: &AtomicU32,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = Instant::now();
        drain(&mut child);
        let status = child.wait().ok();
        pid.store(0, Ordering::SeqCst);
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if started.elapsed() > HEALTHY_RUN {
            backoff = Duration::from_secs(1);
        }
        tracing::debug!(?status, retry_in = ?backoff, "ssh tunnel exited; restarting");
        // Sleep in steps, so a close during the wait is seen soon.
        let until = Instant::now() + backoff;
        while Instant::now() < until {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            match ssh::spawn_tunnel(target, local_port, remote_port) {
                Ok(next) => {
                    pid.store(next.id(), Ordering::SeqCst);
                    // A close that landed between the check and the store
                    // still stops this one.
                    if stop.load(Ordering::SeqCst) {
                        terminate(next.id());
                    }
                    child = next;
                    break;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "ssh tunnel did not restart");
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tests_never_run_ssh_and_closing_nothing_is_fine() {
        let target = ssh::parse_target("me@box").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            open(&target, 3000),
            Err(SshError::TunnelFailed("box".into()))
        );
        retain(None);
        close();
        assert_eq!(key(&target, 3005), "ssh://me@box:22 -> 3005");
    }

    fn fake(key: &str, local_port: u16) -> Open {
        Open {
            key: key.into(),
            local_port,
            stop: Arc::new(AtomicBool::new(false)),
            pid: Arc::new(AtomicU32::new(0)),
        }
    }

    #[test]
    fn a_candidate_tunnel_never_closes_the_one_in_use() {
        let _serial = OPENING.lock().unwrap_or_else(|e| e.into_inner());
        // In use: A. A switch to B opens B's tunnel beside it.
        TUNNELS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend([fake("a", 40001), fake("b", 40002)]);
        assert_eq!(current_port("a"), Some(40001));
        // B failed: the switch path keeps A's tunnel, and only A's.
        retain(Some("a"));
        assert_eq!(current_port("a"), Some(40001));
        assert_eq!(current_port("b"), None);
        // Quitting closes everything.
        close();
        assert_eq!(current_port("a"), None);
    }
}
