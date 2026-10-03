//! Tauri commands for the P2-T04 room lifecycle.
//!
//! P2-T04 commands:
//!
//! - `room_create`        - send `ROOM_CREATE`, return the summary.
//! - `room_join`          - send `ROOM_JOIN_REQUEST`, return the summary.
//! - `room_leave`         - send `ROOM_LEAVE`.
//! - `room_get_state`     - return the cached RoomSummary, if any.
//! - `room_connect_signaling` - idempotent; calls `signaling_connect`
//!   to ensure the WS is open before any room op.
//!
//! P2-T08 commands:
//!
//! - `recent_rooms_list`  - read the local recents table for the
//!   `/rooms` page.
//! - `recent_room_upsert` - persist a recents row on every
//!   `room://state` event so the list survives restarts.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use tauri::State as TauriState;
use uuid::Uuid;

use crate::commands::error::AppError;
use crate::net::room::{RoomClient, RoomClientError, RoomSummaryIpc};
use crate::net::signaling::SignalingClient;
use crate::storage::rooms::{self, RecentRoomEntry};
use crate::storage::Storage;
use crate::transfer::registry::TransferRegistry;

/// Idempotent: ensure the signaling WS is open. Mirrors
/// `signaling_connect` for callers that prefer the
/// `room_*` naming family.
#[tauri::command]
#[specta::specta]
pub async fn room_connect_signaling(
    signaling: TauriState<'_, std::sync::Arc<SignalingClient>>,
) -> Result<(), AppError> {
    signaling
        .start()
        .await
        .map_err(|e| AppError::other(e.to_string()))
}

/// Create a new room. The caller picks the title and the
/// migration setting.
#[tauri::command]
#[specta::specta]
pub async fn room_create(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    title: String,
    migration_enabled: bool,
) -> Result<RoomSummaryIpc, AppError> {
    // A trust anchor or media selection left over from a
    // previous room must not leak into this one. The host
    // installs its own key on its first publish.
    room.clear_expected_host_pubkey();
    room.set_host_media_selection(None);
    room.room_create(title, migration_enabled)
        .await
        .map_err(room_err_to_app)
}

/// Join a room by 6-char code and display name.
///
/// `invite_url` is the host's `locast://join/<code>?h=<key>&v=1`
/// invite. When present it is parsed by the strict
/// [`crate::room::invite::parse_invite`], its room code must match
/// `code`, and its `h=` key becomes the manifest trust anchor
/// BEFORE the join is sent (so a manifest broadcast that races the
/// join reply is checked against it). Without an invite the anchor
/// is cleared: the viewer can sit in the room but every manifest is
/// rejected with `NoTrustAnchor`, so nothing is downloaded.
#[tauri::command]
#[specta::specta]
pub async fn room_join(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    code: String,
    display_name: String,
    invite_url: Option<String>,
) -> Result<RoomSummaryIpc, AppError> {
    let anchor = invite_anchor_for(&code, invite_url.as_deref())?;
    match anchor {
        Some(pk) => room.set_expected_host_pubkey(pk),
        None => room.clear_expected_host_pubkey(),
    }
    room.room_join(code, display_name)
        .await
        .map_err(room_err_to_app)
}

/// Parse an optional invite URL and check it belongs to `code`.
/// Returns the host pubkey to use as the manifest trust anchor.
pub fn invite_anchor_for(
    code: &str,
    invite_url: Option<&str>,
) -> Result<Option<[u8; 32]>, AppError> {
    let Some(url) = invite_url.map(str::trim).filter(|u| !u.is_empty()) else {
        return Ok(None);
    };
    let parsed = crate::room::invite::parse_invite(INVITE_SCHEME, url)
        .map_err(|e| AppError::other(format!("invalid invite link: {e}")))?;
    if !parsed.room_code.eq_ignore_ascii_case(code) {
        return Err(AppError::other(
            "invite link is for a different room code".to_string(),
        ));
    }
    Ok(Some(parsed.host_pubkey))
}

/// The URL scheme used for invite links.
const INVITE_SCHEME: &str = "locast";

/// Return the invite link for the current room. Host only: the
/// link carries the host's own public key as the viewers' trust
/// anchor, so only the host can vouch for it. A viewer gets an
/// error rather than a link built from a key it learned from the
/// server.
#[tauri::command]
#[specta::specta]
pub async fn room_invite_url(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    identity: TauriState<'_, std::sync::Arc<crate::identity::keystore::IdentityService>>,
) -> Result<String, AppError> {
    let summary = room
        .state()
        .await
        .ok_or_else(|| AppError::other("not in a room".to_string()))?;
    let local = room
        .local_user_id()
        .await
        .ok_or_else(|| AppError::other("not in a room".to_string()))?;
    if summary.host_user_id != local.to_string() {
        return Err(AppError::other(
            "only the host can share the invite link".to_string(),
        ));
    }
    let kp = identity
        .load_keypair()
        .await
        .map_err(|e| AppError::other(format!("identity: {e}")))?;
    crate::room::host::build_invite_url(
        INVITE_SCHEME,
        &summary.code,
        kp.signing.verifying_key().to_bytes(),
    )
    .map_err(|e| AppError::other(e.to_string()))
}

