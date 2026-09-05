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

/// Produce the canonical signed bytes for a SIGNAL envelope:
/// `<16-byte domain tag> || <canonical msgpack of payload>`.
///
/// This is the single source of truth shared between the
/// server's `handle_signal` verifier and the client's outbound
/// `send_signal` signer. Architecture §18.9.
///
/// The 16-byte tag is produced by [`domain_tag::build`] with
/// the type name `"SIGNAL"`. The msgpack is
/// `rmp_serde::to_vec_named(payload)` (canonical map keys, no
/// extension types).
pub fn signal_signed_bytes(
    payload: &impl serde::Serialize,
) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let mut buf = Vec::with_capacity(16 + 256);
    buf.extend_from_slice(&domain_tag::build("SIGNAL"));
    let msg = rmp_serde::to_vec_named(payload)?;
    buf.extend_from_slice(&msg);
    Ok(buf)
}

/// Produce the canonical signed bytes for a DRAW_BEGIN
/// envelope (P5-T02).
///
/// Same shape as [`signal_signed_bytes`]: a 16-byte domain
/// tag (`"DRAW_START"`) followed by canonical msgpack of the
/// `StrokeBeginPayload`. The server's drawing dispatcher
/// (`apps/server/src/rooms/drawing.rs`) verifies the
/// signature against this canonical form before admitting
/// the stroke. DRAW_POINT and DRAW_END are NOT individually
/// signed; the BEGIN signature binds the entire stroke to
/// the originating user.
pub fn drawing_signed_bytes(
    payload: &impl serde::Serialize,
) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    let mut buf = Vec::with_capacity(16 + 256);
    buf.extend_from_slice(&domain_tag::build("DRAW_START"));
    let msg = rmp_serde::to_vec_named(payload)?;
    buf.extend_from_slice(&msg);
    Ok(buf)
}
