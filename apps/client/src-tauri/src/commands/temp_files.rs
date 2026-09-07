//! `commands::temp_files` - Tauri command surface for temp file operations.
//!
//! P6-T06 ships three commands:
//!
//! - `get_temp_files(room_id)` - fetch the list of temp files for a room.
//! - `mark_files_permanent(file_ids)` - mark temp files as permanent.
//! - `delete_files_to_trash(file_ids)` - move temp files to trash.
//!
//! All commands forward to the server via the signaling WebSocket.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State as TauriState;
use uuid::Uuid;

use crate::commands::error::AppError;
use crate::net::room::RoomClient;

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct TempFileInfo {
    pub file_id: String,
    pub room_id: String,
    pub filename: String,
    pub size_bytes: i64,
    pub created_ms: i64,
    pub owner_user_id: String,
}

#[tauri::command]
#[specta::specta]
pub async fn get_temp_files(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    room_id: String,
) -> Result<Vec<TempFileInfo>, AppError> {
    let _summary = room.state().await.ok_or_else(|| AppError::Other {
        message: "not in a room".to_string(),
    })?;
    let _rid = Uuid::parse_str(&room_id).map_err(|e| AppError::Other {
        message: format!("bad room_id: {e}"),
    })?;
    room.get_temp_files(_rid)
        .await
        .map_err(|e| AppError::Other {
            message: e.to_string(),
        })
}

#[tauri::command]
#[specta::specta]
pub async fn mark_files_permanent(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    file_ids: Vec<String>,
) -> Result<(), AppError> {
    let summary = room.state().await.ok_or_else(|| AppError::Other {
        message: "not in a room".to_string(),
    })?;
    let room_id = Uuid::parse_str(&summary.id).map_err(|e| AppError::Other {
        message: format!("bad cached room id: {e}"),
    })?;
    let file_uuid_ids: Result<Vec<Uuid>, _> = file_ids.iter().map(|s| Uuid::parse_str(s)).collect();
    let file_uuid_ids = file_uuid_ids.map_err(|e| AppError::Other {
        message: format!("bad file_id: {e}"),
    })?;
    room.mark_files_permanent(room_id, file_uuid_ids)
        .await
        .map_err(|e| AppError::Other {
            message: e.to_string(),
        })
}

#[tauri::command]
#[specta::specta]
pub async fn delete_files_to_trash(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    file_ids: Vec<String>,
) -> Result<(), AppError> {
    let summary = room.state().await.ok_or_else(|| AppError::Other {
        message: "not in a room".to_string(),
    })?;
    let room_id = Uuid::parse_str(&summary.id).map_err(|e| AppError::Other {
        message: format!("bad cached room id: {e}"),
    })?;
    let file_uuid_ids: Result<Vec<Uuid>, _> = file_ids.iter().map(|s| Uuid::parse_str(s)).collect();
    let file_uuid_ids = file_uuid_ids.map_err(|e| AppError::Other {
        message: format!("bad file_id: {e}"),
    })?;
    room.delete_files_to_trash(room_id, file_uuid_ids)
        .await
        .map_err(|e| AppError::Other {
            message: e.to_string(),
        })
}
