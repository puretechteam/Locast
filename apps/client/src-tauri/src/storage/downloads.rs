//! `storage::downloads` - the client-side download repository.
//!
//! P7-T04: wraps the existing `DownloadStore` (from `transfer/state.rs`)
//! with a thinner, more focused API surface that the Tauri commands
//! can use. Keeps all SQL behind this module so the command layer
//! stays clean and testable.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use sqlx::SqlitePool;

use crate::transfer::state::{ChunkStateError, DownloadStore, DownloadSummary, NewDownload};

/// Repository for the `downloads` and `download_chunks` tables.
/// Delegates to `DownloadStore` for the state machine logic.
#[derive(Debug, Clone)]
pub struct DownloadRepository {
    store: DownloadStore,
}

impl DownloadRepository {
    pub fn new(pool: &SqlitePool) -> Self {
        Self {
            store: DownloadStore::new(pool.clone()),
        }
    }

    /// Create a new download row with chunk pre-population.
    pub async fn create(
        &self,
        new: &NewDownload,
        chunks: &[(u32, u64, u32, String)],
    ) -> Result<(), ChunkStateError> {
        self.store.create(new, chunks).await
    }

    /// Get a single download by ID.
    pub async fn fetch(
        &self,
        download_id: &str,
    ) -> Result<crate::transfer::state::DownloadRecord, ChunkStateError> {
        self.store.fetch(download_id).await
    }

    /// List recent downloads (newest first).
    pub async fn list(&self, limit: i64) -> Result<Vec<DownloadSummary>, ChunkStateError> {
        self.store.list(limit).await
    }

    /// Transition download state (pause/resume/cancel).
    pub async fn transition(
        &self,
        download_id: &str,
        to: crate::transfer::state::DownloadState,
    ) -> Result<(), ChunkStateError> {
        self.store.transition(download_id, to).await
    }

    /// Get completed chunk indices (verified + received) for resume.
    pub async fn completed_chunk_indices(
        &self,
        download_id: &str,
    ) -> Result<Vec<u32>, ChunkStateError> {
        self.store.completed_chunk_indices(download_id).await
    }

    /// Get completed chunk bitmap for resume hello frame.
    pub async fn completed_chunk_bitmap(
        &self,
        download_id: &str,
    ) -> Result<Vec<u8>, ChunkStateError> {
        self.store.completed_chunk_bitmap(download_id).await
    }

    /// Reset in-flight chunks to pending (called on WS reconnect).
    pub async fn reset_in_flight_to_pending(
        &self,
        download_id: &str,
    ) -> Result<u64, ChunkStateError> {
        self.store.reset_in_flight_to_pending(download_id).await
    }

    /// Bind a newer manifest version (must be strictly greater).
    pub async fn bind_manifest_version(
        &self,
        download_id: &str,
        new_version: i64,
    ) -> Result<(), ChunkStateError> {
        self.store
            .bind_manifest_version(download_id, new_version)
            .await
    }

    /// Set last error message on a download.
    pub async fn set_last_error(
        &self,
        download_id: &str,
        msg: &str,
    ) -> Result<(), ChunkStateError> {
        self.store.set_last_error(download_id, msg).await
    }

    /// Mark chunk verified (idempotent with hash check).
    pub async fn mark_chunk_verified(
        &self,
        download_id: &str,
        index: u32,
        sha256: &str,
    ) -> Result<(), ChunkStateError> {
        self.store
            .mark_chunk_verified(download_id, index, sha256)
            .await
    }

    /// Mark chunk received (bytes on disk, not yet verified).
    pub async fn mark_chunk_received(
        &self,
        download_id: &str,
        index: u32,
    ) -> Result<(), ChunkStateError> {
        self.store.mark_chunk_received(download_id, index).await
    }

