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
//! In any build with `cfg(test)` or the `test-support` feature, an unset base
//! resolves to an isolated per-process temp base instead of the default, so
//! no test anywhere in the workspace can reach the real one — every crate
//! whose tests reach Trace Commons enables `ironclaw_trace_commons/test-support`
//! as a dev-dependency. Shipping builds never enable `test-support`, and the
//! `zlo` binary binds its home with [`set_base_dir`] before any resolution.

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
///
/// Test builds (`cfg(test)` or `test-support`) never fall back to the real
/// default: an unset base binds the process's isolated temp base, so even a
/// test that resolves a default path before any shared helper ran stays off
/// the developer's live state.
pub fn base_dir() -> PathBuf {
    if let Some(base) = BASE_DIR.get() {
        return base.clone();
    }
    #[cfg(any(test, feature = "test-support"))]
    {
        // Without a private dir there is no safe base; fail the test loudly
        // rather than fall back anywhere predictable.
        #[allow(clippy::expect_used)] // test builds only (cfg(test) / test-support)
        bind_isolated_or_existing().expect("create a private Trace Commons test base") // safety: test builds only
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        ironclaw_common::paths::ironclaw_base_dir()
    }
}

/// Bind this process's Trace Commons base to a fresh private temp dir (see
/// [`private_test_dir`]), so test code that reaches the default-path helpers
/// (directly or through production callers such as host-runtime dispatch,
/// turn-runner capture, or product views) can neither read the developer's
/// real enrollment nor write into it.
///
/// [`base_dir`] does this on its own in test builds; call this explicitly
/// only to fail fast when a real-default base is already bound.
///
/// Idempotent: every call in a process returns the same directory. When a
/// base was already bound (e.g. a test drove a host entry point against a
/// temp home) that base is kept and returned — unless it is the real default
/// base, which is an error.
#[cfg(any(test, feature = "test-support"))]
pub fn bind_isolated_base_dir_for_tests() -> std::io::Result<PathBuf> {
    let bound = bind_isolated_or_existing()?;
    if bound == ironclaw_common::paths::compute_ironclaw_base_dir() {
        return Err(std::io::Error::other(format!(
            "Trace Commons base is already bound to the real default base {}; \
             tests must not touch live user state",
            bound.display()
        )));
    }
    Ok(bound)
}

/// A fresh private directory for a test Trace Commons base, created with
/// `mkdtemp` semantics (random name, owner-only, never reused, no symlink
/// followed) and kept for the rest of the process — pass it to
/// [`set_base_dir`] from a process-wide fixture.
///
/// It is deliberately not removed: statics never drop, and a shared sweep
/// root is its own attack surface. Each test process leaves one small
/// `ironclaw-trace-commons-test-*` dir in the system temp dir, which the OS
/// temp cleaner reclaims.
#[cfg(any(test, feature = "test-support"))]
pub fn private_test_dir() -> std::io::Result<PathBuf> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("ironclaw-trace-commons-test-");
    // `tempdir()` alone honours the umask (often 0755); create it owner-only.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    Ok(builder.tempdir()?.keep())
}

/// The process's isolated base, created on first use, or whatever base is
/// already bound.
#[cfg(any(test, feature = "test-support"))]
fn bind_isolated_or_existing() -> std::io::Result<PathBuf> {
    static ISOLATED: OnceLock<PathBuf> = OnceLock::new();
    if let Some(bound) = BASE_DIR.get() {
        return Ok(bound.clone());
    }
    let isolated = match ISOLATED.get() {
        Some(isolated) => isolated.clone(),
        None => {
            let created = private_test_dir()?;
            ISOLATED.get_or_init(|| created).clone()
        }
    };
    Ok(match set_base_dir(isolated.clone()) {
        Ok(()) => isolated,
        Err(existing) => existing,
    })
}

/// Regression probe for real-home leaks: re-runs one test in a child copy of
/// the current test binary with `HOME` pointing at a throwaway home whose
/// *default* base (`$HOME/.ironclaw`) carries an ENABLED instance enrollment —
/// the exact state of a developer who opted into Trace Commons. The child
/// test asserts "not enrolled" behavior; it can only pass if the code under
/// test ignores the default base. Afterwards the probe also checks the child
/// wrote nothing into that base and left its `policy.json` byte-identical.
/// The child also gets `IRONCLAW_BASE_DIR` pointed at that same base, so the
/// real-default fallback resolves there on every platform (on Windows `dirs`
/// reads the profile known folder, not `HOME`).
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
        let (contributions, seeded_policy) = seed_enrolled_home(home)?;
        let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let output = std::process::Command::new(exe)
            .args([test_path, "--exact", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, "1")
            .env("HOME", home)
            // `dirs` ignores HOME on Windows (profile known folder), so also
            // point the env override at the seeded base: a regressed fallback
            // then lands where this probe inspects on every platform.
            .env("IRONCLAW_BASE_DIR", home.join(".ironclaw"))
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
        let policy_path = contributions.join("policy.json");
        let policy_after = std::fs::read(&policy_path)
            .map_err(|e| format!("read {} after probe: {e}", policy_path.display()))?;
        if policy_after != seeded_policy {
            return Err(format!(
                "probe child `{test_path}` rewrote the default base's policy.json"
            ));
        }
        Ok(())
    }

    /// Seed the enrolled default base; returns its `trace_contributions/`
    /// dir and the exact `policy.json` bytes written.
    fn seed_enrolled_home(home: &Path) -> Result<(PathBuf, Vec<u8>), String> {
        let contributions = home.join(".ironclaw").join("trace_contributions");
        std::fs::create_dir_all(&contributions)
            .map_err(|e| format!("create {}: {e}", contributions.display()))?;
        let policy = crate::contribution::StandingTraceContributionPolicy {
            enabled: true,
            ingestion_endpoint: Some("https://trace.example.com/v1/traces".to_string()),
            ..Default::default()
        };
        let body = serde_json::to_string_pretty(&policy).map_err(|e| e.to_string())?;
        std::fs::write(contributions.join("policy.json"), &body)
            .map_err(|e| format!("seed policy: {e}"))?;
        Ok((contributions, body.into_bytes()))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn unset_base_binds_a_private_temp_dir() {
        let base = super::base_dir();
        assert_ne!(base, ironclaw_common::paths::compute_ironclaw_base_dir());
        assert_eq!(super::base_dir(), base, "binding is stable for the process");
        assert!(base.starts_with(std::env::temp_dir()), "{}", base.display());
        assert!(
            base.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("ironclaw-trace-commons-test-")),
            "{}",
            base.display()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&base)
                .expect("base exists")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "private base is owner-only");
        }
    }

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
