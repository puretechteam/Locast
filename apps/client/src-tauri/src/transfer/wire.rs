//! P3-T06 transfer wire framing.
//!
//! This module defines the framing used between a Locast source
//! (host) and a Locast viewer (downloader) over a per-peer
//! transport. The wire is a length-prefixed JSON stream:
//!
//! ```text
//! Frame = u32-be length || serde_json::Value bytes
//! ```
//!
//! The architecture (section 9) describes a similar shape using
//! CBOR ("the wire format is length-prefixed CBOR (or JSON for
//! v1 simplicity; CBOR is the documented v1.1 target)"). P3-T06
//! pins this decision to **JSON** for v1 and reserves the CBOR
//! migration for v1.1. The framing itself is identical for both
//! encodings; only the `serde_json::to_vec` / `from_slice` calls
//! in [`codec::encode`] and [` codec::decode`] would change.
//!
//! # Frame kinds
//!
//! The closed enum [`Frame`] is the only thing that ever crosses
//! the transport. Every variant is bound to a single
//! [`transfer::plan::DownloadPlan`](crate::transfer::plan::DownloadPlan)
//! and carries no peer-trusted metadata that the receiver does
//! not already possess from the verified manifest:
//!
//! - `Hello` — viewer → source: announces the verified peer
//!   pubkey, the download id, the media id, the manifest version,
//!   and the bitmap of chunks the viewer already has. The source
//!   refuses to send any chunk until it has verified the pubkey
//!   against the room's known host identity and accepted the
//!   download id.
//! - `Offer` — source → viewer: confirms the download id, the
//!   media id, the manifest version, the total bytes, the full
//!   file SHA-256, and the full file BLAKE3. The viewer must
//!   re-verify every field against the bound plan before it
//!   accepts any chunk.
//! - `Request` — viewer → source: a single `chunk_index`.
//! - `Chunk` — source → viewer: a single chunk's bytes plus the
//!   SHA-256 (lowercase hex) the source computed. The viewer
//!   re-hashes independently and compares; a mismatch is
//!   resolved by sending `Nak` for the same index.
//! - `Ack` — viewer → source: confirms `chunk_index` is verified
//!   on disk. Closes the in-flight slot in the sliding window.
//! - `Nak` — viewer → source: rejects `chunk_index`, carries the
//!   SHA-256 the viewer expected. Re-queues the chunk.
//! - `Cancel` — either direction: tears the session down. The
//!   receiver does not modify the persistent state beyond what
//!   `mark_chunk_verified` already persisted.
//! - `Error` — either direction: protocol-level error. Closes
//!   the session. The reason string is bounded to
//!   [`MAX_ERROR_LEN`] bytes and is never required to carry any
//!   secret material.
//!
//! # Bounds
//!
//! Every wire-frame size is bounded:
//!
//! - A single frame is at most [`MAX_FRAME_BYTES`] (1 MiB). Any
//!   length prefix above that is rejected without reading the
//!   payload.
//! - The `Error.reason` string is at most [`MAX_ERROR_LEN`]
//!   bytes. Longer strings are rejected.
//! - All string fields use the existing validators in
//!   [`crate::core::paths`] (sha / download id / sanitized
//!   filename) and [`crate::room::peer_id`]. The wire parser
//!   enforces them at the parse boundary, not later.
//!
//! # Security
//!
//! The wire framing is deliberately agnostic of *who* the
//! remote peer is. Authentication is layered on top by the
//! [`crate::net::webrtc`] layer (DTLS) and by the `Hello`
//! pubkey handshake in [`crate::transfer::session`]. The wire
//! types in this file hold no private key material, no bearer
//! token, no SDP body, no ICE candidate, and no file content.
//! `tracing` events emitted from this module reference only
//! stable, non-sensitive identifiers (`download_id`,
//! `chunk_index`, frame kind, byte counts).
//!
//! # Panic / unwrap discipline
//!
//! Every public function returns `Result<_, WireError>`. There
//! are no `panic!`, `unwrap()`, or `expect()` calls in the
//! non-test code paths. The protocol parser is total on
//! attacker-controlled input.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::io;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::room::peer_id::{derive_peer_id, is_canonical_peer_id};