    /// Mark download complete with final size and chunk hashes.
    pub async fn mark_complete(
        &self,
        download_id: &str,
        total_bytes: u64,
        source_peer_id: Option<&str>,
        chunk_hashes: &[(u32, String)],
    ) -> Result<(), ChunkStateError> {
        self.store
            .mark_complete(download_id, total_bytes, source_peer_id, chunk_hashes)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn fresh_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory sqlite");
        // Run the migrations that create downloads/download_chunks
        sqlx::query(
            "CREATE TABLE downloads (
                id TEXT PRIMARY KEY,
                media_id TEXT NOT NULL,
                room_id TEXT,
                user_id TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN ('pending','connecting','transferring','verifying','complete','failed','paused','cancelled')),
                total_bytes INTEGER NOT NULL,
                transferred_bytes INTEGER NOT NULL DEFAULT 0,
                started_at INTEGER NOT NULL,
                completed_at INTEGER,
                last_error TEXT,
                source_peer_id TEXT NOT NULL,
                chunk_size_bytes INTEGER NOT NULL DEFAULT 262144,
                manifest_version INTEGER NOT NULL DEFAULT 1
            )",
        )
        .execute(&pool)
        .await
        .expect("create downloads");
        sqlx::query(
            "CREATE TABLE download_chunks (
                id TEXT PRIMARY KEY,
                download_id TEXT NOT NULL,
                \"index\" INTEGER NOT NULL,
                offset INTEGER NOT NULL,
                length INTEGER NOT NULL,
                sha256 TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN ('pending','in_flight','received','verified','failed')),
                UNIQUE (download_id, \"index\")
            )",
        )
        .execute(&pool)
        .await
        .expect("create download_chunks");
        pool
    }

    use uuid::Uuid;

    fn new_download() -> NewDownload {
        NewDownload {
            download_id: Uuid::new_v4().to_string(),
            media_id: Uuid::new_v4().to_string(),
            room_id: Some(Uuid::new_v4().to_string()),
            user_id: Uuid::new_v4().to_string(),
            total_bytes: 1024 * 1024,
            source_peer_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
            chunk_size_bytes: 262144,
            manifest_version: 1,
        }
    }

    fn chunks_for(size: u64) -> Vec<(u32, u64, u32, String)> {
        let chunk_size = 262144u64;
        let total_chunks = if size == 0 {
            0
        } else {
            size.div_ceil(chunk_size) as u32
        };
        (0..total_chunks)
            .map(|i| {
                let offset = (i as u64) * chunk_size;
                let length = std::cmp::min(chunk_size, size - offset) as u32;
                (i, offset, length, format!("{:064x}", i))
            })
            .collect()
    }

    #[tokio::test]
    async fn create_then_list_round_trips() {
        let pool = fresh_pool().await;
        let repo = DownloadRepository::new(&pool);
        let new = new_download();
        let chunks = chunks_for(new.total_bytes);
        repo.create(&new, &chunks).await.expect("create");
        let list = repo.list(10).await.expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, new.download_id);
        assert_eq!(list[0].media_id, new.media_id);
        assert_eq!(list[0].state.as_str(), "pending");
    }

    #[tokio::test]
    async fn fetch_returns_download_record() {
        let pool = fresh_pool().await;
        let repo = DownloadRepository::new(&pool);
        let new = new_download();
        let chunks = chunks_for(new.total_bytes);
        repo.create(&new, &chunks).await.expect("create");
        let record = repo.fetch(&new.download_id).await.expect("fetch");
        assert_eq!(record.id, new.download_id);
        assert_eq!(record.media_id, new.media_id);
        assert_eq!(record.state.as_str(), "pending");
    }

    #[tokio::test]
    async fn transition_to_paused_then_resume() {
        let pool = fresh_pool().await;
        let repo = DownloadRepository::new(&pool);
        let new = new_download();
        let chunks = chunks_for(new.total_bytes);
        repo.create(&new, &chunks).await.expect("create");
        repo.transition(
            &new.download_id,
            crate::transfer::state::DownloadState::Paused,
        )
        .await
        .expect("pause");
        let record = repo.fetch(&new.download_id).await.expect("fetch");
        assert_eq!(record.state.as_str(), "paused");
        repo.transition(
            &new.download_id,
            crate::transfer::state::DownloadState::Transferring,
        )
        .await
        .expect("resume");
        let record = repo.fetch(&new.download_id).await.expect("fetch");
        assert_eq!(record.state.as_str(), "transferring");
    }

    #[tokio::test]
    async fn completed_chunk_bitmap_works() {
        let pool = fresh_pool().await;
        let repo = DownloadRepository::new(&pool);
        let new = new_download();
        let chunks = chunks_for(new.total_bytes);
        repo.create(&new, &chunks).await.expect("create");
        // Mark a few chunks verified
        repo.mark_chunk_verified(&new.download_id, 0, &chunks[0].3)
            .await
            .expect("mark 0");
        repo.mark_chunk_verified(&new.download_id, 1, &chunks[1].3)
            .await
            .expect("mark 1");
        let bitmap = repo
            .completed_chunk_bitmap(&new.download_id)
            .await
            .expect("bitmap");
        assert!(!bitmap.is_empty());
        // First byte: bits 0 and 1 set = 0b00000011 = 3
        assert_eq!(bitmap[0], 3);
    }
}
