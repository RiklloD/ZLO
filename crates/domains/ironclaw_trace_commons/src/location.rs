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
        bind_isolated_or_existing()
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        ironclaw_common::paths::ironclaw_base_dir()
    }
}

/// Bind this process's Trace Commons base to a process-private directory
/// under a shared, well-known parent in the system temp dir (see
/// [`isolated_test_root`]), so test code that reaches the default-path
/// helpers (directly or through production callers such as host-runtime
/// dispatch, turn-runner capture, or product views) can neither read the
/// developer's real enrollment nor write into it.
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
    let bound = bind_isolated_or_existing();
    if bound == ironclaw_common::paths::compute_ironclaw_base_dir() {
        return Err(std::io::Error::other(format!(
            "Trace Commons base is already bound to the real default base {}; \
             tests must not touch live user state",
            bound.display()
        )));
    }
    std::fs::create_dir_all(&bound)?;
    Ok(bound)
}

/// Parent of every test process's isolated base and owned fixture dir. One
/// fixed directory under the system temp dir, so dirs left by dead processes
/// are found and reclaimed instead of accumulating.
#[cfg(any(test, feature = "test-support"))]
pub fn isolated_test_root() -> PathBuf {
    std::env::temp_dir().join("ironclaw-trace-commons-tests")
}

/// Create a fresh directory under [`isolated_test_root`] that this process
/// owns for its whole lifetime, for test fixtures that need a process-wide
/// Trace Commons base (pass it to [`set_base_dir`]). Unlike a `TempDir` held
/// in a static (statics never drop), it is reclaimed by a later test process
/// once this one has exited.
#[cfg(any(test, feature = "test-support"))]
pub fn owned_test_dir() -> std::io::Result<PathBuf> {
    let root = isolated_test_root();
    static SWEPT: std::sync::Once = std::sync::Once::new();
    SWEPT.call_once(|| owned_dirs::sweep_abandoned(&root));
    owned_dirs::create_under(&root)
}

#[cfg(any(test, feature = "test-support"))]
fn bind_isolated_or_existing() -> PathBuf {
    static ISOLATED: OnceLock<PathBuf> = OnceLock::new();
    let isolated = ISOLATED.get_or_init(|| {
        owned_test_dir().unwrap_or_else(|error| {
            // Still a private temp path, never the real base; it is just not
            // reclaimable by the sweep.
            let fallback = isolated_test_root().join(format!("unowned-{}", std::process::id()));
            eprintln!(
                "trace commons tests: could not create an owned isolated base ({error}); \
                 using {}",
                fallback.display()
            );
            fallback
        })
    });
    match set_base_dir(isolated.clone()) {
        Ok(()) => isolated.clone(),
        Err(existing) => existing,
    }
}

/// Lock-based ownership of the dirs under [`isolated_test_root`].
///
/// Each owned dir is named `pid-<pid>-<nanos>-<seq>` and holds an `.owner`
/// file that its process locks exclusively (and stamps with its pid) for its
/// whole lifetime, through a static that never drops. The OS releases the
/// lock when the process dies, so the sweep deletes a dir only when it can
/// take that lock itself AND finds the stamp: the owner is gone. An empty
/// `.owner` means the owner is still between creating and stamping it, a
/// dir without `.owner` is not ours to judge, and names that do not match
/// the pattern are never touched.
#[cfg(any(test, feature = "test-support"))]
mod owned_dirs {
    use std::fs::{File, TryLockError};
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    const OWNER_FILE: &str = ".owner";

    /// Lock handles of every dir this process owns; never dropped.
    static OWNERS: Mutex<Vec<File>> = Mutex::new(Vec::new());

    pub(super) fn create_under(root: &Path) -> std::io::Result<PathBuf> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(root)?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = root.join(format!("pid-{}-{nanos}-{seq}", std::process::id()));
        std::fs::create_dir(&dir)?;
        let mut owner = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(OWNER_FILE))?;
        // Blocking: a sweeper that grabbed the fresh, still-empty file
        // releases it at once without deleting anything.
        owner.lock()?;
        write!(owner, "{}", std::process::id())?;
        owner.flush()?;
        OWNERS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(owner);
        Ok(dir)
    }

    pub(super) fn is_owned_dir_name(name: &str) -> bool {
        let Some(rest) = name.strip_prefix("pid-") else {
            return false;
        };
        let parts: Vec<&str> = rest.split('-').collect();
        parts.len() == 3
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    }

    /// Remove dirs under `root` whose owning process has exited. Best effort.
    pub(super) fn sweep_abandoned(root: &Path) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let is_candidate = entry.file_name().to_str().is_some_and(is_owned_dir_name)
                && entry.file_type().is_ok_and(|kind| kind.is_dir());
            if is_candidate
                && owner_is_gone(&entry.path().join(OWNER_FILE))
                && let Err(error) = std::fs::remove_dir_all(entry.path())
            {
                eprintln!(
                    "trace commons tests: could not reclaim {}: {error}",
                    entry.path().display()
                );
            }
        }
    }

    /// True only when this process can take `.owner`'s lock and it carries
    /// an owner stamp. The lock is released before returning; the dir name
    /// is unique to a process that is gone, so nothing re-takes it.
    fn owner_is_gone(owner_path: &Path) -> bool {
        let Ok(mut owner) = File::options().read(true).write(true).open(owner_path) else {
            return false;
        };
        match owner.try_lock() {
            Ok(()) => {
                let mut stamp = String::new();
                owner.read_to_string(&mut stamp).is_ok() && !stamp.trim().is_empty()
            }
            Err(TryLockError::WouldBlock | TryLockError::Error(_)) => false,
        }
    }
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
    fn sweep_reclaims_only_abandoned_owned_dirs() {
        let root = tempfile::tempdir().expect("sweep root");
        let make = |name: &str, stamp: Option<&str>| {
            let dir = root.path().join(name);
            std::fs::create_dir(&dir).expect("create dir");
            if let Some(stamp) = stamp {
                std::fs::write(dir.join(".owner"), stamp).expect("write owner");
            }
            dir
        };
        // Live: another holder keeps the lock; an ancient mtime must not matter.
        let held = make("pid-11-1-0", Some("11"));
        let held_lock = std::fs::File::options()
            .read(true)
            .write(true)
            .open(held.join(".owner"))
            .expect("open held owner");
        held_lock.lock().expect("hold owner lock");
        #[cfg(unix)]
        {
            let ancient = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1);
            std::fs::File::open(&held)
                .and_then(|dir| dir.set_modified(ancient))
                .expect("age held dir");
        }
        // Live through the real creator (lock kept in this process's static).
        let created = super::owned_dirs::create_under(root.path()).expect("owned dir");
        // Abandoned: stamped, nobody holds the lock.
        let abandoned = make("pid-22-2-0", Some("22"));
        // Not ours to judge: wrong name, or no / empty owner file.
        let unrelated = make("keep-me", Some("33"));
        let ownerless = make("pid-44-4-0", None);
        let unstamped = make("pid-55-5-0", Some(""));

        super::owned_dirs::sweep_abandoned(root.path());

        assert!(held.exists(), "a dir whose owner lock is held must survive");
        assert!(created.exists(), "this process's own dir must survive");
        assert!(
            !abandoned.exists(),
            "an abandoned owned dir must be reclaimed"
        );
        assert!(
            unrelated.exists(),
            "names outside the pattern are never touched"
        );
        assert!(ownerless.exists(), "a dir without .owner is not reclaimed");
        assert!(
            unstamped.exists(),
            "an unstamped .owner may be mid-creation"
        );
        drop(held_lock);
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
