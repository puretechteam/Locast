//! `commands::library` - the library catalog IPC surface.
//!
//! P1-T09 gives the Library page the three operations it needs on top of
//! the existing `media_import` and `library_scan` commands:
//!
//! - `library_list(query, limit, offset)` - list the `media_items` rows,
//!   newest first, or FTS5-search them (architecture section 7, "FTS5
//!   virtual table for library search") ranked by relevance.
//! - `library_make_permanent(id)` - flip `status` from `temporary` to
//!   `permanent` (architecture 23.7) and clear `last_room_id`.
//! - `library_delete(id)` - "Delete from library": move the file into
//!   `<library_root>/trash/` and delete the row (architecture "Trash vs
//!   immediate delete", `mode = "trash"`).
//!
//! The functions the commands wrap ([`list_items`], [`make_permanent`],
//! [`delete_item`]) take plain arguments so the integration tests can call
//! them without a Tauri runtime. [`LibraryItem`] exposes only columns the
//! schema defines; paths, hashes beyond `sha256`, and provenance stay on
//! the Rust side.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use specta::Type;
use sqlx::Row;
use tauri::State as TauriState;

use crate::commands::error::AppError;
use crate::core::paths::{validate_library_path, LibraryPathError};
use crate::storage::Storage;

/// Default page size for `library_list` when the caller passes none.
pub const DEFAULT_LIST_LIMIT: u32 = 500;
/// Hard cap on `library_list` page size.
pub const MAX_LIST_LIMIT: u32 = 1000;

/// One library entry as shown on the Library page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct LibraryItem {
    pub id: String,
    pub sha256: String,
    pub filename: String,
    #[specta(type = specta_typescript::Number)]
    pub size_bytes: i64,
    /// Probe-derived; `None` when ffprobe was unavailable at import.
    pub duration_ms: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub container: Option<String>,
    /// `"permanent"` or `"temporary"` (the schema's CHECK constraint).
    pub status: String,
    /// Unix milliseconds.
    #[specta(type = specta_typescript::Number)]
    pub created_at: i64,
}

const SELECT_COLUMNS: &str = "m.id, m.sha256, m.filename, m.size_bytes, m.duration_ms, \
     m.width, m.height, m.video_codec, m.audio_codec, m.container, m.status, m.created_at";

/// Read an optional INTEGER column as `u32`, treating values that do not fit
/// (negative, or beyond ~49 days of milliseconds) as unknown.
fn opt_u32(row: &sqlx::sqlite::SqliteRow, col: &str) -> Result<Option<u32>, sqlx::Error> {
    let v: Option<i64> = row.try_get(col)?;
    Ok(v.and_then(|n| u32::try_from(n).ok()))
}

fn row_to_item(row: &sqlx::sqlite::SqliteRow) -> Result<LibraryItem, sqlx::Error> {
    Ok(LibraryItem {
        id: row.try_get("id")?,
        sha256: row.try_get("sha256")?,
        filename: row.try_get("filename")?,
        size_bytes: row.try_get("size_bytes")?,
        duration_ms: opt_u32(row, "duration_ms")?,
        width: opt_u32(row, "width")?,
        height: opt_u32(row, "height")?,
        video_codec: row.try_get("video_codec")?,
        audio_codec: row.try_get("audio_codec")?,
        container: row.try_get("container")?,
        status: row.try_get("status")?,
        created_at: row.try_get("created_at")?,
    })
}

/// Turn free-form user input into a safe FTS5 query: every run of
/// alphanumeric characters becomes a quoted prefix term, and the terms are
/// ANDed. Returns `None` when the input contains no searchable characters.
/// Quoting every term means FTS5 operators and punctuation typed by the
/// user (`"`, `-`, `:`, `AND`, ...) are never interpreted as syntax.
fn fts_query(input: &str) -> Option<String> {
    let terms: Vec<String> = input
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\"*"))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" "))
    }
}

/// List library items. With no (or blank) `query` the newest items come
/// first; otherwise the FTS5 index is searched and results are ranked by
/// relevance. Input with no searchable characters matches nothing.
pub async fn list_items(
    storage: &Storage,
    query: Option<&str>,
    limit: Option<u32>,
    offset: Option<u32>,
) -> Result<Vec<LibraryItem>, AppError> {
    let limit = i64::from(limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT));
    let offset = i64::from(offset.unwrap_or(0));
    let pool = storage.pool();

    let rows = match query.map(str::trim).filter(|q| !q.is_empty()) {
        None => {
            sqlx::query(&format!(
                "SELECT {SELECT_COLUMNS} FROM media_items m \
                 ORDER BY m.created_at DESC, m.id DESC LIMIT ?1 OFFSET ?2"
            ))
            .bind(limit)
            .bind(offset)
            .fetch_all(&pool)
            .await?
        }
        Some(q) => {
            let Some(fts) = fts_query(q) else {
                return Ok(Vec::new());
            };
            sqlx::query(&format!(
                "SELECT {SELECT_COLUMNS} FROM media_items m \
                 JOIN media_items_fts f ON f.rowid = m.rowid \
                 WHERE media_items_fts MATCH ?1 \
                 ORDER BY rank, m.id LIMIT ?2 OFFSET ?3"
            ))
            .bind(fts)
            .bind(limit)
            .bind(offset)
            .fetch_all(&pool)
            .await?
        }
    };
    rows.iter()
        .map(|r| row_to_item(r).map_err(AppError::from))
        .collect()
}