/// Maximum size of a single encoded wire frame (length-prefix
/// payload), in bytes. The architecture permits up to "256 KiB +
/// 1 KiB overhead" per chunk frame; we round up to 1 MiB to
////// also cover `Hello` / `Offer` / `Error` frames that carry
////// descriptive state. Frames larger than this are rejected
////// at the parser without reading the payload.
pub const MAX_FRAME_BYTES: u32 = 1024 * 1024;

/// Maximum size of an `Error.reason` string, in bytes. Anything
/// longer is rejected at parse time.
pub const MAX_ERROR_LEN: usize = 512;

/// The closed set of frame kinds. Every wire frame decodes into
/// exactly one of these variants. Unknown / future kinds are
/// rejected with [`WireError::UnknownKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    Hello,
    Offer,
    Request,
    Chunk,
    Ack,
    Nak,
    Cancel,
    Error,
}

impl FrameKind {
    /// Stable string for use in `tracing` fields. Never logs
    /// the payload of the frame.
    pub fn as_str(self) -> &'static str {
        match self {
            FrameKind::Hello => "hello",
            FrameKind::Offer => "offer",
            FrameKind::Request => "request",
            FrameKind::Chunk => "chunk",
            FrameKind::Ack => "ack",
            FrameKind::Nak => "nak",
            FrameKind::Cancel => "cancel",
            FrameKind::Error => "error",
        }
    }
}

/// A single transfer-protocol frame. All variants share the
/// leading `kind` / `download_id` / `media_id` / `manifest_version`
/// fields so the receiver can dispatch without parsing every
/// variant up front. Sensitive material is never present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Frame {
    Hello(HelloFrame),
    Offer(OfferFrame),
    Request(RequestFrame),
    Chunk(ChunkFrame),
    Ack(AckFrame),
    Nak(NakFrame),
    Cancel(CancelFrame),
    Error(ErrorFrame),
}

impl Frame {
    /// The frame kind, without unwrapping the variant.
    pub fn kind(&self) -> FrameKind {
        match self {
            Frame::Hello(_) => FrameKind::Hello,
            Frame::Offer(_) => FrameKind::Offer,
            Frame::Request(_) => FrameKind::Request,
            Frame::Chunk(_) => FrameKind::Chunk,
            Frame::Ack(_) => FrameKind::Ack,
            Frame::Nak(_) => FrameKind::Nak,
            Frame::Cancel(_) => FrameKind::Cancel,
            Frame::Error(_) => FrameKind::Error,
        }
    }

    /// The opaque download id every variant carries. Returns
    /// `None` if the frame is malformed in a way that should
    /// never happen (all variants have the field).
    pub fn download_id(&self) -> Option<&str> {
        match self {
            Frame::Hello(f) => Some(f.download_id.as_str()),
            Frame::Offer(f) => Some(f.download_id.as_str()),
            Frame::Request(f) => Some(f.download_id.as_str()),
            Frame::Chunk(f) => Some(f.download_id.as_str()),
            Frame::Ack(f) => Some(f.download_id.as_str()),
            Frame::Nak(f) => Some(f.download_id.as_str()),
            Frame::Cancel(f) => Some(f.download_id.as_str()),
            Frame::Error(f) => Some(f.download_id.as_str()),
        }
    }
}

/// Viewer → source: announce identity + resume bitmap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloFrame {
    /// Canonical peer_id of the sender (sha256 of the raw
    /// 32-byte Ed25519 pubkey, 64 lowercase hex chars).
    pub peer_id: String,
    /// Opaque download id (UUID v7 string).
    pub download_id: String,
    /// Opaque media id (UUID string from the verified manifest).
    pub media_id: String,
    /// Manifest version bound to this download.
    pub manifest_version: i64,
    /// Set of chunk indices the viewer already has on disk in
    /// `verified` or `received` state. Source MUST skip them.
    /// Stored sorted ascending by the sender.
    pub have_chunks: Vec<u32>,
}

