//! Server-side manifest publication handler (P3-T03).
//!
//! `MANIFEST_PUBLISH` (host -> server -> all participants)
//! is the wire path that delivers a signed
//! [`locast_manifest::MediaManifest`] to the rest of the
//! room. The server is the relay; it does NOT become the
//! signer and it does NOT trust the host's signature
//! cryptographically (the viewer's TOFU check against the
//! invite's `h=` parameter is the trust boundary). What the
//! server DOES do is:
//!
//! 1. Verify the caller is the current host of the room
//!    named in `envelope.room_id` (the capability gate does
//!    this; if it returns `CapsError::NotHost`, the dispatch
//!    layer surfaces `ROOM_ERROR(NotHost)` to the caller
//!    and skips the handler).
//! 2. Run a defense-in-depth
//!    [`locast_manifest::verify_manifest`] on the supplied
//!    manifest. A failure here indicates the host's signing
//!    pipeline is broken or the message was tampered with;
//!    either way, the relay must NOT broadcast a manifest
//!    it cannot verify.
//! 3. Persist the manifest as a new `room_manifests` row
//!    (P3-T07 will read this back for the room
//!    resume path; the durable table is also the audit log).
//! 4. Emit a `RoomEvent::ManifestPublished` to the room's
//!    broadcast channel and update the in-memory cache.
//!
//! The host itself also gets the broadcast (the relay
//! does not filter on the originator for `MANIFEST_PUBLISHED`).
//! That way the host's React layer can use the same
//! `manifest://state` event as a viewer to render the
//! post-publish state. The direct caller does NOT get a
//! separate `MANIFEST_PUBLISHED` in `to_caller`; the spec
//! keeps the wire to one path per publish.
//!
//! Per docs/ARCHITECTURE.md section 8, the manifest is
//! signed over the canonical bytes; the server does NOT
//! re-canonicalize the manifest beyond what
//! `locast_manifest::verify_manifest` does internally
//! (which is the same path the viewer runs).
//!
//! ## Error contract
//!
//! - The caller-facing error shape is `ROOM_ERROR` with one
//!   of: `InvalidState` (bad payload / verify failed),
//!   `NotHost` (capability gate; emitted by the dispatch
//!   layer, not this function), `NotJoined` (not in a room),
//!   `Internal` (DB write failed).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::time::{SystemTime, UNIX_EPOCH};

use locast_manifest::MediaManifest;
use locast_protocol::envelope::MessageKind;
use locast_protocol::room::{
    ManifestPublishPayload, ManifestPublishedPayload, RoomErrorCode, RoomErrorPayload,
};
use uuid::Uuid;

use super::caps;
use super::error::RoomError;
use super::registry::{CachedManifest, RoomEvent, RoomRegistry};
use crate::db::Db;
use crate::time::Clock;

/// Run the manifest publish flow. The caller (the dispatch
/// layer) is responsible for the bearer / rate-limit /
/// capability gate; this function only runs after those
/// have passed.
///
/// The function:
/// 1. Decodes the [`ManifestPublishPayload`].
/// 2. Verifies the manifest signature as defense in depth.
/// 3. INSERTs a fresh `room_manifests` row with
///    `version = max(existing) + 1`.
/// 4. Updates the registry's in-memory manifest cache.
/// 5. Returns the [`RoomEvent::ManifestPublished`] to be
///    published to the room's broadcast channel.
///
/// The function does NOT directly send anything on the
/// WS — it returns a [`RoomEvent`] and the dispatch
/// layer is responsible for the broadcast. This keeps
/// the registry free of transport types.
pub async fn handle_manifest_publish(
    envelope: &locast_protocol::envelope::Envelope,
    registry: &RoomRegistry,
    db: &Db,
    clock: &dyn Clock,
    user_id: Uuid,
) -> Result<RoomEvent, RoomError> {
    // 1. Decode the payload. The dispatch layer strips the
    //    `bearer` field; we deserialize the cleaned value
    //    straight into the typed struct.
    let payload_value = envelope.payload.clone();
    let payload: ManifestPublishPayload = match serde_json::from_value(payload_value) {
        Ok(p) => p,
        Err(_e) => {
            return Err(RoomError::InvalidState);
        }
    };
    let manifest = payload.manifest;
    let room_id = envelope.room_id.ok_or(RoomError::InvalidState)?;

    // 2. Verify the manifest signature. The viewer's TOFU
    //    check is the trust boundary; this is defense in
    //    depth. A failure here is a caller bug (the host
    //    signed locally and somehow produced a manifest
    //    that does not verify over its own canonical
    //    bytes) or a tamper.
    if let Err(_e) = locast_manifest::verify_manifest(&manifest) {
        return Err(RoomError::InvalidState);
    }

    // 3. Compute the next version. The room row exists
    //    (we checked `is_room_host` earlier) but the
    //    manifest may be the first one published.
    let now = clock.now_ms();
    let (version, manifest_hash) = compute_version_and_hash(&manifest, db, room_id).await?;

    // 4. Persist.
    let manifest_json = serde_json::to_string(&manifest)
        .map_err(|e| RoomError::Internal(format!("serialize manifest for insert: {e}")))?;
    let row_id = Uuid::now_v7();
    db.insert_room_manifest(
        row_id,
        room_id,
        version,
        now,
        &manifest_json,
        &manifest_hash,
        user_id,
    )
    .await
    .map_err(|e| RoomError::Internal(format!("insert_room_manifest: {e}")))?;

    // 5. Update the in-memory cache.
    registry
        .put_current_manifest(
            room_id,
            CachedManifest {
                version,
                manifest: manifest.clone(),
                host_user_id: user_id,
                published_at_ms: now,
                manifest_hash,
            },
        )
        .await;

    // 6. Build the event.
    Ok(RoomEvent::ManifestPublished {
        room_id,
        manifest,
        version,
        published_at_ms: now,
    })
}

