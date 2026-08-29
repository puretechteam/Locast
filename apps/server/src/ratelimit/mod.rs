//! Per-connection rate limiter.
//!
//! Re-homes the `TokenBucket` struct that previously lived
//! inside [`crate::ws`] and wraps it in a [`PerConnLimiter`]
//! that drives both the msg/s and the bytes/s buckets from
//! one [`on_frame`] entry point. P2-T07's deliverable is
//! this module plus the [`crate::rooms::caps`] gate; the
//! `RATE_LIMIT` envelope is emitted from [`crate::ws`]
//! alongside the existing `AUTH_FAIL(Rate)` throttle path.
//!
//! Scope: per-CONNECTION only. The architecture documents
//! `scope: "conn" | "room" | "ip"` for the wire payload
//! (see [`locast_protocol::handshake::RateLimitScope`]) but
//! P2-T07 only emits `scope: Conn`. Per-room / per-IP
//! limits are intentionally NOT implemented here.

#![forbid(unsafe_code)]

use locast_protocol::handshake::{RateLimitPayload, RateLimitScope};

pub use crate::ws::TokenBucket;

/// The post-hit cooldown the server advertises to the
/// client via `RATE_LIMIT.retry_after_ms`. Matches the
/// existing 1 s silent-drop window in [`crate::ws`]
/// (`RATE_THROTTLE_MS`). Documented as the v1 default in
/// the spec; future work may tune it.
pub const DEFAULT_RETRY_AFTER_MS: i64 = 1_000;

/// A single rate-limit hit. Returned by
/// [`PerConnLimiter::on_frame`] when the connection
/// exceeded either the msg/s or the bytes/s bucket. The
/// caller turns this into a `RATE_LIMIT` envelope
/// (post-handshake) or an `AUTH_FAIL(Rate)` envelope
/// (handshake).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitHit {
    pub scope: RateLimitScope,
    pub observed: u32,
    pub limit: u32,
    pub retry_after_ms: u32,
}

impl RateLimitHit {
    /// Build the wire payload the server sends to the
    /// client. v1 always advertises 1 000 ms.
    pub fn to_payload(&self) -> RateLimitPayload {
        RateLimitPayload {
            scope: self.scope,
            retry_after_ms: self.retry_after_ms,
            observed: self.observed,
            limit: self.limit,
        }
    }
}

/// A per-connection pair of token buckets. P2-T07
/// maintains one `PerConnLimiter` per WebSocket connection
/// (no global state). Call order on a new frame is:
/// msg-bucket first, then bytes-bucket. If the msg bucket
/// misses, the bytes bucket is NOT touched (so a single
/// spam frame does not drain byte budget for the next
/// legitimate frame that the msg bucket would have
/// rejected first).
#[derive(Debug, Clone)]
pub struct PerConnLimiter {
    /// Messages-per-second bucket.
    pub msg: TokenBucket,
    /// Bytes-per-second bucket.
    pub bytes: TokenBucket,
    /// The msg/s capacity. Kept so the hit payload can
    /// report the configured limit without an extra
    /// plumbing parameter.
    msg_limit: u32,
    /// The bytes/s capacity. Same as above.
    bytes_limit: u32,
}

impl PerConnLimiter {
    /// Build a limiter with the configured rates. The
    /// `msg_per_sec` and `msg_burst` map to the msg
    /// bucket; the `bytes_per_sec` and `bytes_burst` map
    /// to the bytes bucket. The `*_per_sec` values are
    /// also retained as the `limit` field of any
    /// [`RateLimitHit`] this limiter produces, so the
    /// client can see which ceiling it tripped.
    pub fn new(msg_per_sec: u32, msg_burst: u32, bytes_per_sec: u32, bytes_burst: u32) -> Self {
        Self {
            msg: TokenBucket::new_with(msg_burst, msg_per_sec),
            bytes: TokenBucket::new_with(bytes_burst, bytes_per_sec),
            msg_limit: msg_per_sec,
            bytes_limit: bytes_per_sec,
        }
    }

    /// Try to consume `frame_bytes` from the connection's
    /// budget. Returns `Ok(())` on success, `Err(hit)` on
    /// the first bucket that missed. Call order: msg
    /// first, bytes second.
    pub fn on_frame(&mut self, frame_bytes: usize) -> Result<(), RateLimitHit> {
        if !self.msg.try_consume() {
            return Err(RateLimitHit {
                scope: RateLimitScope::Conn,
                observed: 1,
                limit: self.msg_limit,
                retry_after_ms: DEFAULT_RETRY_AFTER_MS as u32,
            });
        }
        let n = u32::try_from(frame_bytes).unwrap_or(u32::MAX);
        if !self.bytes.try_consume_n(n) {
            return Err(RateLimitHit {
                scope: RateLimitScope::Conn,
                observed: n,
                limit: self.bytes_limit,
                retry_after_ms: DEFAULT_RETRY_AFTER_MS as u32,
            });
        }
        Ok(())
    }

