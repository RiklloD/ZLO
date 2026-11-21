//! This machine's CPU and memory headroom, for load balancing new work
//! across linked environments. Best effort: a reading that fails reports
//! zeros, which [`EnvironmentResources::pressure`] treats as idle.

use ironclaw_product_contracts::peer::EnvironmentResources;

/// Read the current load and memory.
pub async fn probe() -> EnvironmentResources {
    tokio::task::spawn_blocking(read).await.unwrap_or_default()
}

fn cpu_count() -> u32 {
    std::thread::available_parallelism()
        .map(|n| u32::try_from(n.get()).unwrap_or(u32::MAX))
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn read() -> EnvironmentResources {
    let load_1m = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|raw| raw.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0);
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| -> u64 {
        meminfo
            .lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map_or(0, |kib| kib.saturating_mul(1024))
    };
    EnvironmentResources {
        cpu_count: cpu_count(),
        load_1m,
        memory_total_bytes: field("MemTotal:"),
        memory_available_bytes: field("MemAvailable:"),
        active_turns: 0,
    }
}

#[cfg(target_os = "macos")]
fn read() -> EnvironmentResources {
    fn sysctl(name: &str) -> Option<String> {
        let out = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", name])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
    // `{ 1.23 1.10 0.98 }`
    let load_1m = sysctl("vm.loadavg")
        .and_then(|raw| {
            raw.trim_matches(|c| c == '{' || c == '}' || c == ' ')
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(0.0);
    let total = sysctl("hw.memsize")
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(0);
    let available = vm_stat_available().unwrap_or(0).min(total);
    EnvironmentResources {
        cpu_count: cpu_count(),
        load_1m,
        memory_total_bytes: total,
        memory_available_bytes: available,
        active_turns: 0,
    }
}

/// Free + inactive + speculative + purgeable pages, as Activity Monitor
/// counts memory that can be handed out without pressure.
#[cfg(target_os = "macos")]
fn vm_stat_available() -> Option<u64> {
    let out = std::process::Command::new("/usr/bin/vm_stat")
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let page_size = text
        .lines()
        .next()
        .and_then(|line| {
            let start = line.find("page size of ")? + "page size of ".len();
            line[start..].split_whitespace().next()?.parse::<u64>().ok()
        })
        .unwrap_or(16_384);
    let pages = |name: &str| -> u64 {
        text.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| {
                line.rsplit(':')
                    .next()?
                    .trim()
                    .trim_end_matches('.')
                    .parse()
                    .ok()
            })
            .unwrap_or(0)
    };
    let free = pages("Pages free")
        + pages("Pages inactive")
        + pages("Pages speculative")
        + pages("Pages purgeable");
    Some(free.saturating_mul(page_size))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn read() -> EnvironmentResources {
    EnvironmentResources {
        cpu_count: cpu_count(),
        ..EnvironmentResources::default()
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_probe_reports_this_machines_cpus() {
        let r = super::probe().await;
        assert!(r.cpu_count > 0);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        assert!(r.memory_total_bytes > 0);
    }
}
