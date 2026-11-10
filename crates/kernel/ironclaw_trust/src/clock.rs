//! Pluggable time source for trust-policy evaluation.
//!
//! The default [`SystemClock`] reads `chrono::Utc::now()`, which is what
//! production deployments want for accurate audit timestamps. Tests can
//! inject a deterministic clock via [`HostTrustPolicy::with_clock`]
//! (defined in `crate::policy`) so `evaluated_at` is reproducible across
//! runs.

use chrono::Utc;
use ironclaw_host_api::Timestamp;

/// Time source consumed by `HostTrustPolicy`.
pub trait Clock: Send + Sync {
    fn now(&self) -> Timestamp;
}

/// Default production clock — reads the system wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Utc::now()
    }
}

/// Test fixture: a clock that returns a fixed timestamp on every call.
#[cfg(test)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct FixedClock {
    pub instant: chrono::DateTime<Utc>,
}

#[cfg(test)]
impl FixedClock {
    pub(crate) fn new(instant: chrono::DateTime<Utc>) -> Self {
        Self { instant }
    }
}

#[cfg(test)]
impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.instant
    }
}
