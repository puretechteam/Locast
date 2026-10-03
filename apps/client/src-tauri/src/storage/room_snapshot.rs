//! Mirror the server's room snapshot into the local `rooms`,
//! `user_identities` and `room_participants` tables.
//!
//! The P3 download path (`commands::download::open_download_inner`)
//! depends on three local rows that nothing in production wrote
//! before this module existed:
//!
//! - `rooms(id)`: `downloads.room_id` is a foreign key to it.
//! - `room_participants(room_id, user_id)`: the per-room list of
//!   server-issued participant ids.
//! - `user_identities(id = <server user_id>, public_key)`: lets the
//!   `WebRtcManager` map a connected peer (keyed by server user id)
//!   onto a manifest `Source::peer_id` (sha256 of the pubkey).
//!
//! The pubkeys stored here come from the server's `RoomSummary` and
//! are only used for that peer-routing lookup. They are NOT a trust
//! anchor: manifest trust comes from the invite `h=` key, and every
//! chunk is verified against the signed manifest regardless of which
//! peer delivered it.
//!
//! # Local user
//!
//! The local user's own participant entry is skipped. Their identity
//! row is keyed by `sha256(pubkey)` (see
//! `IdentityService::ensure_user_row`), and `user_identities.public_key`
//! is UNIQUE, so a second row for the same key under the server id
//! would break `ensure_user_row` on the next download. The local user
//! never downloads from themselves, so the lookup does not need it.
//! For the same reason the `rooms` row is only written when the host
//! is a remote participant (the viewer side, which is the side that
//! opens downloads).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use locast_protocol::room::{ParticipantStatus, RoomSummary};
use sqlx::SqlitePool;
use tracing::warn;
use uuid::Uuid;