/// Source → viewer: confirm the download + manifest binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfferFrame {
    /// Canonical peer_id of the source.
    pub peer_id: String,
    /// Opaque download id.
    pub download_id: String,
    /// Opaque media id.
    pub media_id: String,
    /// Manifest version bound to this download.
    pub manifest_version: i64,
    /// Total file size in bytes.
    pub total_bytes: u64,
    /// Chunk size in bytes. Must equal the locked 256 KiB.
    pub chunk_size_bytes: u32,
    /// Total chunk count. Must equal `ceil(total_bytes / chunk_size_bytes)`.
    pub total_chunks: u32,
    /// Full-file SHA-256 (64 lowercase hex chars).
    pub sha256: String,
    /// Full-file BLAKE3 (64 lowercase hex chars).
    pub blake3: String,
    /// Sanitized filename (no path separators).
    pub filename: String,
}

/// Viewer → source: request a single chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestFrame {
    pub download_id: String,
    pub chunk_index: u32,
}

/// Source → viewer: a single chunk's bytes.
///
/// The bytes are base64-encoded so the JSON payload stays a
/// well-formed UTF-8 string. The sha256 is computed by the
/// source over the raw bytes (before base64) and re-verified
/// by the viewer after base64 decode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkFrame {
    pub download_id: String,
    pub chunk_index: u32,
    /// Base64 (standard, padded) of the chunk bytes.
    pub bytes_b64: String,
    /// SHA-256 of the raw chunk bytes (64 lowercase hex).
    pub sha256: String,
}

/// Viewer → source: chunk accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AckFrame {
    pub download_id: String,
    pub chunk_index: u32,
}

/// Viewer → source: chunk rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NakFrame {
    pub download_id: String,
    pub chunk_index: u32,
    /// The SHA-256 the viewer expected for this index (from the
    /// bound plan). Informational; the source re-checks against
    /// its own copy.
    pub expected_sha256: String,
}

/// Either direction: end the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelFrame {
    pub download_id: String,
    /// Short tag identifying the cancel reason (one of
    /// `"user_cancel"`, `"complete"`, `"hash_mismatch"`,
    /// `"peer_disappeared"`, `"internal"`). Bounded.
    pub reason: String,
}

/// Either direction: protocol-level error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorFrame {
    pub download_id: String,
    /// Human-readable reason (≤ [`MAX_ERROR_LEN`] bytes).
    /// Never carries secrets, file content, or chunk bytes.
    pub reason: String,
}

/// Closed set of wire-protocol errors. Mirrors the
/// `PlanError` / `ChunkStateError` / `ChunkVerifyError` /
/// `PathError` pattern used elsewhere in the codebase.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WireError {
    /// Frame kind was not one of the closed variants.
    #[error("unknown frame kind")]
    UnknownKind,
    /// Length prefix was zero, > [`MAX_FRAME_BYTES`], or could
    /// not be read.
    #[error("invalid length prefix: {0}")]
    InvalidLength(u32),
    /// JSON payload could not be deserialized.
    #[error("malformed frame payload: {0}")]
    Malformed(String),
    /// `download_id` was empty or not uuid-shaped.
    #[error("invalid download_id: must be uuid-shaped lowercase hex + '-'")]
    InvalidDownloadId(String),
    /// `media_id` was empty.
    #[error("invalid media_id: must be non-empty")]
    InvalidMediaId(String),
    /// A peer_id field was not canonical (64 lowercase hex).
    #[error("invalid peer_id: must be 64 lowercase hex chars")]
    InvalidPeerId(String),
    /// A SHA-256 / BLAKE3 field was not 64 lowercase hex.
    #[error("invalid hash: must be 64 lowercase hex chars")]
    InvalidHash(String),
    /// A filename carried by an `Offer` frame failed the
    /// sanitizer.
    #[error("invalid filename: {0}")]
    InvalidFilename(String),
    /// `chunk_index` was out of `[0, total_chunks)`.
    #[error("chunk_index {index} out of range [0, {total})")]
    ChunkIndexOutOfRange { index: u32, total: u32 },
    /// `manifest_version` was `< 1`.
    #[error("invalid manifest_version: {0}")]
    InvalidManifestVersion(i64),
    /// `total_bytes` did not match what the manifest bound.
    #[error("total_bytes mismatch: got {got}, expected {expected}")]
    TotalBytesMismatch { got: u64, expected: u64 },
    /// `chunk_size_bytes` did not match the locked value.
    #[error("chunk_size_bytes mismatch: got {got}, expected {expected}")]
    ChunkSizeMismatch { got: u32, expected: u32 },
    /// `total_chunks` did not equal `ceil(total_bytes / chunk_size_bytes)`.
    #[error("total_chunks mismatch: got {got}, expected {expected}")]
    TotalChunksMismatch { got: u32, expected: u32 },
    /// `Error.reason` was longer than [`MAX_ERROR_LEN`].
    #[error("error reason too long: {len} > {max}")]
    ErrorReasonTooLong { len: usize, max: usize },
    /// An `Error.reason` was empty.
    #[error("error reason must be non-empty")]
    ErrorReasonEmpty,
    /// Underlying I/O error.
    #[error("transport io error: {0}")]
    Io(String),
}

