//! `net::reconnect` - the 1s -> 30s exponential backoff with
//! +/-20% jitter, per architecture section 22.3.1.
//!
//! The schedule is fixed at `[1, 2, 4, 8, 16, 30, 30]`. The 6th
//! and subsequent attempts both map to 30s. The schedule is
//! applied without per-error tuning; the only exception is an
//! explicit `skip_to_cap` used after AUTH_FAIL (banned / bad
//! signature) so the client does not hammer a server that has
//! just rejected it.
//!
//! The jitter is uniformly random in `[-20%, +20%]` of the
//! base value. The implementation is deterministic when a
//! caller provides a seeded `SmallRng` (e.g. in tests); the
//! default constructor uses `OsRng`.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::time::Duration;

use rand::rngs::{OsRng, SmallRng};
use rand::{Rng, SeedableRng};

/// The reconnect schedule, in seconds. Index 0 is the delay
/// before the first reconnect attempt, index 1 before the
/// second, and so on. The 6th and later attempts both map to
/// 30s; the array is the v1 schedule verbatim from the
/// architecture.
pub const SCHEDULE_SECONDS: &[u64] = &[1, 2, 4, 8, 16, 30, 30];

/// The jitter magnitude. The actual delay is in
/// `[base * (1 - JITTER_PCT), base * (1 + JITTER_PCT)]`.
pub const JITTER_PCT: f64 = 0.20;

/// State for the backoff schedule. The struct holds the current
/// attempt counter and a PRNG. It is `Clone` so it can be
/// shared inside a `SignalingInner`; the PRNG is the only
/// non-`Copy` field, so the clone is cheap.
#[derive(Debug, Clone)]
pub struct Backoff {
    /// Zero-based attempt index. `0` means the next call to
    /// `next_delay` returns the first schedule entry. After
    /// each `next_delay` call the counter is incremented.
    attempt: u32,
    /// PRNG used to draw the jitter. Seeded from `OsRng` in
    /// [`Backoff::new`]; tests use [`Backoff::with_rng`].
    rng: SmallRng,
}

impl Backoff {
    /// Construct a fresh backoff with a PRNG seeded from the
    /// OS CSPRNG. The first `next_delay` call returns
    /// `SCHEDULE_SECONDS[0]` (1s) with jitter.
    pub fn new() -> Self {
        Self {
            attempt: 0,
            rng: SmallRng::from_rng(OsRng).expect("OsRng is available"),
        }
    }

    /// Construct a backoff with a caller-supplied PRNG. Used by
    /// tests for deterministic jitter.
    pub fn with_rng(rng: SmallRng) -> Self {
        Self { attempt: 0, rng }
    }

    /// The base delay for the upcoming attempt, in seconds.
    /// The 7th and later attempts both clamp to 30s.
    pub fn base_seconds(&self) -> u64 {
        let idx = (self.attempt as usize).min(SCHEDULE_SECONDS.len() - 1);
        SCHEDULE_SECONDS[idx]
    }

    /// Compute the next delay with +/-20% jitter and advance
    /// the attempt counter. The returned duration is bounded by
    /// `base * (1 - JITTER_PCT)` on the low end and
    /// `base * (1 + JITTER_PCT)` on the high end; the lower
    /// bound is floored at 1ms to avoid a 0ms sleep.
    pub fn next_delay(&mut self) -> Duration {
        let base_ms = (self.base_seconds() as f64) * 1000.0;
        let jitter: f64 = self.rng.gen_range(-JITTER_PCT..=JITTER_PCT);
        let scaled = (base_ms * (1.0 + jitter)).round().max(1.0) as u64;
        let delay = Duration::from_millis(scaled);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// Skip the remaining schedule and force the next call to
    /// `next_delay` to return a 30s-base delay with jitter.
    /// Used after an AUTH_FAIL so a banned or rejected client
    /// does not tight-loop on the early schedule.
    pub fn skip_to_cap(&mut self) {
        self.attempt = (SCHEDULE_SECONDS.len() - 1) as u32;
    }

    /// Reset the backoff to attempt 0. Called after a
    /// successful AUTH_OK so a fresh cycle starts at 1s.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// The current attempt counter (0-based, read-only).
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_constants_match_architecture() {
        assert_eq!(SCHEDULE_SECONDS, &[1u64, 2, 4, 8, 16, 30, 30]);
    }

    #[test]
    fn jitter_constant_is_twenty_percent() {
        assert!((JITTER_PCT - 0.20).abs() < f64::EPSILON);
    }

    #[test]
    fn base_seconds_walks_schedule() {
        // Deterministic PRNG; the schedule is independent of
        // the PRNG state.
        let mut b = Backoff::with_rng(SmallRng::seed_from_u64(0xC0FFEE));
        let expected = [1u64, 2, 4, 8, 16, 30, 30, 30, 30];
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(b.base_seconds(), *want, "index {i}");
            // Advance so the next iteration sees a different
            // schedule entry.
            let _ = b.next_delay();
        }
    }

    #[test]
    fn jitter_stays_in_bounds() {
        let mut b = Backoff::with_rng(SmallRng::seed_from_u64(0xDEADBEEF));
        // The schedule has 7 entries (1, 2, 4, 8, 16, 30,
        // 30). The 6th and later all clamp to base 30s. We
        // pull 1024 samples and assert each is within
        // +/-20% of the schedule entry that was current at
        // the time the sample was taken.
        for _ in 0..1024 {
            // Peek at the base before consuming it.
            let base = b.base_seconds() as f64 * 1000.0;
            let d = b.next_delay();
            let ms = d.as_millis() as f64;
            let lo = base * (1.0 - JITTER_PCT);
            let hi = base * (1.0 + JITTER_PCT);
            assert!(
                (lo..=hi).contains(&ms),
                "delay {ms}ms out of [{lo}, {hi}]ms for base {base}"
            );
        }
    }

    #[test]
    fn next_delay_increments_attempt() {
        let mut b = Backoff::with_rng(SmallRng::seed_from_u64(7));
        assert_eq!(b.attempt(), 0);
        let _ = b.next_delay();
        assert_eq!(b.attempt(), 1);
        let _ = b.next_delay();
        assert_eq!(b.attempt(), 2);
    }

    #[test]
    fn reset_returns_to_zero() {
        let mut b = Backoff::with_rng(SmallRng::seed_from_u64(7));
        for _ in 0..5 {
            let _ = b.next_delay();
        }
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.base_seconds(), 1);
    }

    #[test]
    fn skip_to_cap_lands_on_thirty() {
        let mut b = Backoff::with_rng(SmallRng::seed_from_u64(7));
        b.skip_to_cap();
        assert_eq!(b.base_seconds(), 30);
        let d = b.next_delay();
        // 30s base, +/- 20% => 24000..=36000ms.
        let ms = d.as_millis();
        assert!(
            (24_000..=36_000).contains(&ms),
            "delay {ms}ms out of cap range"
        );
    }
}
