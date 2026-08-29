//! Manifest data model.
//!
//! These types describe the JSON shape of a Locast room manifest. They are
//! defined here so both the host (which builds a manifest) and the viewer
//! (which verifies one) can share a single source of truth for the field
//! names, types, and required-vs-optional classification. See
//! `docs/ARCHITECTURE.md` section 8 for the authoritative spec.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// A media entry inside a manifest's `media` array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct MediaEntry {
    /// Stable identifier for this media item (UUIDv4 in v1).
    pub id: String,
    /// File name as it appears in the source tree.
    pub filename: String,
    /// SHA-256 of the file's full contents, 64 lowercase hex chars.
    pub sha256: String,
    /// BLAKE3 of the file's full contents, 64 lowercase hex chars.
    pub blake3: String,
    /// File size in bytes.
    pub size_bytes: u64,
    /// MIME type.
    pub mime: String,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Optional dimensions. Absent (not `null`) from the canonical JSON
    /// when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<Dimensions>,
    /// Optional codec metadata. Absent (not `null`) from the canonical
    /// JSON when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codecs: Option<Codecs>,
    /// Ordered list of sources, host first.
    pub sources: Vec<Source>,
}

/// Pixel dimensions. Both fields are required when the parent
/// `Dimensions` value is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

/// Codec metadata for a media entry. Each component is optional so the
/// caller can describe audio-only, video-only, or container-only streams.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct Codecs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
}

/// A download source. The host is always the first entry in v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct Source {
    /// Peer identifier. Either a base64 Ed25519 public key or a
    /// `sha256:` hex prefix; the canonicalizer does not normalize
    /// between these two representations.
    pub peer_id: String,
    /// URL hint. v1 always emits `null` here.
    pub url_hint: Option<String>,
    /// 0 means "preferred". Higher numbers are deprioritized.
    pub priority: i32,
    /// Chunk size in bytes.
    pub chunk_size: u32,
    /// Number of chunks. Equal to `chunk_hashes.len()`.
    pub total_chunks: u32,
    /// Per-chunk SHA-256 hashes, one per chunk, in order.
    pub chunk_hashes: Vec<String>,
}

/// A subtitle entry. The `sources` field uses the same `Source` shape
/// as `MediaEntry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct SubtitleEntry {
    pub id: String,
    /// BCP-47 language tag.
    pub language: String,
    /// Human-readable label.
    pub label: String,
    pub filename: String,
    /// SHA-256 of the file's full contents, 64 lowercase hex chars.
    pub sha256: String,
    /// File size in bytes.
    pub size_bytes: u64,
    /// Subtitle codec, one of `srt`, `ass`, `ssa`, `vtt`, `webvtt`.
    pub codec: String,
    pub sources: Vec<Source>,
}

/// Host signature block. Present on the wire at runtime; the
/// canonicalizer always strips the actual value and emits `null` in the
/// canonical bytes regardless of what the data model holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct HostSignature {
    /// Public key identifier.
    pub public_key: String,
    /// Signature algorithm name (e.g. `ed25519`).
    pub algorithm: String,
    /// Signature value, base64 or hex encoded depending on algorithm.
    pub value: String,
}

/// Top-level Locast media manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct MediaManifest {
    /// Manifest schema version. Currently 1.
    pub manifest_version: u32,
    /// Room identifier (UUIDv4 in v1).
    pub room_id: String,
    /// Media items in the room. May be empty in principle; the viewer
    /// decides what "empty" means for its UI.
    pub media: Vec<MediaEntry>,
    /// Subtitle tracks. May be empty; serializes as `[]`.
    pub subtitles: Vec<SubtitleEntry>,
    /// Creation timestamp as Unix milliseconds. Display only; not part
    /// of the signed payload's identity contract.
    pub created_at: i64,
    /// Host signature. The canonicalizer always replaces this field's
    /// value with `null`; the data model retains whatever the host set.
    pub host_signature: Option<HostSignature>,
}