/// One shared media item, as the room UI needs it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct SharedMediaIpc {
    pub id: String,
    pub filename: String,
    pub size_bytes: u64,
    pub mime: String,
    pub sha256: String,
}

/// The verified manifest currently cached for the room.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct SharedManifestIpc {
    pub room_id: String,
    pub version: Option<i64>,
    pub media: Vec<SharedMediaIpc>,
}

/// Return the manifest the local client has ACCEPTED for the
/// current room (signature + trust anchor checked by
/// `RoomClient::accept_manifest`), or `None`. Read-only: it never
/// contacts the server and never bypasses verification, because
/// only verified manifests enter the cache.
#[tauri::command]
#[specta::specta]
pub async fn manifest_current(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
) -> Result<Option<SharedManifestIpc>, AppError> {
    let Some(summary) = room.state().await else {
        return Ok(None);
    };
    let room_id = Uuid::parse_str(&summary.id)
        .map_err(|e| AppError::other(format!("bad cached room id: {e}")))?;
    Ok(room.verified_manifest(room_id).map(|m| SharedManifestIpc {
        room_id: summary.id.clone(),
        version: room.verified_manifest_version(room_id),
        media: m
            .media
            .into_iter()
            .map(|e| SharedMediaIpc {
                id: e.id,
                filename: e.filename,
                size_bytes: e.size_bytes,
                mime: e.mime,
                sha256: e.sha256,
            })
            .collect(),
    }))
}

/// Leave the current room. The server broadcasts
/// ROOM_CLOSED / PARTICIPANT_LEFT in response; the webview
/// observes the cached state clear.
///
/// P3-T13 review fix H#28: also cancel every in-flight
/// transfer registered with the [`TransferRegistry`]. The v1
/// model is single-room-per-process: leaving the room
/// invalidates the file-transfer addressing, so any open
/// download becomes moot and should be torn down. Cancellation
/// happens AFTER the server confirms the leave so a slow
/// server does not orphan a successful transfer.
#[tauri::command]
#[specta::specta]
pub async fn room_leave(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    registry: TauriState<'_, std::sync::Arc<TransferRegistry>>,
) -> Result<(), AppError> {
    let res = room.room_leave().await.map_err(room_err_to_app);
    registry.cancel_all().await;
    res
}

/// Return the most recent cached room summary.
#[tauri::command]
#[specta::specta]
pub async fn room_get_state(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
) -> Result<Option<RoomSummaryIpc>, AppError> {
    Ok(room.state().await)
}

/// P2-T08: list the recents rooms for the `/rooms` page.
///
/// The list is ordered newest-activity first and capped at
/// `LIMIT` rows (100 in v1). The cap is a hard-coded IPC-level
/// constant; the user has no UI to override it in this phase.
#[tauri::command]
#[specta::specta]
pub async fn recent_rooms_list(
    storage: TauriState<'_, Storage>,
) -> Result<Vec<RecentRoomEntry>, AppError> {
    rooms::list_recent_rooms(&storage, 100)
        .await
        .map_err(AppError::from)
}

/// P2-T08: upsert a recents row. The React side calls this on every
/// `room://state` event (and on initial mount from the recents
/// table) so the list survives a restart.
///
/// `entry.last_ended_ms` is `Some` once the room has ended; on a
/// stale event arriving after end, the SQL `COALESCE` in
/// `storage::rooms::upsert_recent_room` keeps the prior non-null
/// end timestamp.
#[tauri::command]
#[specta::specta]
pub async fn recent_room_upsert(
    storage: TauriState<'_, Storage>,
    entry: RecentRoomEntry,
) -> Result<(), AppError> {
    rooms::upsert_recent_room(&storage, &entry)
        .await
        .map_err(AppError::from)
}

fn room_err_to_app(e: RoomClientError) -> AppError {
    AppError::other(e.to_string())
}

/// P3-T03: publish a signed `MediaManifest` to the current
/// room. The host must already be in a `Connected` room
/// state; the command reads the cached `room_id` from the
/// `RoomClient`, builds the manifest from the local
/// `media_items` table, signs it through the local
/// identity, and sends a `MANIFEST_PUBLISH` envelope over
/// the signaling client.
///
/// The server enforces the host-only capability. The
/// command itself does not need to check `is_host` because
/// the `RoomClient.state().host_user_id == identity.user_id`
/// invariant is maintained by the room-lifecycle commands;
/// if the host is wrong, the server returns a
/// P3-T04 prerequisite 3: fetch the room's current
/// manifest from the server. Used by late-joiners to
/// catch up on a manifest published before they joined.
/// The server returns the manifest with the per-room
/// `version` and `published_at_ms`; the caller (the
/// Tauri command's invocation) is expected to feed the
/// manifest into the local `RoomClient` so the
/// `MANIFEST_PUBLISHED` handler's TOFU check + persistence
/// path runs.
#[tauri::command]
#[specta::specta]
pub async fn manifest_fetch(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    media_id: Uuid,
) -> Result<locast_protocol::room::ManifestResponsePayload, AppError> {
    let summary = room
        .state()
        .await
        .ok_or_else(|| AppError::other("not in a room".to_string()))?;
    let room_id = Uuid::parse_str(&summary.id)
        .map_err(|e| AppError::other(format!("bad cached room id: {e}")))?;
    room.manifest_fetch(room_id, media_id)
        .await
        .map_err(|e| AppError::other(e.to_string()))
}