/// Promote an item to `permanent` and detach it from any room
/// (architecture 23.7). Idempotent for items that are already permanent.
pub async fn make_permanent(storage: &Storage, id: &str) -> Result<(), AppError> {
    let res = sqlx::query(
        "UPDATE media_items SET status = 'permanent', last_room_id = NULL WHERE id = ?1",
    )
    .bind(id)
    .execute(&storage.pool())
    .await?;
    if res.rows_affected() == 0 {
        return Err(AppError::NotFound {
            message: format!("no library item with id {id}"),
        });
    }
    Ok(())
}

/// Remove an item from the library: move its file into
/// `<library_root>/trash/<sha256>-<unix_ms>/` and delete the row (the
/// `downloads` and `media_subtitles` rows cascade). A file that is already
/// missing from disk is not an error; the stale row is still removed. If
/// the row cannot be deleted after the file moved, the move is undone.
pub async fn delete_item(storage: &Storage, library_root: &Path, id: &str) -> Result<(), AppError> {
    let pool = storage.pool();
    let row = sqlx::query("SELECT sha256, filename, relative_path FROM media_items WHERE id = ?1")
        .bind(id)
        .fetch_optional(&pool)
        .await?
        .ok_or_else(|| AppError::NotFound {
            message: format!("no library item with id {id}"),
        })?;
    let sha256: String = row.try_get("sha256")?;
    let filename: String = row.try_get("filename")?;
    let relative_path: String = row.try_get("relative_path")?;

    let source = match validate_library_path(library_root, &relative_path).await {
        Ok(p) => Some(p),
        Err(LibraryPathError::NotFound) => None,
        Err(e) => {
            return Err(AppError::OutOfLibrary {
                message: e.to_string(),
            })
        }
    };

    let moved: Option<(PathBuf, PathBuf)> = match source {
        None => None,
        Some(src) => {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let dir = library_root
                .join("trash")
                .join(format!("{sha256}-{now_ms}"));
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| AppError::Fs {
                    message: format!("create trash dir: {e}"),
                })?;
            let dest = dir.join(&filename);
            tokio::fs::rename(&src, &dest)
                .await
                .map_err(|e| AppError::Fs {
                    message: format!("move to trash: {e}"),
                })?;
            Some((src, dest))
        }
    };

    if let Err(e) = sqlx::query("DELETE FROM media_items WHERE id = ?1")
        .bind(id)
        .execute(&pool)
        .await
    {
        if let Some((src, dest)) = moved {
            let _ = tokio::fs::rename(&dest, &src).await;
        }
        return Err(e.into());
    }
    Ok(())
}

/// Tauri command: list or search the library.
#[tauri::command]
#[specta::specta]
pub async fn library_list(
    storage: TauriState<'_, Storage>,
    query: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
) -> Result<Vec<LibraryItem>, AppError> {
    list_items(storage.inner(), query.as_deref(), limit, offset).await
}

/// Tauri command: "Make permanent" for one library item.
#[tauri::command]
#[specta::specta]
pub async fn library_make_permanent(
    storage: TauriState<'_, Storage>,
    id: String,
) -> Result<(), AppError> {
    make_permanent(storage.inner(), &id).await
}

/// Tauri command: "Delete from library" for one library item.
#[tauri::command]
#[specta::specta]
pub async fn library_delete(storage: TauriState<'_, Storage>, id: String) -> Result<(), AppError> {
    let data_dir = storage
        .path()
        .parent()
        .ok_or_else(|| AppError::InvalidPath {
            path: storage.path().to_string_lossy().into_owned(),
            message: "storage path has no parent".to_string(),
        })?
        .to_path_buf();
    delete_item(storage.inner(), &data_dir, &id).await
}

#[cfg(test)]
mod tests {
    use super::fts_query;

    #[test]
    fn fts_query_quotes_every_term_as_a_prefix() {
        assert_eq!(
            fts_query("movie night").as_deref(),
            Some("\"movie\"* \"night\"*")
        );
    }

    #[test]
    fn fts_query_neutralises_operators_and_punctuation() {
        assert_eq!(
            fts_query("\"a\" OR -b: c*").as_deref(),
            Some("\"a\"* \"OR\"* \"b\"* \"c\"*")
        );
    }

    #[test]
    fn fts_query_without_searchable_characters_is_none() {
        assert_eq!(fts_query("  -- \"\" *"), None);
        assert_eq!(fts_query(""), None);
    }

    #[test]
    fn fts_query_keeps_non_ascii_letters() {
        assert_eq!(fts_query("Amélie").as_deref(), Some("\"Amélie\"*"));
    }
}
