use std::time::Duration;

use wasmtime::{Config, Engine};

pub use ironclaw_wasm_limiter::WasmResourceLimiter;

/// Epoch tick interval used as the backup timeout mechanism.
pub const EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(500);

const DEFAULT_MEMORY_BYTES: u64 = 10 * 1024 * 1024;
const DEFAULT_FUEL: u64 = 500_000_000;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct SandboxLimits {
    pub memory_bytes: u64,
    pub fuel: u64,
    pub timeout: Duration,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        Self {
            memory_bytes: DEFAULT_MEMORY_BYTES,
            fuel: DEFAULT_FUEL,
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl SandboxLimits {
    pub fn with_memory_bytes(mut self, memory_bytes: u64) -> Self {
        self.memory_bytes = memory_bytes;
        self
    }

    pub fn with_fuel(mut self, fuel: u64) -> Self {
        self.fuel = fuel;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("failed to create WASM engine: {0}")]
    EngineCreationFailed(String),
}

pub fn component_engine(thread_name: impl Into<String>) -> Result<Engine, SandboxError> {
    let mut config = Config::new();
    configure_component_engine(&mut config);
    let engine = Engine::new(&config)
        .map_err(|error| SandboxError::EngineCreationFailed(error.to_string()))?;
    spawn_epoch_ticker(&engine, thread_name)?;
    Ok(engine)
}

fn configure_component_engine(config: &mut Config) {
    config.consume_fuel(true);
    config.epoch_interruption(true);
    config.wasm_component_model(true);
    config.wasm_threads(false);
    config.debug_info(false);
}

/// Spawn the per-engine epoch ticker.
///
/// The ticker holds a `Weak<Engine>` reference rather than a strong clone so
/// that, once every owned `Engine` clone has been dropped by the host, the
/// ticker observes `upgrade() == None`, exits, and the thread joins. Without
/// this, every `component_engine` call would leak both a thread and an
/// `Engine` clone for the lifetime of the process — a real problem for tests
/// and for long-running hosts that rebuild runtimes (config reloads,
/// per-installation runtimes, etc.).
fn spawn_epoch_ticker(engine: &Engine, thread_name: impl Into<String>) -> Result<(), SandboxError> {
    let weak = engine.weak();
    std::thread::Builder::new()
        .name(thread_name.into())
        .spawn(move || {
            loop {
                std::thread::sleep(EPOCH_TICK_INTERVAL);
                match weak.upgrade() {
                    Some(engine) => engine.increment_epoch(),
                    None => break,
                }
            }
        })
        .map(|_| ())
        .map_err(|error| SandboxError::EngineCreationFailed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use ironclaw_wasm_limiter::WasmResourceLimiter;
    use wasmtime::ResourceLimiter;

    #[test]
    fn limiter_tracks_aggregate_growth_across_memories() {
        let mut limiter = WasmResourceLimiter::new(128 * 1024);
        assert!(limiter.memory_growing(0, 64 * 1024, None).unwrap());
        assert!(limiter.memory_growing(0, 64 * 1024, None).unwrap());
        assert!(!limiter.memory_growing(0, 64 * 1024, None).unwrap());
    }

    #[test]
    fn limiter_allows_component_model_internal_resources() {
        let limiter = WasmResourceLimiter::new(1024);
        assert_eq!(limiter.instances(), 10);
        assert_eq!(limiter.tables(), 10);
        assert_eq!(limiter.memories(), 10);
    }

    #[test]
    fn limiter_reexport_preserves_bookkeeping_accessors() {
        let mut limiter = WasmResourceLimiter::new(128 * 1024);
        assert_eq!(limiter.memory_limit(), 128 * 1024);
        assert_eq!(limiter.memory_used(), 0);

        assert!(limiter.memory_growing(0, 32 * 1024, None).unwrap());
        assert_eq!(limiter.memory_used(), 32 * 1024);
    }

    /// Regression: the epoch ticker thread must observe `Weak::upgrade() ==
    /// None` and exit after every `Engine` clone is dropped. Without this,
    /// each `component_engine` call would permanently leak a thread + an
    /// `Engine` clone, which is visible immediately in test runs and matters
    /// even more in long-running hosts that rebuild runtimes on config
    /// reload or per-installation.
    #[test]
    fn epoch_ticker_exits_when_engine_is_dropped() {
        use super::{EPOCH_TICK_INTERVAL, component_engine};
        use std::thread;
        use std::time::Duration;

        let engine =
            component_engine("sandbox-core-ticker-drop-test").expect("engine should construct");
        // Take a weak handle so we can observe when every owned `Engine`
        // clone has been dropped. We piggy-back on wasmtime's own
        // `Engine::weak()` for this.
        let engine_weak = engine.weak();
        // Drop the caller's strong reference. If the ticker is still
        // holding a clone (the pre-fix bug), `upgrade()` will keep
        // succeeding forever.
        drop(engine);

        // The ticker wakes at most once per EPOCH_TICK_INTERVAL. Give it a
        // few intervals of slack for scheduler jitter before declaring the
        // shutdown broken.
        let mut released = false;
        for _ in 0..10 {
            thread::sleep(EPOCH_TICK_INTERVAL + Duration::from_millis(100));
            if engine_weak.upgrade().is_none() {
                released = true;
                break;
            }
        }
        assert!(
            released,
            "epoch ticker still holding an Engine clone after >10 tick intervals; the Weak<Engine> shutdown path is broken"
        );
    }
}
