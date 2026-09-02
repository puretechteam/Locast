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
    signaling: TauriState<'_, SignalingClient>,
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
    room: TauriState<'_, RoomClient>,
    title: String,
    migration_enabled: bool,
) -> Result<RoomSummaryIpc, AppError> {
    room.room_create(title, migration_enabled)
        .await
        .map_err(room_err_to_app)
}

/// Join a room by 6-char code and display name.
#[tauri::command]
#[specta::specta]
pub async fn room_join(
    room: TauriState<'_, RoomClient>,
    code: String,
    display_name: String,
) -> Result<RoomSummaryIpc, AppError> {
    room.room_join(code, display_name)
        .await
        .map_err(room_err_to_app)
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
    room: TauriState<'_, RoomClient>,
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
    room: TauriState<'_, RoomClient>,
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
) -> Result<(), AppError> {
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
    crate::room::host::build_sign_and_publish(
        identity.inner().clone(),
        signaling.inner().clone(),
        room.inner().clone(),
        storage.pool(),
        library_root,
        room_id,
    )
    .await
    .map_err(|e| AppError::other(e.to_string()))
}
