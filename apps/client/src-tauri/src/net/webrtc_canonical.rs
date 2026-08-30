//! Canonical signed bytes for SIGNAL envelopes (P3-T05).
//!
//! `locast_crypto::signal_signed_bytes` is the single source
//! of truth for the bytes that get signed and verified
//! across the wire (architecture §18.9). This module
//! re-exports it for callers within the client crate that
//! prefer the local path.

pub use locast_crypto::signal_signed_bytes;