fn connection_state(status: ParticipantStatus) -> &'static str {
    match status {
        ParticipantStatus::Joining => "connecting",
        ParticipantStatus::Connected => "connected",
        ParticipantStatus::Reconnecting => "reconnecting",
        ParticipantStatus::Disconnected => "disconnected",
        ParticipantStatus::Left => "left",
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Upsert the rows for every remote participant in `room`. Idempotent.
///
/// `local_user_id` is the caller's own server-issued id; that entry is
/// skipped (see the module docs). Participants whose pubkey is not 32
/// bytes, or whose pubkey is already stored under a different id, are
/// skipped with a warning rather than failing the whole snapshot.
pub async fn persist_room_snapshot(
    pool: &SqlitePool,
    room: &RoomSummary,
    local_user_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    let now = now_ms();
    let room_id = room.id.to_string();
    let mut tx = pool.begin().await?;

    let mut stored: Vec<(String, String, &'static str, &'static str)> = Vec::new();
    for p in &room.participants {
        if Some(p.user_id) == local_user_id {
            continue;
        }
        if p.pubkey.len() != 32 {
            warn!(user_id = %p.user_id, len = p.pubkey.len(), "room snapshot: skipping participant with bad pubkey length");
            continue;
        }
        let uid = p.user_id.to_string();
        // `ON CONFLICT DO NOTHING` without a target covers both the
        // `id` and the `public_key` UNIQUE constraints.
        sqlx::query(
            "INSERT INTO user_identities (id, public_key, display_name, created_at, last_seen) \
             VALUES (?1, ?2, ?3, ?4, ?4) \
             ON CONFLICT DO NOTHING",
        )
        .bind(&uid)
        .bind(BASE64.encode(&p.pubkey))
        .bind(&p.display_name)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE user_identities SET last_seen = ?2 WHERE id = ?1")
            .bind(&uid)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        let exists: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM user_identities WHERE id = ?1")
            .bind(&uid)
            .fetch_optional(&mut *tx)
            .await?;
        if exists.is_none() {
            warn!(user_id = %uid, "room snapshot: pubkey already stored under another id; skipping participant");
            continue;
        }
        let role = if p.is_host { "host" } else { "guest" };
        stored.push((
            uid,
            p.display_name.clone(),
            role,
            connection_state(p.status),
        ));
    }

    let host_id = room.host_user_id.to_string();
    let host_is_remote =
        Some(room.host_user_id) != local_user_id && stored.iter().any(|(uid, ..)| *uid == host_id);
    if !host_is_remote {
        tx.commit().await?;
        return Ok(());
    }

    // Room codes are reused by the server after a room ends. Move a
    // stale row out of the way so the UNIQUE(code) constraint does not
    // block the new room; the old row keeps its id and history.
    sqlx::query("UPDATE rooms SET code = id WHERE code = ?1 AND id <> ?2")
        .bind(&room.code)
        .bind(&room_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO rooms (id, code, host_user_id, created_at, ended_at, state, settings) \
         VALUES (?1, ?2, ?3, ?4, NULL, 'open', '{}') \
         ON CONFLICT(id) DO UPDATE SET host_user_id = excluded.host_user_id",
    )
    .bind(&room_id)
    .bind(&room.code)
    .bind(&host_id)
    .bind(room.created_ms)
    .execute(&mut *tx)
    .await?;

    for (uid, name, role, state) in &stored {
        sqlx::query(
            "INSERT INTO room_participants \
                (id, room_id, user_id, display_name, role, joined_at, left_at, connection_state, capabilities) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, '{}') \
             ON CONFLICT(room_id, user_id) DO UPDATE SET \
                display_name = excluded.display_name, \
                role = excluded.role, \
                left_at = NULL, \
                connection_state = excluded.connection_state",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(&room_id)
        .bind(uid)
        .bind(name)
        .bind(*role)
        .bind(now)
        .bind(*state)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Convenience wrapper that logs instead of returning the error. The
/// room event loop calls this; a failed mirror must not break room
/// state handling.
pub async fn persist_room_snapshot_best_effort(
    pool: &SqlitePool,
    room: &RoomSummary,
    local_user_id: Option<Uuid>,
) {
    if let Err(e) = persist_room_snapshot(pool, room, local_user_id).await {
        warn!(room_id = %room.id, error = %e, "room snapshot: local mirror failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;
    use locast_protocol::room::Participant;
    use tempfile::TempDir;

    async fn identity_row_exists(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM user_identities WHERE id = ?1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
        Ok(row.is_some())
    }

    fn participant(user_id: Uuid, pk: u8, is_host: bool) -> Participant {
        Participant {
            user_id,
            pubkey: vec![pk; 32],
            display_name: format!("p{pk}"),
            joined_ms: 1,
            status: ParticipantStatus::Connected,
            last_seen_ms: 1,
            is_host,
        }
    }

    fn summary(code: &str, host: Uuid, participants: Vec<Participant>) -> RoomSummary {
        RoomSummary {
            id: Uuid::now_v7(),
            code: code.into(),
            title: "t".into(),
            host_user_id: host,
            host_migration_enabled: false,
            created_ms: 1,
            participants,
            host_disconnected: false,
            host_disconnect_deadline_ms: None,
        }
    }

    async fn storage() -> (TempDir, Storage) {
        let dir = TempDir::new().unwrap();
        let s = Storage::open(dir.path().join("db.sqlite")).await.unwrap();
        (dir, s)
    }

    async fn count(pool: &SqlitePool, sql: &str) -> i64 {
        let (n,): (i64,) = sqlx::query_as(sql).fetch_one(pool).await.unwrap();
        n
    }

    #[tokio::test]
    async fn viewer_side_writes_room_host_and_remote_participants_but_not_self() {
        let (_d, s) = storage().await;
        let pool = s.pool();
        let host = Uuid::now_v7();
        let me = Uuid::now_v7();
        let other = Uuid::now_v7();
        let room = summary(
            "ABCDEF",
            host,
            vec![
                participant(host, 1, true),
                participant(me, 2, false),
                participant(other, 3, false),
            ],
        );
        persist_room_snapshot(&pool, &room, Some(me)).await.unwrap();
        // Twice: idempotent.
        persist_room_snapshot(&pool, &room, Some(me)).await.unwrap();

        assert_eq!(count(&pool, "SELECT COUNT(*) FROM rooms").await, 1);
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM room_participants").await,
            2
        );
        assert!(identity_row_exists(&pool, &host.to_string()).await.unwrap());
        assert!(!identity_row_exists(&pool, &me.to_string()).await.unwrap());
        let (pk,): (String,) =
            sqlx::query_as("SELECT public_key FROM user_identities WHERE id = ?1")
                .bind(host.to_string())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(BASE64.decode(pk).unwrap(), vec![1u8; 32]);
    }

    #[tokio::test]
    async fn host_side_skips_rooms_row() {
        let (_d, s) = storage().await;
        let pool = s.pool();
        let host = Uuid::now_v7();
        let viewer = Uuid::now_v7();
        let room = summary(
            "ABCDEF",
            host,
            vec![participant(host, 1, true), participant(viewer, 2, false)],
        );
        persist_room_snapshot(&pool, &room, Some(host))
            .await
            .unwrap();
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM rooms").await, 0);
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM room_participants").await,
            0
        );
        assert!(identity_row_exists(&pool, &viewer.to_string())
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn reused_room_code_does_not_block_new_room() {
        let (_d, s) = storage().await;
        let pool = s.pool();
        let host = Uuid::now_v7();
        let me = Uuid::now_v7();
        let first = summary("ABCDEF", host, vec![participant(host, 1, true)]);
        let second = summary("ABCDEF", host, vec![participant(host, 1, true)]);
        persist_room_snapshot(&pool, &first, Some(me))
            .await
            .unwrap();
        persist_room_snapshot(&pool, &second, Some(me))
            .await
            .unwrap();
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM rooms").await, 2);
        let (code,): (String,) = sqlx::query_as("SELECT code FROM rooms WHERE id = ?1")
            .bind(second.id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(code, "ABCDEF");
    }

    #[tokio::test]
    async fn pubkey_collision_with_local_identity_is_skipped_not_fatal() {
        let (_d, s) = storage().await;
        let pool = s.pool();
        // A row for the same pubkey under a different id (e.g. the
        // local sha256-keyed identity row).
        sqlx::query(
            "INSERT INTO user_identities (id, public_key, display_name, created_at, last_seen) \
             VALUES ('local-sha', ?1, '', 1, 1)",
        )
        .bind(BASE64.encode([9u8; 32]))
        .execute(&pool)
        .await
        .unwrap();
        let host = Uuid::now_v7();
        let room = summary("ABCDEF", host, vec![participant(host, 9, true)]);
        persist_room_snapshot(&pool, &room, None).await.unwrap();
        assert!(!identity_row_exists(&pool, &host.to_string()).await.unwrap());
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM rooms").await, 0);
    }
}
