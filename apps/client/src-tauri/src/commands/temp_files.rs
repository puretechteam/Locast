//! `commands::temp_files` - Tauri command surface for the leave-room
//! "Keep / Delete" flow.
//!
//! P6-T06 ships three commands:
//!
//! - `get_temp_files(room_id)` - list the room's temporary media.
//! - `mark_files_permanent(file_ids)` - Keep: mark that media permanent.
//! - `delete_files_to_trash(file_ids)` - Delete: move that media into the
//!   library trash (`<library_root>/trash/`).
//!
//! "Room-temporary media" is a `media_items` row whose `status` is
//! `temporary` and which the local user finished downloading in that room
//! (a `downloads` row with `state = 'complete'` and the room's id). A
//! file id is the `media_items.id`. Mark / delete only ever touch rows that
//! match that definition for the caller's current room: unknown ids,
//! permanent items, and items from other rooms are skipped, never changed.
//!
//! Everything is local: the work reuses [`crate::commands::library`]'s
//! `make_permanent` and `delete_item`, so Keep and Delete have exactly the
//! same storage semantics as the Library page's "Make permanent" and
//! "Delete from library". The commands still require the caller to be in a
//! room.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use specta::Type;
use sqlx::Row;
use tauri::State as TauriState;
use uuid::Uuid;

use crate::commands::error::AppError;
use crate::commands::library::{delete_item, make_permanent};
use crate::net::room::RoomClient;
use crate::storage::Storage;

/// One room-temporary media item shown in the leave-room modal.
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct TempFileInfo {
    /// `media_items.id`.
    pub file_id: String,
    pub room_id: String,
    pub filename: String,
    pub size_bytes: i64,
    /// `media_items.created_at` (unix milliseconds).
    pub created_ms: i64,
    /// The local user that downloaded the item (`downloads.user_id`).
    pub owner_user_id: String,
}

/// Library root for a storage handle: the parent of the SQLite file, the
/// same rule `library_delete` uses.
fn library_root_of(storage: &Storage) -> Result<PathBuf, AppError> {
    storage
        .path()
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| AppError::InvalidPath {
            path: storage.path().to_string_lossy().into_owned(),
            message: "storage path has no parent".to_string(),
        })
}

