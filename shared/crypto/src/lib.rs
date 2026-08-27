//! Shared crypto crate.
//!
//! P0-T01: placeholder. P1-T03 added the BLAKE3 streaming hasher used for
//! full-file integrity. P2-T02 adds:
//!
//! - [`ed25519`] - thin Ed25519 sign / verify wrappers around
//!   `ed25519_dalek` with explicit zero-pubkey rejection (small-subgroup
//!   attack guard).
//! - [`sha256`] - SHA-256 digest helpers used by the server for bearer
//!   token hashing and other one-shot digests.
//! - [`domain_tag`] - 16-byte domain separation tag per
//!   `docs/ARCHITECTURE.md` section 18.9. Reserved for post-handshake
//!   signed envelopes; the handshake itself uses raw-byte signing over
//!   the 32-byte nonce (§20.4.4).
//! - [`canonical`] - canonical MessagePack encoder stub. v1 protocol
//!   messages are field-name-tagged structs and do not require sorted
//!   map keys; the canonical encoder is reserved for future map-typed
//!   payloads.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod blake3;
pub mod canonical;
pub mod domain_tag;
pub mod ed25519;
pub mod sha256;

/// Library version string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