impl From<io::Error> for WireError {
    fn from(e: io::Error) -> Self {
        WireError::Io(e.to_string())
    }
}

/// Validate that the string is a download id: lowercase hex
/// digits and dashes, no path separators. Mirrors the path
/// builder's `check_download_id` validator.
fn check_download_id(s: &str) -> Result<(), WireError> {
    if s.is_empty() || s.len() > 64 {
        return Err(WireError::InvalidDownloadId(s.to_string()));
    }
    for c in s.chars() {
        let ok = c.is_ascii_digit()
            || ('a'..='f').contains(&c)
            || c == '-';
        if !ok {
            return Err(WireError::InvalidDownloadId(s.to_string()));
        }
    }
    Ok(())
}

/// Validate a 64-char lowercase hex string. Same predicate as
/// `transfer::plan::is_64_lowercase_hex`.
fn check_64_lowercase_hex(s: &str) -> Result<(), WireError> {
    if s.len() != 64 {
        return Err(WireError::InvalidHash(s.to_string()));
    }
    for c in s.chars() {
        if !c.is_ascii_digit() && !('a'..='f').contains(&c) {
            return Err(WireError::InvalidHash(s.to_string()));
        }
    }
    Ok(())
}

/// Compute a canonical 64-char lowercase hex peer_id from a
/// raw 32-byte Ed25519 public key. Re-exported for callers
/// that need to derive the field value for outbound frames.
pub fn peer_id_from_pubkey(pubkey: &[u8; 32]) -> String {
    derive_peer_id(*pubkey)
}

