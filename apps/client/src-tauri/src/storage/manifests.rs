//! `storage::manifests` - the client-side mirror of the
//! server's `room_manifests` table.
//!
//! P3-T03: when the viewer receives a verified manifest
//! over the room channel, it stores a row in the local
//! SQLite `room_manifests` table for the download planner
//! (P3-T04) to read back. The viewer also exposes the
//! `RoomManifestEntry` struct for code that wants to load
//! the latest manifest for a given room.
//!
//! The host does NOT need to write to this table when it
//! signs and publishes — the host's own `room_manifests`
//! row is created the first time the host receives its own
//! `MANIFEST_PUBLISHED` broadcast (i.e. the same path the
//! viewer takes). This keeps the writer single-path and
//! avoids divergent local state.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use locast_manifest::MediaManifest;

/// One row of the `room_manifests` table. The schema
/// mirrors `apps/client/src-tauri/migrations/0001_init.sql`
/// lines 46-56: `id` (UUIDv7), `room_id` (FK), `created_at`
/// (unix ms), `media` (JSON array), `subtitles` (JSON
/// array, default `[]`), `version` (per-room monotonic).
/// `host_signature` lives inside the stored media JSON, per
/// the schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoomManifestEntry {
    pub id: String,
    pub room_id: String,
    pub created_at: i64,
    pub media_json: String,
    pub subtitles_json: String,
    pub version: i64,
}

/// Repository for the `room_manifests` table.
pub struct ManifestStore<'a> {
    pool: &'a SqlitePool,
}

impl<'a> ManifestStore<'a> {
    /// Build a store bound to the given pool. Borrow-only;
    /// the storage is owned by the `IdentityService` /
    /// `RoomClient` tree.
    pub fn new(pool: &'a SqlitePool) -> Self {
        Self { pool }
    }

    /// INSERT (or REPLACE) a manifest row. The `id` is
    /// assigned by the caller (typically a UUIDv7). The
    /// `version` is the server's per-room monotonic version
    /// for this manifest.
    pub async fn upsert(
        &self,
        id: Uuid,
        room_id: Uuid,
        created_at: i64,
        manifest: &MediaManifest,
        version: i64,
    ) -> Result<(), sqlx::Error> {
        let media_json = serde_json::to_string(&manifest.media).map_err(|e| {
            sqlx::Error::Encode(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("serialize media: {e}"),
            )))
        })?;
        let subtitles_json = serde_json::to_string(&manifest.subtitles).map_err(|e| {
            sqlx::Error::Encode(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("serialize subtitles: {e}"),
            )))
        })?;
        sqlx::query(
            "INSERT INTO room_manifests (id, room_id, created_at, media, subtitles, version) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(id) DO UPDATE SET \
                media      = excluded.media, \
                subtitles  = excluded.subtitles, \
                created_at = excluded.created_at, \
                version    = excluded.version",
        )
        .bind(id.to_string())
        .bind(room_id.to_string())
        .bind(created_at)
        .bind(media_json)
        .bind(subtitles_json)
        .bind(version)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// Load the latest version of a room's manifest, if any.
    /// Returns `Ok(None)` if no row exists.
    pub async fn get_latest(
        &self,
        room_id: Uuid,
    ) -> Result<Option<RoomManifestEntry>, sqlx::Error> {
        let row: Option<(String, String, i64, String, String, i64)> = sqlx::query_as(
            "SELECT id, room_id, created_at, media, subtitles, version \
             FROM room_manifests WHERE room_id = ?1 \
             ORDER BY version DESC LIMIT 1",
        )
        .bind(room_id.to_string())
        .fetch_optional(self.pool)
        .await?;
        Ok(row.map(
            |(id, room_id, created_at, media_json, subtitles_json, version)| RoomManifestEntry {
                id,
                room_id,
                created_at,
                media_json,
                subtitles_json,
                version,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use locast_manifest::{Codecs, Dimensions, HostSignature, MediaEntry, MediaManifest, Source};
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        sqlx::query(
            "CREATE TABLE room_manifests (
                id TEXT PRIMARY KEY,
                room_id TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                media TEXT NOT NULL,
                subtitles TEXT NOT NULL DEFAULT '[]',
                version INTEGER NOT NULL,
                UNIQUE (room_id, version)
            )",
        )
        .execute(&pool)
        .await
        .expect("create table");
        pool
    }

    fn fixture_manifest() -> MediaManifest {
        MediaManifest {
            manifest_version: 1,
            room_id: Uuid::now_v7().to_string(),
            media: vec![MediaEntry {
                id: Uuid::now_v7().to_string(),
                filename: "x.mp4".to_string(),
                sha256: "a".repeat(64),
                blake3: "b".repeat(64),
                size_bytes: 1024,
                mime: "video/mp4".to_string(),
                duration_ms: 60_000,
                dimensions: Some(Dimensions {
                    width: 1920,
                    height: 1080,
                }),
                codecs: Some(Codecs {
                    video: Some("h264".into()),
                    audio: Some("aac".into()),
                    container: Some("mp4".into()),
                }),
                sources: vec![Source {
                    peer_id: "peer".into(),
                    url_hint: None,
                    priority: 0,
                    chunk_size: 65536,
                    total_chunks: 1,
                    chunk_hashes: vec!["c".repeat(64)],
                }],
            }],
            subtitles: vec![],
            created_at: 1_000,
            host_signature: Some(HostSignature {
                public_key: "pk".into(),
                algorithm: "ed25519".into(),
                value: "sig".into(),
            }),
        }
    }

    #[tokio::test]
    async fn upsert_then_get_latest_round_trips() {
        let pool = fresh_pool().await;
        let store = ManifestStore::new(&pool);
        let room_id = Uuid::now_v7();
        let manifest = fixture_manifest();
        let id = Uuid::now_v7();
        store
            .upsert(id, room_id, 1_000, &manifest, 1)
            .await
            .expect("upsert");
        let got = store
            .get_latest(room_id)
            .await
            .expect("get_latest")
            .expect("row");
        assert_eq!(got.id, id.to_string());
        assert_eq!(got.version, 1);
        let media: Vec<MediaEntry> = serde_json::from_str(&got.media_json).expect("media json");
        assert_eq!(media.len(), 1);
        assert_eq!(media[0].filename, "x.mp4");
    }

    #[tokio::test]
    async fn get_latest_returns_none_for_empty_room() {
        let pool = fresh_pool().await;
        let store = ManifestStore::new(&pool);
        let got = store.get_latest(Uuid::now_v7()).await.expect("get_latest");
        assert!(got.is_none());
    }
}
