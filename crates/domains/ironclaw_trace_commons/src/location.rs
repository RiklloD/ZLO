//! Where Trace Commons keeps its local state (queue, policy, device keys,
//! credentials, submissions): `<base>/trace_contributions/`.
//!
//! The base defaults to `crate::location::base_dir()`
//! (`IRONCLAW_BASE_DIR`, else `~/.ironclaw`). A host that owns a data folder
//! sets it once at process start with [`set_base_dir`] — the `zlo` binary sets
//! its resolved `ZLO_HOME`, so a ZLO process never writes outside its home.
//! Every default-path helper in this crate resolves through [`base_dir`];
//! the `*_at` variants still take an explicit base for tests and tools.
//!
//! Test processes must never fall through to the developer's real base: a
//! real `~/.ironclaw/trace_contributions/policy.json` (instance enrollment)
//! flips "not enrolled" assertions and test writes land in live user state.
//! This crate's own tests bind an isolated per-process base automatically;
//! downstream test suites call [`bind_isolated_base_dir_for_tests`]
//! (`test-support` feature) from their shared test helpers.

use std::path::PathBuf;
use std::sync::OnceLock;

static BASE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Set the base directory for this process. First call wins; returns
/// `Err(existing)` when a different base was already set, so a host never
/// silently splits its state across two folders.
pub fn set_base_dir(base: PathBuf) -> Result<(), PathBuf> {
    let current = BASE_DIR.get_or_init(|| base.clone());
    if *current == base {
        Ok(())
    } else {
        Err(current.clone())
    }
}

/// The base directory default-path helpers use.
pub fn base_dir() -> PathBuf {
    #[cfg(test)]
    #[allow(clippy::expect_used)] // cfg(test) only: never fall back to the real home.
    let fallback = || {
        bind_isolated_base_dir_for_tests().expect("bind isolated Trace Commons base dir for tests") // safety: cfg(test) only
    };
    #[cfg(not(test))]
    let fallback = ironclaw_common::paths::ironclaw_base_dir;
    BASE_DIR.get().cloned().unwrap_or_else(fallback)
}

/// Bind this process's Trace Commons base to a fresh, process-private
/// temporary directory, so test code that reaches the default-path helpers
/// (directly or through production callers such as host-runtime dispatch,
/// turn-runner capture, or product views) can neither read the developer's
/// real enrollment nor write into it.
///
/// Idempotent: every call in a process returns the same directory. When a
/// base was already bound (e.g. a test drove a host entry point against a
/// temp home) that base is kept and returned — unless it is the real default
/// base, which is an error.
#[cfg(any(test, feature = "test-support"))]
pub fn bind_isolated_base_dir_for_tests() -> std::io::Result<PathBuf> {
    static ISOLATED: OnceLock<PathBuf> = OnceLock::new();
    let isolated = ISOLATED.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "ironclaw-trace-commons-test-{}-{nanos}",
            std::process::id()
        ))
    });
    std::fs::create_dir_all(isolated)?;
    match set_base_dir(isolated.clone()) {
        Ok(()) => Ok(isolated.clone()),
        Err(existing) if existing == ironclaw_common::paths::compute_ironclaw_base_dir() => {
            Err(std::io::Error::other(format!(
                "Trace Commons base is already bound to the real default base {}; \
                 tests must not touch live user state",
                existing.display()
            )))
        }
        Err(existing) => Ok(existing),
    }
}

/// Regression probe for real-home leaks: re-runs one test in a child copy of
/// the current test binary with `HOME` pointing at a throwaway home whose
/// *default* base (`$HOME/.ironclaw`) carries an ENABLED instance enrollment —
/// the exact state of a developer who opted into Trace Commons. The child
/// test asserts "not enrolled" behavior; it can only pass if the code under
/// test ignores the default base. Afterwards the probe also checks the child
/// wrote nothing into that base.
///
/// Env changes go only to the child process, so this is safe under parallel
/// tests.
#[cfg(any(test, feature = "test-support"))]
pub mod leak_probe {
    use std::path::{Path, PathBuf};

