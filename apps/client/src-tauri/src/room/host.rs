//! `room::host` - the host-side manifest publication path.
//!
//! P3-T03 added the basic `build_manifest` + `sign_and_publish`
//! flow with a placeholder single-chunk manifest.
//!
//! P3-T04 prerequisites close:
//!
//! - The `Source::chunk_size` / `total_chunks` / `chunk_hashes[]`
//!   fields now come from the real file-streaming chunk
//!   planner ([`crate::room::chunk_plan::plan_file`]), not
//!   from a placeholder stub.
//! - The `Source::peer_id` is the canonical
//!   `sha256(public_key) hex` form (see
//!   [`crate::room::peer_id::derive_peer_id`]). The host
//!   passes the raw 32-byte pubkey (not the base64 string)
//!   and the manifest builder derives the canonical form.
//! - The host also emits a `locast://join/<code>?h=<...>`
//!   invite URL via [`build_invite_url`] so the viewer can
//!   thread the trusted host pubkey into the trust check.
//!
//! The host's `MANIFEST_PUBLISH` envelope shape and the
//! signing path are unchanged from P3-T03.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::Path;
use std::sync::Arc;

use locast_manifest::{MediaEntry, MediaManifest, Source};
use locast_protocol::envelope::{Envelope, MessageKind};
use locast_protocol::room::ManifestPublishPayload;
use serde::Serialize;
use sqlx::SqlitePool;
use thiserror::Error;
use tracing::warn;
use uuid::Uuid;

use crate::core::hashing::CHUNK_SIZE;
use crate::identity::keystore::{IdentityService, IdentityServiceError};
use crate::net::room::RoomClient;
use crate::net::signaling::{SignalingClient, SignalingError};
use crate::room::chunk_plan::{self, ChunkPlan, ChunkPlanError};
use crate::room::peer_id::derive_peer_id;

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
    /// The chunk planner could not process a media file
    /// (file missing, IO error, empty file, etc.).
    #[error("chunk planning failed for {filename}: {source}")]
    ChunkPlan {
        filename: String,
        #[source]
        source: ChunkPlanError,
    },
    /// The invite URL could not be built (e.g. the room
    /// code is malformed).
    #[error("invite url: {0}")]
    InviteUrl(String),
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

impl From<uuid::Error> for HostError {
    fn from(e: uuid::Error) -> Self {
        HostError::InviteUrl(e.to_string())
    }
}

