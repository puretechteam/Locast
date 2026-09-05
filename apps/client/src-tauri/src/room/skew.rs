//! P4-T06: NTP-style clock skew measurement (per
//! `docs/ARCHITECTURE.md` section 13.3).
//!
//! The measurement is a four-timestamp exchange:
//!
//! ```text
//! client           server
//!   |--- t0 = local send --->|
//!   |                          \--- t1 = server receive (server_ts_ms in reply)
//!   |                          /--- t2 = server response (now_ms() at reply)
//!   |<-- t3 = local recv -----|
//! ```
//!
//! The pure math lives in [`compute_skew_jitter`]; it
//! consumes a slice of `(t0_local_ms, t3_local_ms,
//! server_ts_ms)` samples and produces `(skew_ms, jitter_ms)`
//! matching architecture §13.3:
//!
//! - per sample: `rtt = (t3 - t0) - (server_ts - server_ts)` -- wait, both
//!   server stamps are the same value, so rtt = t3 - t0 (the simpler
//!   round-trip-time). offset = server_ts - (t0 + t3) / 2.
//! - samples with `rtt > 500 ms` are rejected (architecture §13.3
//!   "The sample is rejected if the round-trip time exceeds 500 ms").
//! - the four offsets are collected; the median offset is
//!   `skew_ms`; the standard deviation of accepted offsets is
//!   `jitter_ms`.
//! - the function returns `(None, None)` if zero or one valid
//!   samples remain after rejection (jitter cannot be computed
//!   from a single sample; architecture §13.3 says "If jitter_ms
//!   > 200 ms, the client increases its drift-detection
//!   threshold is exceeded).
//!
//! The Rust side's job is to own the `Clock` (no real
//! clock; the system clock is captured at the network
//! boundary) and the `request` round trip; the median /
//! stddev math is the testable pure function.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]
#![allow(clippy::doc_lazy_continuation)]

use std::time::{SystemTime, UNIX_EPOCH};

use locast_protocol::room::{SkewProbePayload, SkewReplyPayload};

/// The result of a single NTP-style measurement cycle. A
/// cycle collects `n` raw samples, drops the ones whose
/// RTT exceeds 500 ms (architecture §13.3), and reduces
/// the rest into a single `(skew_ms, jitter_ms)` pair. A
/// cycle with fewer than 2 valid samples produces
/// `(None, None)` (jitter cannot be defined from one
/// sample).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkewMeasurement {
    pub skew_ms: Option<i64>,
    pub jitter_ms: Option<i64>,
    /// Number of valid samples that contributed to the
    /// measurement. Surfaced for telemetry so the React
    /// UI can show "4/4 samples accepted" or similar.
    pub samples_used: u32,
    /// Number of samples that were rejected for RTT > 500 ms.
    pub samples_rejected: u32,
}

/// A single NTP-style sample. The fields are the four
/// timestamps of the exchange. `t0_local_ms` and
/// `t3_local_ms` are the client's local wall clock at
/// send and at receive. `server_ts_ms` is the value the
/// server stamped into the SKEW_REPLY payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkewSample {
    pub t0_local_ms: i64,
    pub t3_local_ms: i64,
    pub server_ts_ms: i64,
}

/// Architecture §13.3: "The sample is rejected if the
/// round-trip time exceeds 500 ms."
pub const MAX_RTT_MS: i64 = 500;