    const CHILD_ENV: &str = "IRONCLAW_TRACE_COMMONS_LEAK_PROBE_CHILD";

    /// True inside the probe's child process. Probe tests return early
    /// (trivially passing) when this is false and they are not the parent.
    pub fn is_child() -> bool {
        std::env::var_os(CHILD_ENV).is_some()
    }

    /// Run `test_path` (the libtest path, e.g. `module::tests::name`) in a
    /// child process against an enrolled fake home. Returns an error that
    /// names the failure when the child fails or writes into the fake home.
    pub fn run_child_with_enrolled_fake_home(test_path: &str) -> Result<(), String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let home = std::env::temp_dir().join(format!(
            "ironclaw-trace-leak-probe-{}-{nanos}",
            std::process::id()
        ));
        let result = run_in(&home, test_path);
        // Best-effort cleanup of a throwaway temp home; the probe verdict
        // is `result`, not whether the temp dir could be removed.
        if let Err(error) = std::fs::remove_dir_all(&home) {
            eprintln!("leak probe: could not remove {}: {error}", home.display());
        }
        result
    }

    fn run_in(home: &Path, test_path: &str) -> Result<(), String> {
        let contributions = seed_enrolled_home(home)?;
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let output = std::process::Command::new(exe)
            .args([test_path, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .env("HOME", home)
            .env_remove("IRONCLAW_BASE_DIR")
            .env_remove("ZLO_HOME")
            .output()
            .map_err(|e| format!("spawn probe child: {e}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() || !stdout.contains("1 passed") {
            return Err(format!(
                "probe child `{test_path}` failed against an enrolled fake home \
                 (real-home Trace Commons state leaks into tests)\n\
                 --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
            ));
        }
        let mut written: Vec<PathBuf> = std::fs::read_dir(&contributions)
            .map_err(|e| format!("read {}: {e}", contributions.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.file_name().is_none_or(|name| name != "policy.json"))
            .collect();
        written.sort();
        if !written.is_empty() {
            return Err(format!(
                "probe child `{test_path}` wrote into the default base: {written:?}"
            ));
        }
        Ok(())
    }

    fn seed_enrolled_home(home: &Path) -> Result<PathBuf, String> {
        let contributions = home.join(".ironclaw").join("trace_contributions");
        std::fs::create_dir_all(&contributions)
            .map_err(|e| format!("create {}: {e}", contributions.display()))?;
        let policy = crate::contribution::StandingTraceContributionPolicy {
            enabled: true,
            ingestion_endpoint: Some("https://trace.example.com/v1/traces".to_string()),
            ..Default::default()
        };
        let body = serde_json::to_string_pretty(&policy).map_err(|e| e.to_string())?;
        std::fs::write(contributions.join("policy.json"), body)
            .map_err(|e| format!("seed policy: {e}"))?;
        Ok(contributions)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn default_base_ignores_enrolled_real_home() {
        if super::leak_probe::is_child() {
            assert_ne!(
                super::base_dir(),
                ironclaw_common::paths::compute_ironclaw_base_dir(),
                "test process resolved the real default base"
            );
            let scope = "trace-leak-probe-scope";
            assert!(
                crate::contribution::resolve_effective_capture_policy(Some(scope))
                    .expect("resolve capture policy")
                    .is_none(),
                "enrolled real-home instance policy leaked into a test process"
            );
            crate::contribution::write_trace_policy_for_scope(
                Some(scope),
                &crate::contribution::StandingTraceContributionPolicy::default(),
            )
            .expect("write scoped policy");
            return;
        }
        super::leak_probe::run_child_with_enrolled_fake_home(
            "location::tests::default_base_ignores_enrolled_real_home",
        )
        .unwrap_or_else(|failure| panic!("{failure}"));
    }
}
