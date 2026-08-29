//! Typed repository for the `recent_rooms` table.
//!
//! P2-T08 introduces the "Recent" list on the `/rooms` page. Each row
//! captures the host's display name at write time so the page can
//! render the host's name without re-querying the server or the local
//! identity store. The recents list is write-mostly; the page reads
//! it on mount and on every `room://state` event.
//!
//! # Idempotency
//!
//! `upsert_recent_room` is idempotent on `room_id` and uses
//! `COALESCE(excluded.last_ended_ms, recent_rooms.last_ended_ms)`
//! for the `last_ended_ms` column so a stale `room://state` event
//! that arrives after the room ended cannot "re-open" the room by
//! setting the column back to NULL. Every other column is overwritten
//! with the new values.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use specta::Type;
use thiserror::Error;

use crate::storage::Storage;

/// Errors raised by the recents repository.
#[derive(Debug, Error)]
pub enum RecentRoomsError {
    /// A SQLite statement failed at runtime.
    #[error("recent_rooms sql error: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// The role the local user had in this room at the time the recents
/// row was last updated. `host` is set when the local user is the
/// room's host (creator); `guest` is set when they joined. The role
/// is captured at write time; if the room later migrates to a new
/// host, the migration event triggers another upsert that refreshes
/// `host_user_id` and `host_display_name` but does NOT auto-flip
/// the local user's `role`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
pub enum RecentRoomRole {
    Host,
    Guest,
}

/// One row of the `recent_rooms` table.
///
/// The struct is the wire shape of the `recent_room_upsert` and
/// `recent_rooms_list` IPC commands; it is the only public type in
/// this module. `last_ended_ms` is `None` for rooms that are still
/// active (the recents row was last touched by a non-null
/// `room://state` payload) and `Some` once the room has ended and
/// the React side observed the `room://state` event with
/// `payload === null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct RecentRoomEntry {
    pub room_id: String,
    pub code: String,
    pub title: String,
    pub host_user_id: String,
    pub host_display_name: String,
    pub role: RecentRoomRole,
    pub last_seen_ms: i64,
    pub last_ended_ms: Option<i64>,
    pub created_ms: i64,
}

/// UPSERT a recents row keyed on `room_id`.
///
/// Every column except `last_ended_ms` is overwritten with the
/// incoming values. `last_ended_ms` uses
/// `COALESCE(excluded.last_ended_ms, recent_rooms.last_ended_ms)` so
/// a stale `room://state` event cannot re-open an ended room by
/// writing `NULL` over a previously-set end timestamp. The
/// `created_ms` column is set on insert and left alone on update;
/// for a row that already exists we preserve the original create
/// time so the recents UI can show how long ago the room was
/// originally created.
pub async fn upsert_recent_room(
    storage: &Storage,
    entry: &RecentRoomEntry,
) -> Result<(), RecentRoomsError> {
    let role = match entry.role {
        RecentRoomRole::Host => "host",
        RecentRoomRole::Guest => "guest",
    };
    sqlx::query(
        "INSERT INTO recent_rooms (room_id, code, title, host_user_id, host_display_name, role, last_seen_ms, last_ended_ms, created_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT(room_id) DO UPDATE SET \
             code = excluded.code, \
             title = excluded.title, \
             host_user_id = excluded.host_user_id, \
             host_display_name = excluded.host_display_name, \
             role = excluded.role, \
             last_seen_ms = excluded.last_seen_ms, \
             last_ended_ms = COALESCE(excluded.last_ended_ms, recent_rooms.last_ended_ms)",
    )
    .bind(&entry.room_id)
    .bind(&entry.code)
    .bind(&entry.title)
    .bind(&entry.host_user_id)
    .bind(&entry.host_display_name)
    .bind(role)
    .bind(entry.last_seen_ms)
    .bind(entry.last_ended_ms)
    .bind(entry.created_ms)
    .execute(&storage.pool())
    .await?;
    Ok(())
}

/// The raw row shape returned by `list_recent_rooms`'s SQL
/// statement. Extracted to keep `list_recent_rooms` below the
/// `clippy::type_complexity` threshold.
type RecentRoomRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
    Option<i64>,
    i64,
);

/// Read the recents list, newest activity first.
///
/// The `limit` argument is the maximum number of rows returned; the
/// caller (the IPC command) passes a hard-coded cap (100 in v1).
pub async fn list_recent_rooms(
    storage: &Storage,
    limit: i64,
) -> Result<Vec<RecentRoomEntry>, RecentRoomsError> {
    let rows: Vec<RecentRoomRow> = sqlx::query_as(
        "SELECT room_id, code, title, host_user_id, host_display_name, role, last_seen_ms, last_ended_ms, created_ms FROM recent_rooms ORDER BY last_seen_ms DESC LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(&storage.pool())
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let role = match row.5.as_str() {
            "host" => RecentRoomRole::Host,
            "guest" => RecentRoomRole::Guest,
            other => {
                return Err(RecentRoomsError::Sqlx(sqlx::Error::ColumnDecode {
                    index: "role".into(),
                    source: Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("recent_rooms.role has unknown value {other:?}"),
                    )),
                }));
            }
        };
        out.push(RecentRoomEntry {
            room_id: row.0,
            code: row.1,
            title: row.2,
            host_user_id: row.3,
            host_display_name: row.4,
            role,
            last_seen_ms: row.6,
            last_ended_ms: row.7,
            created_ms: row.8,
        });
    }
    Ok(out)
}