/// Sanity-check every variant of a parsed [`Frame`] against
/// the shape caps. Run by [`codec::decode`] before returning
/// the parsed frame.
pub(crate) fn validate(f: &Frame) -> Result<(), WireError> {
    match f {
        Frame::Hello(x) => {
            check_download_id(&x.download_id)?;
            if !is_canonical_peer_id(&x.peer_id) {
                return Err(WireError::InvalidPeerId(x.peer_id.clone()));
            }
            if x.media_id.is_empty() || x.media_id.len() > 64 {
                return Err(WireError::InvalidMediaId(x.media_id.clone()));
            }
            if x.manifest_version < 1 {
                return Err(WireError::InvalidManifestVersion(x.manifest_version));
            }
            for &i in &x.have_chunks {
                if i >= u32::MAX {
                    return Err(WireError::ChunkIndexOutOfRange {
                        index: i,
                        total: u32::MAX,
                    });
                }
            }
            Ok(())
        }
        Frame::Offer(x) => {
            check_download_id(&x.download_id)?;
            if !is_canonical_peer_id(&x.peer_id) {
                return Err(WireError::InvalidPeerId(x.peer_id.clone()));
            }
            if x.media_id.is_empty() || x.media_id.len() > 64 {
                return Err(WireError::InvalidMediaId(x.media_id.clone()));
            }
            if x.manifest_version < 1 {
                return Err(WireError::InvalidManifestVersion(x.manifest_version));
            }
            check_64_lowercase_hex(&x.sha256)?;
            check_64_lowercase_hex(&x.blake3)?;
            crate::core::library::sanitize::sanitize(&x.filename)
                .map_err(|e| WireError::InvalidFilename(e.to_string()))?;
            Ok(())
        }
        Frame::Request(x) => {
            check_download_id(&x.download_id)?;
            Ok(())
        }
        Frame::Chunk(x) => {
            check_download_id(&x.download_id)?;
            check_64_lowercase_hex(&x.sha256)?;
            Ok(())
        }
        Frame::Ack(x) => {
            check_download_id(&x.download_id)?;
            Ok(())
        }
        Frame::Nak(x) => {
            check_download_id(&x.download_id)?;
            check_64_lowercase_hex(&x.expected_sha256)?;
            Ok(())
        }
        Frame::Cancel(x) => {
            check_download_id(&x.download_id)?;
            if x.reason.is_empty() {
                return Err(WireError::ErrorReasonEmpty);
            }
            if x.reason.len() > MAX_ERROR_LEN {
                return Err(WireError::ErrorReasonTooLong {
                    len: x.reason.len(),
                    max: MAX_ERROR_LEN,
                });
            }
            Ok(())
        }
        Frame::Error(x) => {
            check_download_id(&x.download_id)?;
            if x.reason.is_empty() {
                return Err(WireError::ErrorReasonEmpty);
            }
            if x.reason.len() > MAX_ERROR_LEN {
                return Err(WireError::ErrorReasonTooLong {
                    len: x.reason.len(),
                    max: MAX_ERROR_LEN,
                });
            }
            Ok(())
        }
    }
}

/// Length-prefixed JSON codec. Length is `u32` big-endian.
pub mod codec {
    use super::*;