    /// Debit the msg bucket only. Returns `Some(hit)` if
    /// the bucket missed; the bytes bucket is left
    /// untouched. Used by the WS layer to short-circuit
    /// before reading the full frame body.
    pub fn check_msg(&mut self) -> Result<(), RateLimitHit> {
        if self.msg.try_consume() {
            Ok(())
        } else {
            Err(RateLimitHit {
                scope: RateLimitScope::Conn,
                observed: 1,
                limit: self.msg_limit,
                retry_after_ms: DEFAULT_RETRY_AFTER_MS as u32,
            })
        }
    }

    /// Debit the bytes bucket only. Called AFTER the frame
    /// is fully read into memory (security finding #5).
    pub fn check_bytes(&mut self, frame_bytes: usize) -> Result<(), RateLimitHit> {
        let n = u32::try_from(frame_bytes).unwrap_or(u32::MAX);
        if self.bytes.try_consume_n(n) {
            Ok(())
        } else {
            Err(RateLimitHit {
                scope: RateLimitScope::Conn,
                observed: n,
                limit: self.bytes_limit,
                retry_after_ms: DEFAULT_RETRY_AFTER_MS as u32,
            })
        }
    }

    /// Reset both buckets to full. Used on a successful
    /// AUTH so a fresh authed user gets a clean rate
    /// budget regardless of any pre-auth frames the
    /// connection may have received (security finding #2).
    pub fn reset(
        &mut self,
        msg_per_sec: u32,
        msg_burst: u32,
        bytes_per_sec: u32,
        bytes_burst: u32,
    ) {
        self.msg = TokenBucket::new_with(msg_burst, msg_per_sec);
        self.bytes = TokenBucket::new_with(bytes_burst, bytes_per_sec);
        self.msg_limit = msg_per_sec;
        self.bytes_limit = bytes_per_sec;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_bucket_accepts_to_capacity() {
        let mut tb = TokenBucket::new_with(5, 10);
        for _ in 0..5 {
            assert!(tb.try_consume(), "should accept within capacity");
        }
        assert!(!tb.try_consume(), "should reject past capacity");
    }

    #[test]
    fn try_consume_n_rejects_over_capacity() {
        let mut tb = TokenBucket::new_with(3, 10);
        assert!(tb.try_consume_n(3));
        assert!(!tb.try_consume_n(1));
    }

    #[test]
    fn per_conn_limiter_msg_bucket_takes_precedence() {
        // 1 msg/s with 1 burst; any second frame in the
        // same instant is a miss. The bytes bucket
        // (u32::MAX-ish) should NOT be touched, so a
        // subsequent fresh frame still succeeds once
        // tokens refill. We assert the msg path: a single
        // on_frame returns Ok, the next returns Err with
        // scope=Conn.
        let mut l = PerConnLimiter::new(1, 1, 1_000_000, 2_000_000);
        assert!(l.on_frame(100).is_ok());
        let hit = l.on_frame(100).expect_err("second frame should miss");
        assert_eq!(hit.scope, RateLimitScope::Conn);
        assert_eq!(hit.limit, 1);
        assert_eq!(hit.retry_after_ms, 1_000);
    }

    #[test]
    fn per_conn_limiter_bytes_bucket_trips_after_msg() {
        // 1000 msg/s burst, 100 bytes burst: a single 200-byte
        // frame after the first should miss the bytes bucket
        // (msg bucket has plenty).
        let mut l = PerConnLimiter::new(1_000, 1_000, 100, 100);
        assert!(l.on_frame(50).is_ok());
        let hit = l.on_frame(200).expect_err("oversized frame should miss");
        assert_eq!(hit.scope, RateLimitScope::Conn);
        assert_eq!(hit.observed, 200);
        assert_eq!(hit.limit, 100);
    }

    #[test]
    fn per_conn_limiter_reset_refills() {
        let mut l = PerConnLimiter::new(1, 1, 1, 1);
        // Drain both buckets.
        let _ = l.on_frame(1);
        let _ = l.on_frame(1);
        // Reset: should be full again.
        l.reset(10, 20, 1_000_000, 2_000_000);
        assert_eq!(l.msg_limit, 10);
        assert_eq!(l.bytes_limit, 1_000_000);
        for _ in 0..20 {
            assert!(l.on_frame(0).is_ok());
        }
        let hit = l.on_frame(0).expect_err("burst exhausted");
        assert_eq!(hit.limit, 10);
    }

    #[test]
    fn rate_limit_hit_to_payload_carries_fields() {
        let hit = RateLimitHit {
            scope: RateLimitScope::Conn,
            observed: 250,
            limit: 200,
            retry_after_ms: 1_000,
        };
        let p = hit.to_payload();
        assert_eq!(p.scope, RateLimitScope::Conn);
        assert_eq!(p.observed, 250);
        assert_eq!(p.limit, 200);
        assert_eq!(p.retry_after_ms, 1_000);
    }
}