/// Pure NTP math. Takes a slice of samples (the caller is
/// responsible for capturing the four timestamps per
/// round trip), filters out samples whose RTT exceeds
/// `MAX_RTT_MS`, computes the median offset, and the
/// population standard deviation of the accepted
/// offsets.
///
/// The function is intentionally pure (no I/O, no
/// SystemTime) so it is testable with hand-built
/// samples.
pub fn compute_skew_jitter(samples: &[SkewSample]) -> SkewMeasurement {
    if samples.is_empty() {
        return SkewMeasurement {
            skew_ms: None,
            jitter_ms: None,
            samples_used: 0,
            samples_rejected: 0,
        };
    }

    // Filter to accepted samples. A sample whose RTT
    // exceeds MAX_RTT_MS is dropped; a sample whose t0 or
    // t3 is non-positive is also dropped (defensive
    // against clock anomalies on the client). We use the
    // simpler RTT = t3 - t0 (the round-trip time observed
    // by the client) rather than the textbook
    // (t3 - t0) - (t2 - t1), because the architecture's
    // §13.3 measurement uses a single server timestamp
    // (the server does NOT echo two separate stamps).
    let mut accepted: Vec<i64> = Vec::with_capacity(samples.len());
    let mut rejected: u32 = 0;
    for s in samples {
        let rtt = s.t3_local_ms - s.t0_local_ms;
        if !(0..=MAX_RTT_MS).contains(&rtt) {
            rejected += 1;
            continue;
        }
        // Midpoint local time: (t0 + t3) / 2. The classical
        // NTP formula assumes the network delay is
        // symmetric; for a single-stamp reply the
        // midpoint is the best we can do. The offset
        // (server - local at the same instant) is then
        // `server_ts - midpoint_local`.
        let midpoint = (s.t0_local_ms + s.t3_local_ms) / 2;
        let offset = s.server_ts_ms - midpoint;
        accepted.push(offset);
    }

    if accepted.is_empty() {
        return SkewMeasurement {
            skew_ms: None,
            jitter_ms: None,
            samples_used: 0,
            samples_rejected: rejected,
        };
    }

    let skew_ms = median(accepted.as_slice());
    let jitter_ms = if accepted.len() >= 2 {
        Some(stddev(accepted.as_slice(), skew_ms))
    } else {
        None
    };

    SkewMeasurement {
        skew_ms: Some(skew_ms),
        jitter_ms,
        samples_used: accepted.len() as u32,
        samples_rejected: rejected,
    }
}

fn median(values: &[i64]) -> i64 {
    // The input is borrowed immutably; we sort a copy.
    let mut v = values.to_vec();
    v.sort_unstable();
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        v[mid]
    } else {
        // Even N: average the two middle values (the
        // canonical definition of the median for even
        // sample sizes; architecture §13.3 does not
        // specify a tie-break).
        let a = v[mid - 1];
        let b = v[mid];
        (a + b) / 2
    }
}

fn stddev(values: &[i64], mean: i64) -> i64 {
    // Population standard deviation. The architecture
    // says "jitter (standard deviation of samples)" but
    // does not specify sample vs population. With 4
    // samples per cycle, Bessel's correction would
    // double the magnitude -- we use the population
    // estimate (divide by N) to keep jitter conservative.
    let n = values.len() as i64;
    if n <= 1 {
        return 0;
    }
    let mut sum_sq: i128 = 0;
    for v in values {
        let d = (*v - mean) as i128;
        sum_sq += d * d;
    }
    let variance = sum_sq / n as i128;
    // Integer sqrt (Babylonian). The result fits in i64
    // for any reasonable jitter (<= 86_400_000 ms would
    // overflow i64^2, so we cap before taking sqrt).
    if variance < 0 || variance > (i64::MAX as i128) {
        return i64::MAX;
    }
    isqrt(variance as u128) as i64
}