/// Handle a `MANIFEST_REQUEST` from a room member. Returns
/// the room's currently-authoritative `CachedManifest` so
/// the caller (the dispatch layer) can serialize it as a
/// `MANIFEST_RESPONSE` envelope and send it back to the
/// requesting connection only.
///
/// Authorization is the caller's responsibility: the
/// `caps::check_capability(Command::FetchManifest, ...)` call
/// in `dispatch_room_message` must have already returned
/// `Ok(())` for this function to be reached. That check
/// confirms the caller is a participant of the named
/// room; it does NOT re-check the room's lifecycle state
/// (the cached manifest is authoritative for the room's
/// lifetime, so an "ended" room cannot return one even
/// if a stale connection is still around).
pub async fn handle_manifest_fetch(
    envelope: &locast_protocol::envelope::Envelope,
    registry: &RoomRegistry,
) -> Result<locast_protocol::room::ManifestResponsePayload, RoomError> {
    // Decode the request payload (the dispatch layer has
    // already stripped the bearer). The `media_id` field
    // is informational; we always return the latest
    // authoritative manifest.
    let _payload: locast_protocol::room::ManifestRequestPayload =
        serde_json::from_value(envelope.payload.clone()).map_err(|_| RoomError::InvalidState)?;
    let room_id = envelope.room_id.ok_or(RoomError::InvalidState)?;
    let cached = registry
        .current_manifest(room_id)
        .await
        .ok_or(RoomError::InvalidState)?;
    Ok(locast_protocol::room::ManifestResponsePayload {
        manifest: cached.manifest,
        version: cached.version,
        published_at_ms: cached.published_at_ms,
    })
}

/// Compute the next per-room manifest version and the
/// BLAKE3 of the canonical bytes. The version is
/// `max(existing) + 1` if any row exists for the room,
/// else `1`. The BLAKE3 is the host's canonical-bytes
/// commit (so the cache and the durable row agree on
/// "what was signed").
async fn compute_version_and_hash(
    manifest: &MediaManifest,
    db: &Db,
    room_id: Uuid,
) -> Result<(i64, [u8; 32]), RoomError> {
    let canonical = locast_manifest::serialize(manifest)
        .map_err(|e| RoomError::Internal(format!("canonicalize: {e}")))?;
    let hex = locast_crypto::blake3::blake3_hex(&canonical);
    let hash = hex_to_bytes32(&hex)
        .ok_or_else(|| RoomError::Internal(format!("blake3_hex produced wrong length: {hex}")))?;
    let version = match db
        .get_latest_room_manifest(room_id)
        .await
        .map_err(|e| RoomError::Internal(format!("get_latest_room_manifest: {e}")))?
    {
        Some(latest) => latest.version + 1,
        None => 1,
    };
    Ok((version, hash))
}

fn hex_to_bytes32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = s.as_bytes();
    for i in 0..32 {
        let hi = hex_nibble(bytes[2 * i])?;
        let lo = hex_nibble(bytes[2 * i + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Build a wire envelope for a direct caller reply (e.g.
/// a `MANIFEST_PUBLISHED` confirmation if we ever decide
/// to add one; the current spec does not). Kept here so
/// the dispatch layer has a single place to build
/// manifest-shaped envelopes.
#[allow(dead_code)]
pub fn manifest_published_envelope(
    room_id: Uuid,
    payload: &ManifestPublishedPayload,
) -> locast_protocol::envelope::Envelope {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    locast_protocol::envelope::Envelope {
        v: 1,
        r#type: MessageKind::ManifestPublished,
        id: Uuid::now_v7(),
        room_id: Some(room_id),
        sender: None,
        ts_ms: now,
        seq: 0,
        payload: serde_json::to_value(payload).unwrap_or(serde_json::json!({})),
    }
}

/// Build a `ROOM_ERROR` envelope payload for the
/// caller-facing error path.
#[allow(dead_code)]
pub fn error_envelope(code: RoomErrorCode, message: impl Into<String>) -> RoomErrorPayload {
    RoomErrorPayload {
        code,
        message: message.into(),
    }
}

/// Helper that lets the static `caps::check_capability` be
/// re-exported for use in unit tests; not used in the
/// publish path.
#[allow(dead_code)]
pub fn caps_module() -> caps::CapsError {
    caps::CapsError::NotHost
}