/// `ROOM_ERROR(NotHost)`.
#[tauri::command]
#[specta::specta]
pub async fn manifest_publish(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    identity: TauriState<'_, std::sync::Arc<crate::identity::keystore::IdentityService>>,
    signaling: TauriState<'_, std::sync::Arc<SignalingClient>>,
    storage: TauriState<'_, Storage>,
    media_ids: Option<Vec<String>>,
) -> Result<(), AppError> {
    if matches!(&media_ids, Some(ids) if ids.is_empty()) {
        return Err(AppError::other(
            "select at least one media item to share".to_string(),
        ));
    }
    let summary = room
        .state()
        .await
        .ok_or_else(|| AppError::other("not in a room".to_string()))?;
    let room_id = uuid::Uuid::parse_str(&summary.id)
        .map_err(|e| AppError::other(format!("bad cached room id: {e}")))?;
    // The library root is the parent of the storage file
    // (per the architecture's `<library_root>/library/...`
    // layout). The chunk planner needs it to read the
    // on-disk media file for `Source::chunk_hashes`.
    let library_root = crate::core::paths::library_root_for(storage.path())
        .ok_or_else(|| AppError::other("library root has no parent".to_string()))?;
    crate::room::host::build_sign_and_publish_selected(
        identity.inner().clone(),
        signaling.inner().clone(),
        room.inner().clone(),
        storage.pool(),
        library_root,
        room_id,
        media_ids,
    )
    .await
    .map_err(|e| AppError::other(e.to_string()))
}

/// P6-T02: grant or revoke capabilities for a participant.
/// The caller must be the room host; the server enforces
/// this. Sends a `PERMISSION_SET` envelope and waits for
/// a `CAPABILITY_UPDATE` broadcast confirmation.
#[tauri::command]
#[specta::specta]
pub async fn room_permission_set(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    target_user_id: String,
    add_cap_set: u32,
    remove_cap_set: u32,
) -> Result<(), AppError> {
    let summary = room
        .state()
        .await
        .ok_or_else(|| AppError::other("not in a room".to_string()))?;
    let room_id = Uuid::parse_str(&summary.id)
        .map_err(|e| AppError::other(format!("bad cached room id: {e}")))?;
    let target_uuid = Uuid::parse_str(&target_user_id)
        .map_err(|e| AppError::other(format!("bad target user_id: {e}")))?;
    room.permission_set(room_id, target_uuid, add_cap_set, remove_cap_set)
        .await
        .map_err(|e| AppError::other(e.to_string()))
}

/// P6-T03: send a chat message. The server validates the
/// caller's CHAT capability and broadcasts to all participants.
#[tauri::command]
#[specta::specta]
pub async fn room_chat_message(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    text: String,
    reply_to: Option<String>,
) -> Result<(), AppError> {
    let summary = room
        .state()
        .await
        .ok_or_else(|| AppError::other("not in a room".to_string()))?;
    let room_id = Uuid::parse_str(&summary.id)
        .map_err(|e| AppError::other(format!("bad cached room id: {e}")))?;
    let reply_to_uuid = reply_to
        .map(|s| Uuid::parse_str(&s))
        .transpose()
        .map_err(|e| AppError::other(format!("bad reply_to user_id: {e}")))?;
    room.chat_message(room_id, text, reply_to_uuid)
        .await
        .map_err(|e| AppError::other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::invite_anchor_for;

    const PK: [u8; 32] = [7u8; 32];

    fn invite(code: &str) -> String {
        crate::room::host::build_invite_url("locast", code, PK).expect("url")
    }

    #[test]
    fn no_invite_means_no_anchor() {
        assert_eq!(invite_anchor_for("ABCDEF", None).unwrap(), None);
        assert_eq!(invite_anchor_for("ABCDEF", Some("  ")).unwrap(), None);
    }

    #[test]
    fn matching_invite_yields_the_host_key() {
        let url = invite("ABCDEF");
        assert_eq!(invite_anchor_for("ABCDEF", Some(&url)).unwrap(), Some(PK));
        assert_eq!(invite_anchor_for("abcdef", Some(&url)).unwrap(), Some(PK));
    }

    #[test]
    fn invite_for_another_room_is_rejected() {
        let url = invite("ABCDEF");
        assert!(invite_anchor_for("ZZZZZZ", Some(&url)).is_err());
    }

    #[test]
    fn malformed_invite_is_rejected() {
        assert!(invite_anchor_for("ABCDEF", Some("locast://join/ABCDEF")).is_err());
        assert!(
            invite_anchor_for("ABCDEF", Some("https://evil.example/join/ABCDEF?h=AAAA")).is_err()
        );
        assert!(invite_anchor_for("ABCDEF", Some("locast://join/ABCDEF?h=AAAA&v=1")).is_err());
    }
}