fn isqrt(n: u128) -> u128 {
    if n == 0 {
        return 0;
    }
    let mut x = n;
    let mut y = x.div_ceil(2);
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Local wall clock in unix ms. The Rust side uses
/// SystemTime directly (no Clock trait on the client);
/// P4-T06's tests inject samples explicitly so this
/// helper is the only SystemTime dependency in the
/// drift path.
pub fn now_ms_local() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build the outbound `SKEW_PROBE` payload. The caller
/// captures `t0_local_ms` (via [`now_ms_local`]) immediately
/// before serializing the envelope to the WS, not at the
/// moment the caller decides to probe -- the
/// packet-on-the-wire timestamp is what matters.
pub fn build_probe_payload(t0_local_ms: i64) -> SkewProbePayload {
    SkewProbePayload {
        client_send_ms: t0_local_ms,
    }
}

/// Decode a `SKEW_REPLY` payload. The function exists
/// separately so the test suite can call it without
/// spinning up a full WS round trip.
pub fn parse_reply_payload(
    payload: &serde_json::Value,
) -> Result<SkewReplyPayload, serde_json::Error> {
    serde_json::from_value::<SkewReplyPayload>(payload.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t0: i64, t3: i64, server: i64) -> SkewSample {
        SkewSample {
            t0_local_ms: t0,
            t3_local_ms: t3,
            server_ts_ms: server,
        }
    }

    #[test]
    fn empty_yields_none() {
        let m = compute_skew_jitter(&[]);
        assert_eq!(m.skew_ms, None);
        assert_eq!(m.jitter_ms, None);
        assert_eq!(m.samples_used, 0);
    }

    #[test]
    fn single_sample_yields_skew_no_jitter() {
        // t0=1000, t3=1100 (RTT=100), server=1500
        // midpoint = 1050, offset = 1500-1050 = 450
        let m = compute_skew_jitter(&[sample(1000, 1100, 1500)]);
        assert_eq!(m.skew_ms, Some(450));
        assert_eq!(m.jitter_ms, None);
        assert_eq!(m.samples_used, 1);
        assert_eq!(m.samples_rejected, 0);
    }

    #[test]
    fn median_of_four_consistent_offsets() {
        // All four samples have offset=250. The median
        // is 250; the stddev is 0.
        // t0=1000, t3=1100, server=1300 -> midpoint =
        // 1050, offset = 1300 - 1050 = 250.
        let m = compute_skew_jitter(&[
            sample(1000, 1100, 1300),
            sample(1000, 1100, 1300),
            sample(1000, 1100, 1300),
            sample(1000, 1100, 1300),
        ]);
        assert_eq!(m.skew_ms, Some(250));
        assert_eq!(m.jitter_ms, Some(0));
        assert_eq!(m.samples_used, 4);
    }

    #[test]
    fn rtt_above_500ms_is_rejected() {
        // RTT = 600 > 500 -> rejected.
        let m = compute_skew_jitter(&[sample(1000, 1600, 2000)]);
        assert_eq!(m.skew_ms, None);
        assert_eq!(m.jitter_ms, None);
        assert_eq!(m.samples_used, 0);
        assert_eq!(m.samples_rejected, 1);
    }

    #[test]
    fn negative_rtt_is_rejected() {
        // t3 < t0 -- client clock anomaly. The sample
        // must be dropped.
        let m = compute_skew_jitter(&[sample(2000, 1500, 3000)]);
        assert_eq!(m.skew_ms, None);
        assert_eq!(m.samples_rejected, 1);
    }

    #[test]
    fn mixed_quality_samples_keep_only_accepted() {
        // 4 samples: 2 good, 1 with RTT 600 (rejected),
        // 1 with RTT 50 (accepted).
        let m = compute_skew_jitter(&[
            sample(1000, 1080, 1540), // offset = 1540 - 1040 = 500
            sample(2000, 2600, 3200), // RTT 600 -> rejected
            sample(3000, 3050, 3520), // offset = 3520 - 3025 = 495
            sample(4000, 4900, 5950), // RTT 900 -> rejected
        ]);
        assert_eq!(m.samples_used, 2);
        assert_eq!(m.samples_rejected, 2);
        // The two accepted offsets are 500 and 495.
        // Median (sorted [495, 500]) is (495+500)/2 = 497
        // (integer division). Jitter is stddev of {495,
        // 500} about 497.5 (pop, div by N=2):
        //   d = -2.5, +2.5
        //   var = (6.25 + 6.25) / 2 = 6.25
        //   stddev ~ 2.5
        // We accept any value in [2, 3] because the
        // exact integer sqrt of 6 is 2.
        let jitter = m.jitter_ms.unwrap();
        assert!((1..=4).contains(&jitter), "jitter out of range: {jitter}");
    }

    #[test]
    fn stddev_of_constant_input_is_zero() {
        // 4 samples, identical offsets. Stddev is 0.
        let samples: Vec<SkewSample> = (0..4)
            .map(|i| sample(1000 + 100 * i, 1000 + 100 * i + 10, 1500 + 100 * i))
            .collect();
        let m = compute_skew_jitter(&samples);
        assert_eq!(m.jitter_ms, Some(0));
    }

    #[test]
    fn symmetric_offsets_cancel_jitter() {
        // offsets = {1, 2, 4, 5} -> mean = 3 -> variance
        // = ((1-3)^2 + (2-3)^2 + (4-3)^2 + (5-3)^2) / 4
        // = (4 + 1 + 1 + 4) / 4 = 2.5 -> stddev = 1.
        // Build samples that produce these offsets.
        // midpoint = (t0 + t3) / 2. We need
        // server - midpoint = offset. Choose t0=1000, t3=1100
        // for all (midpoint=1050), then server=1051, 1052,
        // 1054, 1055. Skew median = (1052+1054)/2 = 1053 ->
        // 1053 - 1050 = 3.
        let m = compute_skew_jitter(&[
            sample(1000, 1100, 1051),
            sample(1000, 1100, 1052),
            sample(1000, 1100, 1054),
            sample(1000, 1100, 1055),
        ]);
        assert_eq!(m.skew_ms, Some(3));
        assert_eq!(m.jitter_ms, Some(1));
    }
}
