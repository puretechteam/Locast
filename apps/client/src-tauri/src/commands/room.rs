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

use crate::commands::error::AppError;
use crate::net::room::{RoomClient, RoomClientError, RoomSummaryIpc};
use crate::net::signaling::SignalingClient;
use crate::storage::rooms::{self, RecentRoomEntry};
use crate::storage::Storage;

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
#[tauri::command]
#[specta::specta]
pub async fn room_leave(room: TauriState<'_, RoomClient>) -> Result<(), AppError> {
    room.room_leave().await.map_err(room_err_to_app)
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
