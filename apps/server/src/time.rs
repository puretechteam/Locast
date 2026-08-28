//! Server-side time source. Production uses [`SystemClock`];
//! tests inject [`MockClock`] for deterministic grace / stale
//! cleanup paths.

#![forbid(unsafe_code)]

use std::sync::Mutex;

/// A monotonic-aware time source that returns unix-ms. The
/// `SystemClock` implementation is infallible; the `MockClock`
/// is `Send + Sync` via a `Mutex` so it can sit behind an
/// `Arc<dyn Clock>` inside the same `AppState` as production
/// code.
pub trait Clock: Send + Sync {
    /// Current unix time in milliseconds.
    fn now_ms(&self) -> i64;
}

/// The production clock. Backed by `SystemTime`.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// A test clock. `set` advances the returned `now_ms` value
/// without sleeping the real wall clock. The test that
/// exercises the 200ms host-disconnect grace in P2-T04 uses
/// this to drive the registry's grace ticker deterministically
/// without `tokio::time::sleep`.
pub struct MockClock {
    inner: Mutex<i64>,
}

impl MockClock {
    /// Build a new mock clock that starts at `start_ms`.
    pub fn new(start_ms: i64) -> Self {
        Self {
            inner: Mutex::new(start_ms),
        }
    }

    /// Set the returned time to `ms`.
    pub fn set(&self, ms: i64) {
        let mut g = self.inner.lock().expect("mock clock poisoned");
        *g = ms;
    }

    /// Add `delta_ms` to the current time.
    pub fn advance(&self, delta_ms: i64) {
        let mut g = self.inner.lock().expect("mock clock poisoned");
        *g += delta_ms;
    }
}

impl Clock for MockClock {
    fn now_ms(&self) -> i64 {
        *self.inner.lock().expect("mock clock poisoned")
    }
}

impl Default for MockClock {
    fn default() -> Self {
        Self::new(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_returns_nonzero() {
        let c = SystemClock;
        let now = c.now_ms();
        assert!(now > 0);
    }

    #[test]
    fn mock_clock_starts_at_zero_by_default() {
        let c = MockClock::default();
        assert_eq!(c.now_ms(), 0);
    }

    #[test]
    fn mock_clock_set_and_advance() {
        let c = MockClock::new(1_000);
        assert_eq!(c.now_ms(), 1_000);
        c.set(5_000);
        assert_eq!(c.now_ms(), 5_000);
        c.advance(2_500);
        assert_eq!(c.now_ms(), 7_500);
    }
}
