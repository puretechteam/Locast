//! `room::host` - the host-side manifest publication path (P3-T03).
//!
//! The host's `room_create` flow (P2-T04) puts the host into a
//! room. Once the room is `Connected`, the host can publish
//! a signed `MediaManifest` to the room. This module owns
//! that flow:
//!
//! 1. Read the local `media_items` table (only `status =
//!    'permanent'` rows) and build an in-memory
//!    `MediaManifest`. P3-T04 will replace the single `Source`
//!    per entry with the real chunk planner output; this
//!    initial pass uses `chunk_size = 65536`, `total_chunks =
//!    1`, and a single `chunk_hashes` entry equal to the
//!    row's `sha256`. The host's own pubkey is the `peer_id`
//!    of the source.
//! 2. Sign the manifest through
//!    [`crate::identity::keystore::IdentityService::sign_manifest`].
//!    This is the only place the private key flows; the seed
//!    is dropped as soon as the function returns.
//! 3. Send the signed manifest as a `MANIFEST_PUBLISH`
//!    envelope over the existing `SignalingClient`. The
//!    server authenticates the bearer, enforces the
//!    `PUBLISH_MANIFEST` capability + the host-only check,
//!    persists the row, and broadcasts a
//!    `MANIFEST_PUBLISHED` back to every participant.
//!
//! The host does NOT write a local `room_manifests` row
//! before publishing; the host will receive its own
//! `MANIFEST_PUBLISHED` broadcast and the viewer's storage
//! path will persist the row. This keeps a single writer
//! path and avoids divergent local state between the host's
//! and the viewer's copies.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;

use locast_manifest::{Codecs, Dimensions, MediaEntry, MediaManifest, Source};
use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::room::ManifestPublishPayload;
use serde::Serialize;
use sqlx::SqlitePool;
use thiserror::Error;
use tracing::warn;
use uuid::Uuid;

use crate::identity::keystore::{IdentityService, IdentityServiceError};
use crate::net::signaling::{SignalingClient, SignalingError};

/// Default chunk size used by the v1 single-chunk stub.
/// P3-T04 will replace this with the real chunk-planner
/// output (per-file `chunk_size`, `total_chunks`, per-chunk
/// `sha256`).
const DEFAULT_CHUNK_SIZE: u32 = 65_536;

/// Default total-chunks for the v1 single-chunk stub. The
/// real total comes from `ceil(size_bytes / chunk_size)`
/// in P3-T04; the v1 stub always emits 1.
const DEFAULT_TOTAL_CHUNKS: u32 = 1;

/// Errors raised by the host publication path. The closed
/// set is mapped to the Tauri command surface.
#[derive(Debug, Error)]
pub enum HostError {
    /// The host does not currently have a room (no
    /// `room_create` / `room_join` was completed). The
    /// `manifest_publish` Tauri command should refuse the
    /// call before reaching this branch.
    #[error("not in a room")]
    NotInRoom,
    /// The local media library has no `permanent` rows to
    /// publish.
    #[error("no media items to publish")]
    NoMedia,
    /// A SQL query against the local `media_items` table
    /// failed.
    #[error("storage error: {0}")]
    Storage(String),
    /// Signing the manifest failed (keyring or canonical
    /// error).
    #[error("signing failed: {0}")]
    Signing(String),
    /// The underlying `SignalingClient` refused the
    /// `send_envelope` call.
    #[error("signaling: {0}")]
    Signaling(String),
}

impl From<sqlx::Error> for HostError {
    fn from(e: sqlx::Error) -> Self {
        HostError::Storage(e.to_string())
    }
}

impl From<IdentityServiceError> for HostError {
    fn from(e: IdentityServiceError) -> Self {
        HostError::Signing(e.to_string())
    }
}

impl From<SignalingError> for HostError {
    fn from(e: SignalingError) -> Self {
        HostError::Signaling(e.to_string())
    }
}

/// One `permanent` row from the local `media_items` table,
/// in the order selected by `build_manifest`. The tuple
/// type is local to this module to keep clippy's
/// `type_complexity` lint happy.
type MediaItemRow = (
    String,         // id
    String,         // filename
    String,         // sha256
    String,         // blake3
    i64,            // size_bytes
    String,         // mime
    i64,            // duration_ms
    Option<i64>,    // width
    Option<i64>,    // height
    Option<String>, // video_codec
    Option<String>, // audio_codec
    Option<String>, // container
);

/// Read all `permanent` media items and build an unsigned
/// `MediaManifest` for the given `room_id`. The host's own
/// pubkey (from the local identity) is used as the single
/// `Source::peer_id`. The single `Source` per entry uses
/// the v1 stub values (`chunk_size = 65_536`,
/// `total_chunks = 1`, `chunk_hashes = vec![<sha256>]`).
pub async fn build_manifest(
    pool: &SqlitePool,
    room_id: Uuid,
    host_pubkey_b64: &str,
) -> Result<MediaManifest, HostError> {
    let rows: Vec<MediaItemRow> = sqlx::query_as(
        "SELECT id, filename, sha256, blake3, size_bytes, mime, duration_ms, \
                width, height, video_codec, audio_codec, container \
         FROM media_items WHERE status = 'permanent' \
         ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Err(HostError::NoMedia);
    }

    let media: Vec<MediaEntry> = rows
        .into_iter()
        .map(
            |(
                id,
                filename,
                sha256,
                blake3,
                size_bytes,
                mime,
                duration_ms,
                width,
                height,
                vc,
                ac,
                cc,
            )| {
                let dimensions = match (width, height) {
                    (Some(w), Some(h)) if w > 0 && h > 0 => Some(Dimensions {
                        width: w as u32,
                        height: h as u32,
                    }),
                    _ => None,
                };
                let codecs = if vc.is_some() || ac.is_some() || cc.is_some() {
                    Some(Codecs {
                        video: vc,
                        audio: ac,
                        container: cc,
                    })
                } else {
                    None
                };
                MediaEntry {
                    id,
                    filename,
                    sha256: sha256.clone(),
                    blake3,
                    size_bytes: size_bytes as u64,
                    mime,
                    duration_ms: duration_ms as u64,
                    dimensions,
                    codecs,
                    sources: vec![Source {
                        peer_id: host_pubkey_b64.to_string(),
                        url_hint: None,
                        priority: 0,
                        chunk_size: DEFAULT_CHUNK_SIZE,
                        total_chunks: DEFAULT_TOTAL_CHUNKS,
                        chunk_hashes: vec![sha256],
                    }],
                }
            },
        )
        .collect();

    Ok(MediaManifest {
        manifest_version: 1,
        room_id: room_id.to_string(),
        media,
        subtitles: vec![],
        created_at: now_ms_i64(),
        host_signature: None,
    })
}

