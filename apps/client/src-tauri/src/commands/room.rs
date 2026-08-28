//! Tauri commands for the P2-T04 room lifecycle.
//!
//! Five commands:
//!
//! - `room_create`        - send `ROOM_CREATE`, return the summary.
//! - `room_join`          - send `ROOM_JOIN_REQUEST`, return the summary.
//! - `room_leave`         - send `ROOM_LEAVE`.
//! - `room_get_state`     - return the cached RoomSummary, if any.
//! - `room_connect_signaling` - idempotent; calls `signaling_connect`
//!   to ensure the WS is open before any room op.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use tauri::State as TauriState;

use crate::commands::error::AppError;
use crate::net::room::{RoomClient, RoomClientError, RoomSummaryIpc};
use crate::net::signaling::SignalingClient;

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

fn room_err_to_app(e: RoomClientError) -> AppError {
    AppError::other(e.to_string())
}