    /// Serialize one [`Frame`] and append a single
    /// length-prefixed JSON frame to `out`. The buffer is
    /// NOT cleared; callers append as many frames as they
    /// like.
    pub fn encode(f: &Frame, out: &mut Vec<u8>) -> Result<(), WireError> {
        let payload = serde_json::to_vec(f)
            .map_err(|e| WireError::Malformed(format!("encode: {e}")))?;
        if payload.len() > MAX_FRAME_BYTES as usize {
            return Err(WireError::InvalidLength(payload.len() as u32));
        }
        let len = payload.len() as u32;
        out.reserve(4 + payload.len());
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&payload);
        Ok(())
    }

    /// Deserialize a single length-prefixed JSON frame from
    /// `bytes` (starting at offset 0). On success, returns the
    /// parsed frame and the number of bytes consumed (always
    /// `4 + payload.len()`). On error, the number of bytes
    /// consumed before the failure is returned so the caller
    /// can advance the read cursor and skip the malformed
    /// frame (used by the loopback transport for resilience).
    pub fn decode(bytes: &[u8]) -> Result<(Frame, usize), WireError> {
        if bytes.len() < 4 {
            return Err(WireError::Io(format!(
                "short read: {} bytes (< 4-byte length prefix)",
                bytes.len()
            )));
        }
        let len = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if len == 0 {
            return Err(WireError::InvalidLength(0));
        }
        if len > MAX_FRAME_BYTES {
            return Err(WireError::InvalidLength(len));
        }
        let total = 4 + len as usize;
        if bytes.len() < total {
            return Err(WireError::Io(format!(
                "short read: {} bytes (< declared {})",
                bytes.len(),
                total
            )));
        }
        let payload = &bytes[4..total];
        let frame: Frame = serde_json::from_slice(payload)
            .map_err(|e| WireError::Malformed(format!("decode: {e}")))?;
        validate(&frame)?;
        Ok((frame, total))
    }

    /// Decode all length-prefixed frames in a contiguous
    /// buffer. Stops at the first parse error and returns the
    /// remaining bytes. Used by the loopback transport's
    /// reader.
    pub fn decode_stream(
        bytes: &[u8],
    ) -> Result<(Vec<Frame>, usize), WireError> {
        let mut out = Vec::new();
        let mut cursor = 0usize;
        while cursor < bytes.len() {
            // Need at least the length prefix.
            if bytes.len() - cursor < 4 {
                return Err(WireError::Io(format!(
                    "trailing {} bytes (< 4-byte length prefix)",
                    bytes.len() - cursor
                )));
            }
            let len = u32::from_be_bytes([
                bytes[cursor],
                bytes[cursor + 1],
                bytes[cursor + 2],
                bytes[cursor + 3],
            ]);
            if len == 0 || len > MAX_FRAME_BYTES {
                return Err(WireError::InvalidLength(len));
            }
            let total = 4 + len as usize;
            if bytes.len() - cursor < total {
                return Err(WireError::Io(format!(
                    "short read at offset {cursor}: need {total} bytes, have {}",
                    bytes.len() - cursor
                )));
            }
            let payload = &bytes[cursor + 4..cursor + total];
            let frame: Frame = serde_json::from_slice(payload)
                .map_err(|e| WireError::Malformed(format!("decode: {e}")))?;
            validate(&frame)?;
            out.push(frame);
            cursor += total;
        }
        Ok((out, cursor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello() -> Frame {
        Frame::Hello(HelloFrame {
            peer_id: peer_id_from_pubkey(&[1u8; 32]),
            download_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
            media_id: "m-1".into(),
            manifest_version: 1,
            have_chunks: vec![0, 1, 2],
        })
    }

    #[test]
    fn roundtrip_hello() {
        let f = hello();
        let mut buf = Vec::new();
        codec::encode(&f, &mut buf).expect("encode");
        assert!(buf.len() <= MAX_FRAME_BYTES as usize + 4);
        let (decoded, used) = codec::decode(&buf).expect("decode");
        assert_eq!(used, buf.len());
        assert_eq!(decoded, f);
    }

    #[test]
    fn roundtrip_offer_minimal() {
        let f = Frame::Offer(OfferFrame {
            peer_id: peer_id_from_pubkey(&[2u8; 32]),
            download_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
            media_id: "m-1".into(),
            manifest_version: 1,
            total_bytes: 1024,
            chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
            total_chunks: 1,
            sha256: "a".repeat(64),
            blake3: "b".repeat(64),
            filename: "movie.mp4".into(),
        });
        let mut buf = Vec::new();
        codec::encode(&f, &mut buf).expect("encode");
        let (decoded, used) = codec::decode(&buf).expect("decode");
        assert_eq!(used, buf.len());
        assert_eq!(decoded, f);
    }

    #[test]
    fn decode_rejects_zero_length() {
        let buf = [0u8, 0, 0, 0];
        let err = codec::decode(&buf).unwrap_err();
        assert!(matches!(err, WireError::InvalidLength(0)));
    }

    #[test]
    fn decode_rejects_oversized_length() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&((MAX_FRAME_BYTES + 1).to_be_bytes()));
        let err = codec::decode(&buf).unwrap_err();
        assert!(matches!(err, WireError::InvalidLength(_)));
    }

    #[test]
    fn decode_rejects_bad_json() {
        // Length says 6 bytes, payload is "{not json"
        let mut buf = Vec::new();
        buf.extend_from_slice(&6u32.to_be_bytes());
        buf.extend_from_slice(b"{nope}");
        let err = codec::decode(&buf).unwrap_err();
        assert!(matches!(err, WireError::Malformed(_)));
    }

    #[test]
    fn decode_rejects_short_read() {
        let buf = [0u8, 0, 0, 5, 1, 2, 3];
        let err = codec::decode(&buf).unwrap_err();
        assert!(matches!(err, WireError::Io(_)));
    }

    #[test]
    fn decode_rejects_bad_download_id() {
        let mut f = hello();
        match &mut f {
            Frame::Hello(h) => h.download_id = "not uuid".into(),
            _ => unreachable!(),
        }
        let mut buf = Vec::new();
        codec::encode(&f, &mut buf).expect("encode");
        let err = codec::decode(&buf).unwrap_err();
        assert!(matches!(err, WireError::InvalidDownloadId(_)));
    }

    #[test]
    fn decode_rejects_uppercase_peer_id() {
        let mut f = hello();
        match &mut f {
            Frame::Hello(h) => h.peer_id = h.peer_id.to_uppercase(),
            _ => unreachable!(),
        }
        let mut buf = Vec::new();
        codec::encode(&f, &mut buf).expect("encode");
        let err = codec::decode(&buf).unwrap_err();
        assert!(matches!(err, WireError::InvalidPeerId(_)));
    }

    #[test]
    fn decode_rejects_offer_with_reserved_filename() {
        let f = Frame::Offer(OfferFrame {
            peer_id: peer_id_from_pubkey(&[2u8; 32]),
            download_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
            media_id: "m-1".into(),
            manifest_version: 1,
            total_bytes: 1024,
            chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
            total_chunks: 1,
            sha256: "a".repeat(64),
            blake3: "b".repeat(64),
            filename: "CON".into(),
        });
        let mut buf = Vec::new();
        codec::encode(&f, &mut buf).expect("encode");
        let err = codec::decode(&buf).unwrap_err();
        assert!(matches!(err, WireError::InvalidFilename(_)));
    }

    #[test]
    fn decode_rejects_offer_with_uppercase_sha() {
        let f = Frame::Offer(OfferFrame {
            peer_id: peer_id_from_pubkey(&[2u8; 32]),
            download_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
            media_id: "m-1".into(),
            manifest_version: 1,
            total_bytes: 1024,
            chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
            total_chunks: 1,
            sha256: "A".repeat(64),
            blake3: "b".repeat(64),
            filename: "movie.mp4".into(),
        });
        let mut buf = Vec::new();
        codec::encode(&f, &mut buf).expect("encode");
        let err = codec::decode(&buf).unwrap_err();
        assert!(matches!(err, WireError::InvalidHash(_)));
    }

    #[test]
    fn decode_rejects_error_reason_too_long() {
        let f = Frame::Error(ErrorFrame {
            download_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
            reason: "a".repeat(MAX_ERROR_LEN + 1),
        });
        let mut buf = Vec::new();
        codec::encode(&f, &mut buf).expect("encode");
        let err = codec::decode(&buf).unwrap_err();
        assert!(matches!(err, WireError::ErrorReasonTooLong { .. }));
    }

    #[test]
    fn decode_stream_decodes_multiple_frames() {
        let a = hello();
        let mut b = hello();
        if let Frame::Hello(h) = &mut b {
            h.download_id = "01234567-89ab-cdef-0123-456789abcde0".into();
        }
        let mut buf = Vec::new();
        codec::encode(&a, &mut buf).unwrap();
        codec::encode(&b, &mut buf).unwrap();
        let (frames, used) = codec::decode_stream(&buf).expect("stream");
        assert_eq!(used, buf.len());
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], a);
        assert_eq!(frames[1], b);
    }

    #[test]
    fn frame_kind_strings_are_stable() {
        // Pins the wire-visible strings so a downstream consumer
        // (TS bindings, log scrapers) can rely on them.
        assert_eq!(FrameKind::Hello.as_str(), "hello");
        assert_eq!(FrameKind::Offer.as_str(), "offer");
        assert_eq!(FrameKind::Request.as_str(), "request");
        assert_eq!(FrameKind::Chunk.as_str(), "chunk");
        assert_eq!(FrameKind::Ack.as_str(), "ack");
        assert_eq!(FrameKind::Nak.as_str(), "nak");
        assert_eq!(FrameKind::Cancel.as_str(), "cancel");
        assert_eq!(FrameKind::Error.as_str(), "error");
    }
}