/// Sign the manifest with the local identity, build a
/// `MANIFEST_PUBLISH` envelope, and send it on the
/// signaling client. The function does NOT wait for an
/// ack from the server (the server does not send one in
/// v1 — it broadcasts `MANIFEST_PUBLISHED` to every
/// participant including the host).
pub async fn sign_and_publish(
    identity: &IdentityService,
    signaling: &SignalingClient,
    manifest: &MediaManifest,
) -> Result<(), HostError> {
    let signed = identity.sign_manifest(manifest).await?;
    let room_uuid = Uuid::parse_str(&signed.room_id)
        .map_err(|e| HostError::Signing(format!("bad room_id uuid: {e}")))?;
    let env = envelope(
        MessageKind::ManifestPublish,
        room_uuid,
        &ManifestPublishPayload { manifest: signed },
    );
    signaling.send_envelope(env).await?;
    Ok(())
}

fn envelope<T: Serialize>(kind: MessageKind, room_id: Uuid, payload: &T) -> Envelope {
    Envelope {
        v: 1,
        r#type: kind,
        id: Uuid::now_v7(),
        room_id: Some(room_id),
        sender: None,
        ts_ms: now_ms_i64(),
        seq: 0,
        payload: serde_json::to_value(payload).unwrap_or_else(|e| {
            warn!(error = %e, "failed to serialize MANIFEST_PUBLISH payload");
            serde_json::json!({})
        }),
    }
}

fn now_ms_i64() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// High-level entry point used by the `manifest_publish`
/// Tauri command. Looks up the host's pubkey from the
/// identity, builds the manifest, signs it, and sends it.
pub async fn build_sign_and_publish(
    identity: Arc<IdentityService>,
    signaling: Arc<SignalingClient>,
    storage_pool: SqlitePool,
    room_id: Uuid,
) -> Result<(), HostError> {
    let host_pubkey_b64 = {
        let kp = identity.load_keypair().await?;
        locast_crypto::ed25519::to_base64(&kp.signing.verifying_key().to_bytes())
    };
    let manifest = build_manifest(&storage_pool, room_id, &host_pubkey_b64).await?;
    sign_and_publish(&identity, &signaling, &manifest).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fresh_pool_with_one_row() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        sqlx::query(
            "CREATE TABLE media_items (
                id TEXT PRIMARY KEY,
                filename TEXT NOT NULL,
                sha256 TEXT NOT NULL,
                blake3 TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                mime TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                width INTEGER,
                height INTEGER,
                video_codec TEXT,
                audio_codec TEXT,
                container TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .expect("create table");
        sqlx::query(
            "INSERT INTO media_items (id, filename, sha256, blake3, size_bytes, mime, duration_ms, width, height, video_codec, audio_codec, container, status) \
             VALUES ('mid-1', 'movie.mp4', ?1, ?2, 1024, 'video/mp4', 60000, 1920, 1080, 'h264', 'aac', 'mp4', 'permanent')",
        )
        .bind("a".repeat(64))
        .bind("b".repeat(64))
        .execute(&pool)
        .await
        .expect("insert");
        pool
    }

    #[tokio::test]
    async fn build_manifest_reads_one_permanent_row() {
        let pool = fresh_pool_with_one_row().await;
        let room_id = Uuid::now_v7();
        let m = build_manifest(&pool, room_id, "host-b64")
            .await
            .expect("build");
        assert_eq!(m.media.len(), 1);
        assert_eq!(m.media[0].filename, "movie.mp4");
        assert_eq!(m.media[0].sources.len(), 1);
        assert_eq!(m.media[0].sources[0].peer_id, "host-b64");
        assert_eq!(m.media[0].sources[0].chunk_size, DEFAULT_CHUNK_SIZE);
        assert_eq!(m.media[0].sources[0].total_chunks, 1);
        assert_eq!(m.room_id, room_id.to_string());
        assert!(m.host_signature.is_none());
    }

    #[tokio::test]
    async fn build_manifest_with_no_permanent_rows_errors() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        sqlx::query(
            "CREATE TABLE media_items (
                id TEXT PRIMARY KEY,
                filename TEXT NOT NULL,
                sha256 TEXT NOT NULL,
                blake3 TEXT NOT NULL,
                size_bytes INTEGER NOT NULL,
                mime TEXT NOT NULL,
                duration_ms INTEGER NOT NULL,
                width INTEGER,
                height INTEGER,
                video_codec TEXT,
                audio_codec TEXT,
                container TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                created_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .expect("create");
        let room_id = Uuid::now_v7();
        let err = build_manifest(&pool, room_id, "host-b64")
            .await
            .expect_err("should error on empty library");
        assert!(matches!(err, HostError::NoMedia));
    }
}