/// List the temporary media the local user finished downloading in
/// `room_id`, oldest first.
pub async fn list_room_temp_files(
    storage: &Storage,
    room_id: &str,
) -> Result<Vec<TempFileInfo>, AppError> {
    let rows = sqlx::query(
        "SELECT m.id AS id, d.room_id AS room_id, m.filename AS filename, \
                m.size_bytes AS size_bytes, m.created_at AS created_at, \
                MIN(d.user_id) AS owner \
         FROM media_items m \
         JOIN downloads d ON d.media_id = m.id \
         WHERE m.status = 'temporary' AND d.room_id = ?1 AND d.state = 'complete' \
         GROUP BY m.id \
         ORDER BY m.created_at, m.id",
    )
    .bind(room_id)
    .fetch_all(&storage.pool())
    .await?;
    rows.iter()
        .map(|r| {
            Ok(TempFileInfo {
                file_id: r.try_get("id")?,
                room_id: r.try_get("room_id")?,
                filename: r.try_get("filename")?,
                size_bytes: r.try_get("size_bytes")?,
                created_ms: r.try_get("created_at")?,
                owner_user_id: r.try_get("owner")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(AppError::from)
}

/// True when `file_id` is a temporary media item that was downloaded in
/// `room_id`.
async fn is_room_temp_file(
    storage: &Storage,
    room_id: &str,
    file_id: &str,
) -> Result<bool, AppError> {
    let hit: Option<(i64,)> = sqlx::query_as(
        "SELECT 1 FROM media_items m \
         WHERE m.id = ?1 AND m.status = 'temporary' \
           AND EXISTS (SELECT 1 FROM downloads d \
                       WHERE d.media_id = m.id AND d.room_id = ?2 AND d.state = 'complete')",
    )
    .bind(file_id)
    .bind(room_id)
    .fetch_optional(&storage.pool())
    .await?;
    Ok(hit.is_some())
}

/// Keep: mark the given room-temporary items permanent. Ids that are not
/// temporary items of `room_id` are skipped. Returns how many changed.
pub async fn keep_room_temp_files(
    storage: &Storage,
    room_id: &str,
    file_ids: &[String],
) -> Result<usize, AppError> {
    let mut changed = 0;
    for id in file_ids {
        if is_room_temp_file(storage, room_id, id).await? {
            make_permanent(storage, id).await?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// Delete: move the given room-temporary items into the library trash and
/// remove their rows. Ids that are not temporary items of `room_id` are
/// skipped. Returns how many were deleted.
///
/// Media is deduplicated by content hash, so an item also downloaded in
/// another room is one shared row: deleting it removes it for both rooms.
pub async fn delete_room_temp_files(
    storage: &Storage,
    room_id: &str,
    file_ids: &[String],
) -> Result<usize, AppError> {
    let library_root = library_root_of(storage)?;
    let mut deleted = 0;
    for id in file_ids {
        if is_room_temp_file(storage, room_id, id).await? {
            delete_item(storage, &library_root, id).await?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Parse file ids as UUIDs and normalise them to the canonical lowercase
/// hyphenated form stored in `media_items.id`.
fn parse_file_ids(file_ids: &[String]) -> Result<Vec<String>, AppError> {
    file_ids
        .iter()
        .map(|s| {
            Uuid::parse_str(s)
                .map(|u| u.to_string())
                .map_err(|e| AppError::Other {
                    message: format!("bad file_id: {e}"),
                })
        })
        .collect()
}

/// The room the caller is currently in, as the canonical id string.
async fn current_room_id(room: &RoomClient) -> Result<String, AppError> {
    let summary = room.state().await.ok_or_else(|| AppError::Other {
        message: "not in a room".to_string(),
    })?;
    Uuid::parse_str(&summary.id)
        .map(|u| u.to_string())
        .map_err(|e| AppError::Other {
            message: format!("bad cached room id: {e}"),
        })
}

/// `get_temp_files` only lists the room the caller is currently in; a
/// caller-supplied id for any other room is rejected.
pub fn ensure_active_room(active: &str, requested: &str) -> Result<(), AppError> {
    if active == requested {
        Ok(())
    } else {
        Err(AppError::Other {
            message: "room_id is not the current room".to_string(),
        })
    }
}

#[tauri::command]
#[specta::specta]
pub async fn get_temp_files(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    storage: TauriState<'_, Storage>,
    room_id: String,
) -> Result<Vec<TempFileInfo>, AppError> {
    let active = current_room_id(room.inner()).await?;
    let rid = Uuid::parse_str(&room_id).map_err(|e| AppError::Other {
        message: format!("bad room_id: {e}"),
    })?;
    ensure_active_room(&active, &rid.to_string())?;
    list_room_temp_files(storage.inner(), &active).await
}

#[tauri::command]
#[specta::specta]
pub async fn mark_files_permanent(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    storage: TauriState<'_, Storage>,
    file_ids: Vec<String>,
) -> Result<(), AppError> {
    let room_id = current_room_id(room.inner()).await?;
    let ids = parse_file_ids(&file_ids)?;
    keep_room_temp_files(storage.inner(), &room_id, &ids).await?;
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_files_to_trash(
    room: TauriState<'_, std::sync::Arc<RoomClient>>,
    storage: TauriState<'_, Storage>,
    file_ids: Vec<String>,
) -> Result<(), AppError> {
    let room_id = current_room_id(room.inner()).await?;
    let ids = parse_file_ids(&file_ids)?;
    delete_room_temp_files(storage.inner(), &room_id, &ids).await?;
    Ok(())
}