/// One `permanent` row from the local `media_items` table,
/// in the order selected by `build_manifest`. The tuple
/// type is local to this module to keep clippy's
/// `type_complexity` lint happy.
type MediaItemRow = (
    String,         // id
    String,         // filename
    String,         // sha256 (full-file, from the scanner)
    String,         // blake3 (full-file, from the scanner)
    String,         // relative_path
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
/// pubkey (raw 32 bytes) is used as the single
/// `Source::peer_id` (canonical form:
/// `sha256(public_key) hex`). The `Source` per entry uses
/// the REAL per-file chunk plan (256 KiB chunks,
/// `total_chunks = ceil(size / 256 KiB)`,
/// `chunk_hashes[] = <one SHA-256 per chunk>`).
///
/// # Arguments
///
/// - `pool`: the local SQLite pool.
/// - `library_root`: the absolute path of the library root
///   (the same path passed to `library_scan`). Used to
///   resolve `media_items.relative_path` to an absolute
///   path on disk so the chunk planner can read the file.
/// - `room_id`: the current room id.
/// - `host_pubkey`: the host's 32-byte Ed25519 verifying
///   key. Passed as raw bytes (not base64) so the
///   canonical `peer_id` derivation is local and stable.
pub async fn build_manifest(
    pool: &SqlitePool,
    library_root: &Path,
    room_id: Uuid,
    host_pubkey: [u8; 32],
) -> Result<MediaManifest, HostError> {
    let rows: Vec<MediaItemRow> = sqlx::query_as(
        "SELECT id, filename, sha256, blake3, relative_path, size_bytes, mime, duration_ms, \
                width, height, video_codec, audio_codec, container \
         FROM media_items WHERE status = 'permanent' \
         ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Err(HostError::NoMedia);
    }

    let peer_id = derive_peer_id(host_pubkey);

    let mut media: Vec<MediaEntry> = Vec::with_capacity(rows.len());
    for (
        id,
        filename,
        _scanner_sha256,
        _scanner_blake3,
        relative_path,
        size_bytes,
        mime,
        duration_ms,
        width,
        height,
        vc,
        ac,
        cc,
    ) in rows
    {
        // Resolve the absolute path of the file on disk.
        // `relative_path` is library-relative (see
        // `library::scan::process_file` which sets it via
        // `relative_path_from`).
        let abs_path = library_root.join(&relative_path);
        let plan: ChunkPlan =
            chunk_plan::plan_file(&abs_path)
                .await
                .map_err(|e| HostError::ChunkPlan {
                    filename: filename.clone(),
                    source: e,
                })?;

        let dimensions = match (width, height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => Some(locast_manifest::Dimensions {
                width: w as u32,
                height: h as u32,
            }),
            _ => None,
        };
        let codecs = if vc.is_some() || ac.is_some() || cc.is_some() {
            Some(locast_manifest::Codecs {
                video: vc,
                audio: ac,
                container: cc,
            })
        } else {
            None
        };

        let total_chunks = plan.total_chunks();
        let full_sha = plan.full_sha256;
        let full_blake = plan.full_blake3;
        let per_chunk_sha = plan.per_chunk_sha256;

        media.push(MediaEntry {
            id,
            filename,
            sha256: full_sha,
            blake3: full_blake,
            size_bytes: size_bytes as u64,
            mime,
            duration_ms: duration_ms as u64,
            dimensions,
            codecs,
            sources: vec![Source {
                peer_id: peer_id.clone(),
                url_hint: None,
                priority: 0,
                chunk_size: CHUNK_SIZE as u32,
                total_chunks,
                chunk_hashes: per_chunk_sha,
            }],
        });
    }

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

/// Build the host's invite URL of the form:
///
/// ```text
/// locast://join/<room_code>?h=<base64url-no-pad-of-host-pubkey>&v=1
/// ```
///
/// (Architecture §8 line 795.) The `h=` parameter is the
/// host's raw 32-byte Ed25519 verifying key, encoded with
/// base64-URL **without** padding. The viewer decodes `h=`
/// to raw bytes and uses it as the trust anchor (P3-T04
/// prerequisite 2).
///
/// This function does NOT take a room code from the
/// server (it is computed elsewhere); it produces the
/// final `h=` query-string value for any host pubkey.
pub fn build_invite_h_param(host_pubkey: [u8; 32]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(host_pubkey)
}

/// Build a full `locast://join/<code>?h=<...>&v=1` URL.
pub fn build_invite_url(
    scheme: &str,
    room_code: &str,
    host_pubkey: [u8; 32],
) -> Result<String, HostError> {
    if room_code.is_empty() {
        return Err(HostError::InviteUrl("empty room code".into()));
    }
    Ok(format!(
        "{scheme}://join/{code}?h={h}&v=1",
        scheme = scheme,
        code = room_code,
        h = build_invite_h_param(host_pubkey)
    ))
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
///
/// P3-T15: also seeds the local `RoomClient`'s
/// `expected_host_pubkey` (so the host's own manifest
/// passes the TOFU check on the late-join path) and
/// populates the `verified_manifests` cache via
/// `accept_manifest`, so the host-side sender dispatch
/// can read it without waiting for the server's broadcast
/// (which the server may or may not echo to the host).
#[allow(clippy::too_many_arguments)]
pub async fn build_sign_and_publish(
    identity: Arc<IdentityService>,
    signaling: Arc<SignalingClient>,
    room_client: Arc<RoomClient>,
    storage_pool: SqlitePool,
    library_root: std::path::PathBuf,
    room_id: Uuid,
) -> Result<(), HostError> {
    let host_pubkey = {
        let kp = identity.load_keypair().await?;
        kp.signing.verifying_key().to_bytes()
    };
    let manifest = build_manifest(&storage_pool, &library_root, room_id, host_pubkey).await?;
    sign_and_publish(&identity, &signaling, &manifest).await?;
    // P3-T15: the host is its own trust anchor. Install
    // the local pubkey as the expected host pubkey, then
    // accept our own manifest into the verified cache.
    // Without this, the host dispatch's
    // `room.verified_manifest(room_id)` returns `None`
    // and the sender cannot serve chunks.
    room_client.set_expected_host_pubkey(host_pubkey);
    if let Err(e) = room_client
        .accept_manifest(manifest.clone(), 1, manifest.created_at, "LOCAL_PUBLISH")
        .await
    {
        warn!(
            error = ?e,
            "host: failed to accept own manifest into RoomClient cache; sender dispatch will be unable to serve chunks"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;
    use std::io::Write as _;
    use tempfile::TempDir;

    /// Create an in-memory `media_items` table with the
    /// real schema (minus optional columns) and insert one
    /// `permanent` row pointing at the given on-disk file.
    async fn fresh_pool_with_row(
        filename: &str,
        rel_path: &str,
        size_bytes: i64,
        sha256: &str,
        blake3: &str,
    ) -> SqlitePool {
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
                relative_path TEXT NOT NULL,
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
            "INSERT INTO media_items (id, filename, sha256, blake3, relative_path, size_bytes, mime, duration_ms, width, height, video_codec, audio_codec, container, status) \
             VALUES ('mid-1', ?1, ?2, ?3, ?4, ?5, 'video/mp4', 60000, 1920, 1080, 'h264', 'aac', 'mp4', 'permanent')",
        )
        .bind(filename)
        .bind(sha256)
        .bind(blake3)
        .bind(rel_path)
        .bind(size_bytes)
        .execute(&pool)
        .await
        .expect("insert");
        pool
    }

    /// Write a deterministic temp file of `len` bytes.
    fn write_temp_file(dir: &TempDir, rel_path: &str, len: usize) {
        let full = dir.path().join(rel_path);
        std::fs::create_dir_all(full.parent().unwrap()).expect("mkdir");
        let mut f = std::fs::File::create(&full).expect("create");
        let mut written = 0;
        while written < len {
            let take = (len - written).min(4096);
            let mut chunk = vec![0u8; take];
            for (i, b) in chunk.iter_mut().enumerate() {
                *b = ((written + i) as u8).wrapping_mul(31).wrapping_add(7);
            }
            f.write_all(&chunk).expect("write");
            written += take;
        }
        f.flush().expect("flush");
    }

    /// RFC 8032 test 1 pubkey (used as a stable "host
    /// pubkey" in tests; the exact bytes do not matter
    /// for these tests).
    const TEST_PUBKEY: [u8; 32] = [
        0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07,
        0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07,
        0x51, 0x1a,
    ];

    #[tokio::test]
    async fn build_manifest_uses_real_chunk_plan() {
        // 600 KiB of deterministic data -> 2 full chunks
        // + 1 tail. The host's build_manifest should
        // produce 3 per-chunk SHA-256s and the real full-
        // file BLAKE3.
        let dir = TempDir::new().expect("tempdir");
        let rel_path = "library/ab/abcd1234/movie.mp4";
        let len = 600 * 1024;
        write_temp_file(&dir, rel_path, len);

        let pool = fresh_pool_with_row(
            "movie.mp4",
            rel_path,
            len as i64,
            "00".repeat(32).as_str(),
            "11".repeat(32).as_str(),
        )
        .await;

        let room_id = Uuid::now_v7();
        let m = build_manifest(&pool, dir.path(), room_id, TEST_PUBKEY)
            .await
            .expect("build");
        assert_eq!(m.media.len(), 1);
        let src = &m.media[0].sources[0];
        assert_eq!(src.chunk_size, CHUNK_SIZE as u32);
        assert_eq!(src.total_chunks, 3);
        assert_eq!(src.chunk_hashes.len(), 3);
        // peer_id is the canonical sha256(public_key) hex.
        assert_eq!(src.peer_id, derive_peer_id(TEST_PUBKEY));
        assert_eq!(src.peer_id.len(), 64);
    }

    #[tokio::test]
    async fn build_manifest_small_file_one_chunk() {
        let dir = TempDir::new().expect("tempdir");
        let rel_path = "library/ab/abcdef01/small.mp4";
        let len = 1024;
        write_temp_file(&dir, rel_path, len);

        let pool = fresh_pool_with_row(
            "small.mp4",
            rel_path,
            len as i64,
            "00".repeat(32).as_str(),
            "11".repeat(32).as_str(),
        )
        .await;
        let m = build_manifest(&pool, dir.path(), Uuid::now_v7(), TEST_PUBKEY)
            .await
            .expect("build");
        let src = &m.media[0].sources[0];
        assert_eq!(src.total_chunks, 1);
        assert_eq!(src.chunk_hashes.len(), 1);
    }

    #[tokio::test]
    async fn build_manifest_chunk_hashes_match_independent_sha256() {
        // 600 KiB + 17 bytes -> 3 chunks: two full 256
        // KiB chunks and one trailing ~88 KiB + 17 chunk.
        let dir = TempDir::new().expect("tempdir");
        let rel_path = "library/ab/abcdef02/movie.bin";
        let len: usize = 600 * 1024 + 17;
        write_temp_file(&dir, rel_path, len);

        let pool = fresh_pool_with_row(
            "movie.bin",
            rel_path,
            len as i64,
            "00".repeat(32).as_str(),
            "11".repeat(32).as_str(),
        )
        .await;

        let m = build_manifest(&pool, dir.path(), Uuid::now_v7(), TEST_PUBKEY)
            .await
            .expect("build");
        let src = &m.media[0].sources[0];
        assert_eq!(src.chunk_size, CHUNK_SIZE as u32);
        assert_eq!(src.total_chunks as usize, len.div_ceil(CHUNK_SIZE));
        assert_eq!(src.chunk_hashes.len(), src.total_chunks as usize);

        // Independently re-read the file and recompute
        // per-chunk SHA-256.
        let abs = dir.path().join(rel_path);
        let bytes = std::fs::read(&abs).expect("read fixture");
        let mut expected_chunk = Vec::with_capacity(src.chunk_hashes.len());
        for chunk_start in (0..bytes.len()).step_by(CHUNK_SIZE) {
            let end = (chunk_start + CHUNK_SIZE).min(bytes.len());
            let digest = locast_crypto::sha256::sha256_hex(&bytes[chunk_start..end]);
            expected_chunk.push(digest);
        }
        assert_eq!(src.chunk_hashes, expected_chunk);

        // Full-file BLAKE3 of the manifest matches
        // independent BLAKE3 of the file bytes.
        let expected_blake3 = locast_crypto::blake3::blake3_hex(&bytes);
        assert_eq!(m.media[0].blake3, expected_blake3);

        // Manifest size_bytes agrees with the file.
        assert_eq!(m.media[0].size_bytes, len as u64);
    }

    #[tokio::test]
    async fn build_manifest_empty_library_errors() {
        let dir = TempDir::new().expect("tempdir");
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
                relative_path TEXT NOT NULL,
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
        let err = build_manifest(&pool, dir.path(), Uuid::now_v7(), TEST_PUBKEY)
            .await
            .expect_err("empty library must reject");
        assert!(matches!(err, HostError::NoMedia));
    }

    #[test]
    fn invite_h_param_is_base64url_no_pad() {
        let h = build_invite_h_param(TEST_PUBKEY);
        // 32 bytes -> ceil(32/3)*4 = 44 chars, NO padding
        // (= is the base64-standard padding char; URL-safe
        // no-pad does not emit it).
        assert_eq!(h.len(), 43);
        assert!(!h.contains('='), "no-pad encoding must not include '='");
        assert!(!h.contains('+'), "URL-safe encoding must not include '+'");
        assert!(!h.contains('/'), "URL-safe encoding must not include '/'");
    }

    #[test]
    fn invite_url_has_expected_shape() {
        let url = build_invite_url("locast", "AAAAAA", TEST_PUBKEY).expect("url");
        assert!(url.starts_with("locast://join/AAAAAA?h="));
        assert!(url.ends_with("&v=1"));
    }

    #[test]
    fn invite_url_rejects_empty_code() {
        let err = build_invite_url("locast", "", TEST_PUBKEY).expect_err("empty code");
        assert!(matches!(err, HostError::InviteUrl(_)));
    }
}
