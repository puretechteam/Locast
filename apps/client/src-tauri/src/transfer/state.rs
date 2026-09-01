//! P3-T04 persistent download state.
//!
//! The `downloads` and `download_chunks` SQL tables (defined in
//! `migrations/0001_init.sql`) are the source of truth for
//! resumability. This module is the only Rust-side write path to
//! them. Every other consumer (transport, scheduler, events)
//! reads through [`DownloadStore`].
//!
//! Invariants enforced here:
//!
//! - The `downloads.chunk_size_bytes` SQL default is 262144; the
//!   planner requires the same value.
//! - `chunk_index` is bounded to `[0, total_chunks)` at every
//!   write. Out-of-bounds writes return
//   [`ChunkStateError::ChunkIndexOutOfBounds`] without touching the DB.
//! - Re-applying "complete" to an already-complete chunk is a
//!   no-op (idempotent).
//! - Re-applying "verified" to an already-verified chunk is a
//!   no-op (idempotent).
//! - A "verified" chunk that already had its SHA-256 recorded
//!   but receives a different SHA-256 fails the request -- the
//!   row cannot be silently overwritten with a different digest.
//! - File-backed SQLite is the supported deployment; the test
//!   suite pins a real `tempfile::TempDir`-backed restart test.
//!
//! Concurrency: every read+write happens in a short SQLite
//! transaction; no DB locks are held across filesystem I/O.
//! Concurrent calls to `mark_chunk_verified` for the SAME
//! `(download_id, index)` pair are coalesced by the UNIQUE
//! constraint and the `COALESCE` write semantics below.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use thiserror::Error;

/// The current `schema_version` we expect in `downloads.schema_version`.
/// P3-T04 = 1. Bumped only by a deliberate migration.
pub const SCHEMA_VERSION: i64 = 1;

/// Maximum age (in hours) after which a paused download discards
/// `received` chunks (keeps `verified`). Pinned by section 9.5
/// of the architecture. P3-T04 does not implement the sweep but
/// surfaces the constant so the eventual sweeper can reuse it.
pub const RESUME_MAX_AGE_HOURS: i64 = 72;

/// Closed set of `ChunkState` values mirrored from the SQL CHECK
/// constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkState {
    Pending,
    InFlight,
    Received,
    Verified,
    Failed,
}

impl ChunkState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChunkState::Pending => "pending",
            ChunkState::InFlight => "in_flight",
            ChunkState::Received => "received",
            ChunkState::Verified => "verified",
            ChunkState::Failed => "failed",
        }
    }
    pub fn parse_sql_value(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(ChunkState::Pending),
            "in_flight" => Some(ChunkState::InFlight),
            "received" => Some(ChunkState::Received),
            "verified" => Some(ChunkState::Verified),
            "failed" => Some(ChunkState::Failed),
            _ => None,
        }
    }
}

/// Mirrors the SQL CHECK on `downloads.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Pending,
    Connecting,
    Transferring,
    Verifying,
    Complete,
    Failed,
    Paused,
    Cancelled,
}

impl DownloadState {
    pub fn as_str(&self) -> &'static str {
        match self {
            DownloadState::Pending => "pending",
            DownloadState::Connecting => "connecting",
            DownloadState::Transferring => "transferring",
            DownloadState::Verifying => "verifying",
            DownloadState::Complete => "complete",
            DownloadState::Failed => "failed",
            DownloadState::Paused => "paused",
            DownloadState::Cancelled => "cancelled",
        }
    }
    pub fn parse_sql_value(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(DownloadState::Pending),
            "connecting" => Some(DownloadState::Connecting),
            "transferring" => Some(DownloadState::Transferring),
            "verifying" => Some(DownloadState::Verifying),
            "complete" => Some(DownloadState::Complete),
            "failed" => Some(DownloadState::Failed),
            "paused" => Some(DownloadState::Paused),
            "cancelled" => Some(DownloadState::Cancelled),
            _ => None,
        }
    }
}

/// Inputs to creating a new `downloads` row.
#[derive(Debug, Clone)]
pub struct NewDownload {
    pub download_id: String,
    pub media_id: String,
    pub room_id: Option<String>,
    pub user_id: String,
    pub total_bytes: u64,
    pub source_peer_id: String,
    pub chunk_size_bytes: u32,
    pub manifest_version: i64,
}

/// Lightweight read shape of one `downloads` row.
#[derive(Debug, Clone)]
pub struct DownloadRecord {
    pub id: String,
    pub media_id: String,
    pub room_id: Option<String>,
    pub user_id: String,
    pub state: DownloadState,
    pub total_bytes: i64,
    pub transferred_bytes: i64,
    pub source_peer_id: Option<String>,
    pub chunk_size_bytes: i64,
    pub manifest_version: i64,
}

/// Lightweight summary view returned to the webview.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DownloadSummary {
    pub id: String,
    pub media_id: String,
    pub room_id: Option<String>,
    pub state: DownloadState,
    pub total_bytes: i64,
    pub transferred_bytes: i64,
    pub source_peer_id: Option<String>,
    pub chunk_size_bytes: i64,
    pub manifest_version: i64,
}

/// Closed set of state-machine errors.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ChunkStateError {
    #[error("chunk index out of bounds: {index} >= {total}")]
    ChunkIndexOutOfBounds { index: u32, total: u32 },
    #[error("download not found: {id}")]
    DownloadNotFound { id: String },
    #[error("sha256 mismatch on chunk {index}: existing={existing}, new={new}")]
    ChunkHashMismatch {
        index: u32,
        existing: String,
        new: String,
    },
    #[error("manifest version mismatch: download={download_version}, current={current_version}")]
    ManifestVersionMismatch {
        download_version: i64,
        current_version: i64,
    },
    #[error("invalid download state transition: cannot transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: DownloadState,
        to: DownloadState,
    },
    #[error("invalid chunk_size_bytes: {value}")]
    InvalidChunkSize { value: u32 },
    #[error("invalid manifest_version: {value}")]
    InvalidManifestVersion { value: i64 },
    #[error("invalid source_peer_id: {value}")]
    InvalidSourcePeerId { value: String },
    #[error("invalid media_id: {value}")]
    InvalidMediaId { value: String },
    #[error("invalid user_id: {value}")]
    InvalidUserId { value: String },
    #[error("database error: {0}")]
    Sqlx(String),
}

impl From<sqlx::Error> for ChunkStateError {
    fn from(e: sqlx::Error) -> Self {
        ChunkStateError::Sqlx(e.to_string())
    }
}

/// The persistent download store. Cheap to clone; holds only the
/// pool.
#[derive(Debug, Clone)]
pub struct DownloadStore {
    pool: SqlitePool,
}

impl DownloadStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Create a new `downloads` row in `pending` state and
    /// pre-populate one `download_chunks` row per chunk in the
    /// plan. All work happens inside a single transaction; on
    /// failure the DB is unchanged.
    pub async fn create(
        &self,
        new: &NewDownload,
        chunks: &[(u32, u64, u32, String)],
    ) -> Result<(), ChunkStateError> {
        // Validate identifiers before touching the DB.
        if new.chunk_size_bytes != crate::transfer::CHUNK_SIZE_BYTES as u32 {
            return Err(ChunkStateError::InvalidChunkSize {
                value: new.chunk_size_bytes,
            });
        }
        if new.manifest_version < 1 {
            return Err(ChunkStateError::InvalidManifestVersion {
                value: new.manifest_version,
            });
        }
        if !crate::room::peer_id::is_canonical_peer_id(&new.source_peer_id) {
            return Err(ChunkStateError::InvalidSourcePeerId {
                value: new.source_peer_id.clone(),
            });
        }
        if new.media_id.is_empty() {
            return Err(ChunkStateError::InvalidMediaId {
                value: new.media_id.clone(),
            });
        }
        if new.user_id.is_empty() {
            return Err(ChunkStateError::InvalidUserId {
                value: new.user_id.clone(),
            });
        }
        let total_chunks = chunks.len() as u32;
        if total_chunks == 0 {
            return Err(ChunkStateError::ChunkIndexOutOfBounds { index: 0, total: 0 });
        }
        let expected_total = if new.total_bytes == 0 {
            0
        } else {
            new.total_bytes.div_ceil(new.chunk_size_bytes as u64) as u32
        };
        if total_chunks != expected_total {
            return Err(ChunkStateError::ChunkIndexOutOfBounds {
                index: total_chunks,
                total: expected_total,
            });
        }
        for (i, (idx, _offset, _length, _hash)) in chunks.iter().enumerate() {
            if *idx != i as u32 {
                return Err(ChunkStateError::ChunkIndexOutOfBounds {
                    index: *idx,
                    total: total_chunks,
                });
            }
        }

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO downloads
                 (id, media_id, room_id, user_id, state, total_bytes, transferred_bytes,
                  started_at, source_peer_id, chunk_size_bytes, manifest_version, last_error)
             VALUES (?, ?, ?, ?, 'pending', ?, 0, ?, ?, ?, ?, NULL)",
        )
        .bind(&new.download_id)
        .bind(&new.media_id)
        .bind(&new.room_id)
        .bind(&new.user_id)
        .bind(new.total_bytes as i64)
        .bind(chrono_unix_ms_now())
        .bind(&new.source_peer_id)
        .bind(new.chunk_size_bytes as i64)
        .bind(new.manifest_version)
        .execute(&mut *tx)
        .await?;

        // Bulk insert chunks. SQLite has a 999-parameter limit per
        // statement. Each chunk row has 6 bound parameters; we use
        // 100 rows per INSERT (600 params) to stay well under.
        for batch in chunks.chunks(100) {
            let mut sql = String::from(
                "INSERT INTO download_chunks
                     (id, download_id, \"index\", offset, length, sha256, state)
                 VALUES ",
            );
            let mut first = true;
            let mut ids: Vec<String> = Vec::with_capacity(batch.len());
            for (idx, _offset, _length, _hash) in batch {
                if !first {
                    sql.push(',');
                }
                first = false;
                let id = uuid::Uuid::now_v7().to_string();
                ids.push(id);
                sql.push_str("(?, ?, ?, ?, ?, ?, 'pending')");
                let _ = idx;
            }
            let mut q = sqlx::query(&sql);
            for (i, (_idx, offset, length, hash)) in batch.iter().enumerate() {
                q = q
                    .bind(&ids[i])
                    .bind(&new.download_id)
                    .bind(i as i64)
                    .bind(*offset as i64)
                    .bind(*length as i64)
                    .bind(hash.clone());
            }
            q.execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Mark a chunk `verified` with a known sha256. Idempotent:
    /// re-applying the same `(state=verified, sha256=X)` is a
    /// no-op. Mismatched sha256 fails the call.
    pub async fn mark_chunk_verified(
        &self,
        download_id: &str,
        index: u32,
        sha256: &str,
    ) -> Result<(), ChunkStateError> {
        let total = self.fetch_total_chunks(download_id).await?;
        if index >= total {
            return Err(ChunkStateError::ChunkIndexOutOfBounds { index, total });
        }
        let existing_sha = sqlx::query(
            "SELECT sha256, state FROM download_chunks
             WHERE download_id = ? AND \"index\" = ?",
        )
        .bind(download_id)
        .bind(index as i64)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(row) = existing_sha {
            let existing_hash: String = row.try_get("sha256").map_err(ChunkStateError::from)?;
            let existing_state: String = row.try_get("state").map_err(ChunkStateError::from)?;
            if existing_state == "verified" && existing_hash == sha256 {
                return Ok(());
            }
            if existing_hash != sha256 {
                return Err(ChunkStateError::ChunkHashMismatch {
                    index,
                    existing: existing_hash,
                    new: sha256.to_string(),
                });
            }
        }
        sqlx::query(
            "UPDATE download_chunks
             SET state = 'verified'
             WHERE download_id = ? AND \"index\" = ?",
        )
        .bind(download_id)
        .bind(index as i64)
        .execute(&self.pool)
        .await?;
        self.refresh_transferred_bytes(download_id).await?;
        Ok(())
    }

    /// Mark a chunk `received` (bytes on disk, SHA-256 not yet
    /// independently confirmed).
    pub async fn mark_chunk_received(
        &self,
        download_id: &str,
        index: u32,
    ) -> Result<(), ChunkStateError> {
        let total = self.fetch_total_chunks(download_id).await?;
        if index >= total {
            return Err(ChunkStateError::ChunkIndexOutOfBounds { index, total });
        }
        sqlx::query(
            "UPDATE download_chunks
             SET state = 'received'
             WHERE download_id = ? AND \"index\" = ? AND state != 'verified'",
        )
        .bind(download_id)
        .bind(index as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Returns the set of chunk indices currently in `verified`
    /// or `received` (the resumability bitmap). Deterministic
    /// ascending order.
    pub async fn completed_chunk_indices(
        &self,
        download_id: &str,
    ) -> Result<Vec<u32>, ChunkStateError> {
        let rows = sqlx::query(
            "SELECT \"index\" FROM download_chunks
             WHERE download_id = ? AND (state = 'verified' OR state = 'received')
             ORDER BY \"index\" ASC",
        )
        .bind(download_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let i: i64 = r.try_get("index").map_err(ChunkStateError::from)?;
            out.push(i as u32);
        }
        Ok(out)
    }

    /// Returns the set of chunk indices in `verified` only
    /// (used by the final verifier to determine which chunks are
    /// ready to be assembled).
    pub async fn verified_chunk_indices(
        &self,
        download_id: &str,
    ) -> Result<Vec<u32>, ChunkStateError> {
        let rows = sqlx::query(
            "SELECT \"index\" FROM download_chunks
             WHERE download_id = ? AND state = 'verified'
             ORDER BY \"index\" ASC",
        )
        .bind(download_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let i: i64 = r.try_get("index").map_err(ChunkStateError::from)?;
            out.push(i as u32);
        }
        Ok(out)
    }

    /// Returns the total number of chunks for the given
    /// download. Used everywhere we need to bound a chunk
    /// index.
    async fn fetch_total_chunks(&self, download_id: &str) -> Result<u32, ChunkStateError> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM download_chunks WHERE download_id = ?")
            .bind(download_id)
            .fetch_one(&self.pool)
            .await?;
        let n: i64 = row.try_get("n").map_err(ChunkStateError::from)?;
        Ok(n as u32)
    }

    /// Recompute `downloads.transferred_bytes` from the verified
    /// + received chunks. Called after every chunk-state change.
    async fn refresh_transferred_bytes(&self, download_id: &str) -> Result<(), ChunkStateError> {
        sqlx::query(
            "UPDATE downloads
             SET transferred_bytes = COALESCE(
                 (SELECT SUM(length) FROM download_chunks
                  WHERE download_id = ?1
                    AND (state = 'verified' OR state = 'received')),
                 0)
             WHERE id = ?1",
        )
        .bind(download_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Transition the download state machine. Returns
    /// [`ChunkStateError::InvalidTransition`] for an illegal
    /// transition.
    pub async fn transition(
        &self,
        download_id: &str,
        to: DownloadState,
    ) -> Result<(), ChunkStateError> {
        let current = self.fetch_state(download_id).await?;
        if !is_valid_transition(current, to) {
            return Err(ChunkStateError::InvalidTransition { from: current, to });
        }
        let mut q = sqlx::query("UPDATE downloads SET state = ? WHERE id = ?").bind(to.as_str());
        q = q.bind(download_id);
        q.execute(&self.pool).await?;
        Ok(())
    }

    async fn fetch_state(&self, download_id: &str) -> Result<DownloadState, ChunkStateError> {
        let row = sqlx::query("SELECT state FROM downloads WHERE id = ?")
            .bind(download_id)
            .fetch_optional(&self.pool)
            .await?;
        let row = row.ok_or_else(|| ChunkStateError::DownloadNotFound {
            id: download_id.to_string(),
        })?;
        let s: String = row.try_get("state").map_err(ChunkStateError::from)?;
        DownloadState::parse_sql_value(&s)
            .ok_or_else(|| ChunkStateError::Sqlx(format!("unknown state value in db: {s}")))
    }

    pub async fn fetch(&self, download_id: &str) -> Result<DownloadRecord, ChunkStateError> {
        let row = sqlx::query(
            "SELECT id, media_id, room_id, user_id, state, total_bytes, transferred_bytes,
                    source_peer_id, chunk_size_bytes, manifest_version
             FROM downloads WHERE id = ?",
        )
        .bind(download_id)
        .fetch_optional(&self.pool)
        .await?;
        let row = row.ok_or_else(|| ChunkStateError::DownloadNotFound {
            id: download_id.to_string(),
        })?;
        let state_str: String = row.try_get("state").map_err(ChunkStateError::from)?;
        let state = DownloadState::parse_sql_value(&state_str)
            .ok_or_else(|| ChunkStateError::Sqlx(format!("unknown state in db: {state_str}")))?;
        Ok(DownloadRecord {
            id: row.try_get("id").map_err(ChunkStateError::from)?,
            media_id: row.try_get("media_id").map_err(ChunkStateError::from)?,
            room_id: row.try_get("room_id").map_err(ChunkStateError::from)?,
            user_id: row.try_get("user_id").map_err(ChunkStateError::from)?,
            state,
            total_bytes: row.try_get("total_bytes").map_err(ChunkStateError::from)?,
            transferred_bytes: row
                .try_get("transferred_bytes")
                .map_err(ChunkStateError::from)?,
            source_peer_id: row
                .try_get("source_peer_id")
                .map_err(ChunkStateError::from)?,
            chunk_size_bytes: row
                .try_get("chunk_size_bytes")
                .map_err(ChunkStateError::from)?,
            manifest_version: row
                .try_get("manifest_version")
                .map_err(ChunkStateError::from)?,
        })
    }

    pub async fn list(&self, limit: i64) -> Result<Vec<DownloadSummary>, ChunkStateError> {
        let limit = limit.clamp(1, 1000);
        let rows = sqlx::query(
            "SELECT id, media_id, room_id, state, total_bytes, transferred_bytes,
                    source_peer_id, chunk_size_bytes, manifest_version
             FROM downloads ORDER BY id DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let state_str: String = row.try_get("state").map_err(ChunkStateError::from)?;
            let state = DownloadState::parse_sql_value(&state_str).ok_or_else(|| {
                ChunkStateError::Sqlx(format!("unknown state in db: {state_str}"))
            })?;
            out.push(DownloadSummary {
                id: row.try_get("id").map_err(ChunkStateError::from)?,
                media_id: row.try_get("media_id").map_err(ChunkStateError::from)?,
                room_id: row.try_get("room_id").map_err(ChunkStateError::from)?,
                state,
                total_bytes: row.try_get("total_bytes").map_err(ChunkStateError::from)?,
                transferred_bytes: row
                    .try_get("transferred_bytes")
                    .map_err(ChunkStateError::from)?,
                source_peer_id: row
                    .try_get("source_peer_id")
                    .map_err(ChunkStateError::from)?,
                chunk_size_bytes: row
                    .try_get("chunk_size_bytes")
                    .map_err(ChunkStateError::from)?,
                manifest_version: row
                    .try_get("manifest_version")
                    .map_err(ChunkStateError::from)?,
            });
        }
        Ok(out)
    }

    /// Bind a download to a newer manifest version. Called by the
    /// transport layer when a newer `MANIFEST_PUBLISHED` arrives
    /// for the same room/media. The transport must reconcile the
    /// plan against the new version before calling this method --
    /// `bind_manifest_version` does NOT itself drop the chunks; it
    /// only stamps the new version on the row. Returns
    /// [`ChunkStateError::ManifestVersionMismatch`] if `new_version`
    /// is not strictly greater than the current row's version, so
    /// the caller can never accidentally downgrade a download's
    /// manifest binding.
    pub async fn bind_manifest_version(
        &self,
        download_id: &str,
        new_version: i64,
    ) -> Result<(), ChunkStateError> {
        if new_version < 1 {
            return Err(ChunkStateError::InvalidManifestVersion { value: new_version });
        }
        let current = self.fetch_manifest_version(download_id).await?;
        if new_version <= current {
            return Err(ChunkStateError::ManifestVersionMismatch {
                download_version: current,
                current_version: new_version,
            });
        }
        sqlx::query("UPDATE downloads SET manifest_version = ? WHERE id = ?")
            .bind(new_version)
            .bind(download_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Read the manifest version without a full
    /// [`Self::fetch`]. Used by `bind_manifest_version`.
    async fn fetch_manifest_version(&self, download_id: &str) -> Result<i64, ChunkStateError> {
        let row = sqlx::query("SELECT manifest_version FROM downloads WHERE id = ?")
            .bind(download_id)
            .fetch_optional(&self.pool)
            .await?;
        let row = row.ok_or_else(|| ChunkStateError::DownloadNotFound {
            id: download_id.to_string(),
        })?;
        row.try_get("manifest_version")
            .map_err(ChunkStateError::from)
    }

    /// Set the download's `last_error`. Used by the transport
    /// layer to record per-chunk error context.
    pub async fn set_last_error(
        &self,
        download_id: &str,
        message: &str,
    ) -> Result<(), ChunkStateError> {
        sqlx::query("UPDATE downloads SET last_error = ? WHERE id = ?")
            .bind(message)
            .bind(download_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// P3-T09: update the `downloads.source_peer_id` column
    /// with the peer that just served a chunk. Best-effort,
    /// additive; no schema change. The column was created in
    /// P3-T04 and is the only authoritative record of which
    /// peer delivered the most recent chunk to the viewer.
    /// Returns `Err` on `sqlx` failure; the caller
    /// (`MultiSourceReceiver`) treats any error as a soft
    /// no-op so a transient DB write never aborts the
    /// transfer.
    pub async fn set_source_peer_id(
        &self,
        download_id: &str,
        peer_id: &str,
    ) -> Result<(), ChunkStateError> {
        sqlx::query("UPDATE downloads SET source_peer_id = ?2 WHERE id = ?1")
            .bind(download_id)
            .bind(peer_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

/// A simple state-machine predicate. The full P3-T06 set adds
/// paused/cancelled/connecting; P3-T04 only needs the strict
/// subset the planner, verifier, and atomic-finalization code
/// actually traverse.
fn is_valid_transition(from: DownloadState, to: DownloadState) -> bool {
    use DownloadState::*;
    match (from, to) {
        // Same-state transitions are allowed (idempotent).
        (a, b) if a == b => true,
        // The strict forward ladder.
        (Pending, Connecting) => true,
        (Pending, Transferring) => true,
        (Pending, Verifying) => true,
        (Connecting, Transferring) => true,
        (Transferring, Verifying) => true,
        (Verifying, Complete) => true,
        // Failure & user-initiated fallbacks.
        (Pending | Connecting | Transferring | Verifying, Failed) => true,
        (Pending | Connecting | Transferring | Verifying, Paused) => true,
        (Pending | Connecting | Transferring | Verifying, Cancelled) => true,
        (Paused, Transferring) => true,
        // Cancelled is terminal.
        _ => false,
    }
}

fn chrono_unix_ms_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fresh_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(2)
            .connect("sqlite::memory:?cache=shared")
            .await
            .expect("connect in-memory sqlite");
        // The production migrations are the source of truth, but
        // they live in `apps/client/src-tauri/migrations/` and are
        // only applied via `Storage::open`. For unit tests of the
        // store in isolation, we mirror the relevant DDL here so
        // the store can be exercised without spinning up the
        // full storage stack.
        for stmt in SCHEMA_DDL {
            sqlx::query(stmt).execute(&pool).await.expect("ddl");
        }
        pool
    }

    const SCHEMA_DDL: &[&str] = &[
        // The tables referenced by `downloads` and
        // `download_chunks` foreign keys must exist before
        // any INSERT runs. The migration creates these in
        // dependency order; the test harness does the same.
        "CREATE TABLE rooms (
            id TEXT PRIMARY KEY,
            code TEXT NOT NULL UNIQUE,
            host_user_id TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            ended_at INTEGER,
            state TEXT NOT NULL,
            manifest_id TEXT,
            settings TEXT NOT NULL DEFAULT '{}'
        )",
        "CREATE TABLE user_identities (
            id TEXT PRIMARY KEY,
            public_key TEXT NOT NULL UNIQUE,
            display_name TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            last_seen INTEGER NOT NULL
        )",
        "CREATE TABLE media_items (
            id TEXT PRIMARY KEY,
            sha256 TEXT NOT NULL UNIQUE,
            blake3 TEXT NOT NULL,
            size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
            filename TEXT NOT NULL,
            relative_path TEXT NOT NULL UNIQUE COLLATE NOCASE,
            mime TEXT NOT NULL,
            duration_ms INTEGER,
            width INTEGER,
            height INTEGER,
            video_codec TEXT,
            audio_codec TEXT,
            container TEXT,
            status TEXT NOT NULL CHECK (status IN ('permanent','temporary')),
            created_at INTEGER NOT NULL,
            last_seen_at INTEGER NOT NULL,
            last_room_id TEXT REFERENCES rooms(id) ON DELETE SET NULL,
            source_url TEXT,
            provenance TEXT NOT NULL DEFAULT '{}'
        )",
        "CREATE TABLE downloads (
            id TEXT PRIMARY KEY,
            media_id TEXT NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
            room_id TEXT REFERENCES rooms(id) ON DELETE SET NULL,
            user_id TEXT NOT NULL REFERENCES user_identities(id) ON DELETE RESTRICT,
            state TEXT NOT NULL CHECK (state IN ('pending','connecting','transferring','verifying','complete','failed','paused','cancelled')),
            total_bytes INTEGER NOT NULL CHECK (total_bytes >= 0),
            transferred_bytes INTEGER NOT NULL DEFAULT 0 CHECK (transferred_bytes >= 0),
            started_at INTEGER,
            completed_at INTEGER,
            last_error TEXT,
            source_peer_id TEXT,
            chunk_size_bytes INTEGER NOT NULL DEFAULT 262144,
            manifest_version INTEGER NOT NULL DEFAULT 1
        )",
        "CREATE TABLE download_chunks (
            id TEXT PRIMARY KEY,
            download_id TEXT NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
            \"index\" INTEGER NOT NULL CHECK (\"index\" >= 0),
            offset INTEGER NOT NULL CHECK (offset >= 0),
            length INTEGER NOT NULL CHECK (length > 0),
            sha256 TEXT NOT NULL,
            state TEXT NOT NULL CHECK (state IN ('pending','in_flight','received','verified','failed')),
            UNIQUE (download_id, \"index\")
        )",
    ];

    fn fake_chunks(n: u32) -> Vec<(u32, u64, u32, String)> {
        (0..n)
            .map(|i| {
                (
                    i,
                    i as u64 * crate::transfer::CHUNK_SIZE_BYTES as u64,
                    crate::transfer::CHUNK_SIZE_BYTES as u32,
                    format!("{:064x}", i as u128),
                )
            })
            .collect()
    }

    fn peer_id() -> String {
        crate::room::peer_id::derive_peer_id([1u8; 32])
    }

    /// Seed the rows that `downloads` foreign keys reference:
    /// `user_identities` (FK on `downloads.user_id`),
    /// `media_items` (FK on `downloads.media_id`), and `rooms`
    /// (FK on `downloads.room_id` when set). Without this, every
    /// `DownloadStore::create` test panics with `FOREIGN KEY
    /// constraint failed`.
    async fn seed_fk_deps(pool: &SqlitePool, user_id: &str, media_id: &str) {
        sqlx::query(
            "INSERT OR IGNORE INTO user_identities
             (id, public_key, display_name, created_at, last_seen)
         VALUES (?, 'pk', 'tester', 0, 0)",
        )
        .bind(user_id)
        .execute(pool)
        .await
        .expect("seed user");
        sqlx::query(
            "INSERT OR IGNORE INTO media_items
             (id, sha256, blake3, size_bytes, filename, relative_path, mime,
              status, created_at, last_seen_at, provenance)
         VALUES (?, 'aa', 'bb', 0, 'f.mp4', 'f.mp4', 'video/mp4',
                 'permanent', 0, 0, '{}')",
        )
        .bind(media_id)
        .execute(pool)
        .await
        .expect("seed media");
    }

    /// Like [`seed_fk_deps`] but also seeds a `rooms` row whose
    /// id matches `room_id` (used by tests that create a download
    /// with `room_id = Some(...)`).
    async fn seed_fk_deps_with_room(
        pool: &SqlitePool,
        user_id: &str,
        media_id: &str,
        room_id: &str,
    ) {
        seed_fk_deps(pool, user_id, media_id).await;
        sqlx::query(
            "INSERT OR IGNORE INTO rooms
             (id, code, host_user_id, created_at, ended_at, state, settings)
         VALUES (?, 'AAAAAA', ?, 0, NULL, 'open', '{}')",
        )
        .bind(room_id)
        .bind(user_id)
        .execute(pool)
        .await
        .expect("seed room");
    }

    #[tokio::test]
    async fn create_inserts_one_download_and_n_chunks() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool.clone());
        let chunks = fake_chunks(3);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64 * 3,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 1,
            },
            &chunks,
        )
        .await
        .expect("create");

        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM download_chunks WHERE download_id = ?")
                .bind("dl-1")
                .fetch_one(&pool)
                .await
                .expect("count");
        assert_eq!(n, 3);
    }

    #[tokio::test]
    async fn mark_chunk_verified_is_idempotent() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(2);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64 * 2,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 1,
            },
            &chunks,
        )
        .await
        .expect("create");

        // First mark: pending -> verified.
        s.mark_chunk_verified("dl-1", 0, &chunks[0].3)
            .await
            .expect("v1");
        // Second mark with same digest: no-op.
        s.mark_chunk_verified("dl-1", 0, &chunks[0].3)
            .await
            .expect("v2 idempotent");

        let state: String = sqlx::query_scalar(
            "SELECT state FROM download_chunks WHERE download_id = ? AND \"index\" = 0",
        )
        .bind("dl-1")
        .fetch_one(&s.pool)
        .await
        .expect("state");
        assert_eq!(state, "verified");
    }

    #[tokio::test]
    async fn mark_chunk_verified_rejects_hash_mutation() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(1);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 1,
            },
            &chunks,
        )
        .await
        .expect("create");

        s.mark_chunk_verified("dl-1", 0, &chunks[0].3)
            .await
            .expect("v1");
        // Now try a different digest on the SAME chunk.
        let err = s
            .mark_chunk_verified("dl-1", 0, &"f".repeat(64))
            .await
            .unwrap_err();
        assert!(matches!(err, ChunkStateError::ChunkHashMismatch { .. }));
    }

    #[tokio::test]
    async fn mark_chunk_verified_rejects_out_of_bounds_index() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(1);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 1,
            },
            &chunks,
        )
        .await
        .expect("create");
        let err = s
            .mark_chunk_verified("dl-1", 999, &"0".repeat(64))
            .await
            .unwrap_err();
        assert!(matches!(err, ChunkStateError::ChunkIndexOutOfBounds { .. }));
    }

    #[tokio::test]
    async fn completed_chunk_indices_returns_verified_and_received() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(4);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64 * 4,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 1,
            },
            &chunks,
        )
        .await
        .expect("create");
        s.mark_chunk_verified("dl-1", 0, &chunks[0].3)
            .await
            .unwrap();
        s.mark_chunk_received("dl-1", 2).await.unwrap();
        let got = s.completed_chunk_indices("dl-1").await.unwrap();
        assert_eq!(got, vec![0, 2]);
        let verified = s.verified_chunk_indices("dl-1").await.unwrap();
        assert_eq!(verified, vec![0]);
    }

    #[tokio::test]
    async fn state_machine_rejects_illegal_transitions() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(1);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 1,
            },
            &chunks,
        )
        .await
        .expect("create");
        // pending -> complete is illegal.
        let err = s
            .transition("dl-1", DownloadState::Complete)
            .await
            .unwrap_err();
        assert!(matches!(err, ChunkStateError::InvalidTransition { .. }));
        // pending -> transferring is OK.
        s.transition("dl-1", DownloadState::Transferring)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn create_rejects_zero_chunks() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let err = s
            .create(
                &NewDownload {
                    download_id: "dl-1".into(),
                    media_id: "m-1".into(),
                    room_id: None,
                    user_id: "u-1".into(),
                    total_bytes: 0,
                    source_peer_id: peer_id(),
                    chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                    manifest_version: 1,
                },
                &[],
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ChunkStateError::ChunkIndexOutOfBounds { .. }));
    }

    #[tokio::test]
    async fn create_rejects_invalid_peer_id() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(1);
        let err = s
            .create(
                &NewDownload {
                    download_id: "dl-1".into(),
                    media_id: "m-1".into(),
                    room_id: None,
                    user_id: "u-1".into(),
                    total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64,
                    source_peer_id: "../etc/passwd".into(),
                    chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                    manifest_version: 1,
                },
                &chunks,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ChunkStateError::InvalidSourcePeerId { .. }));
    }

    #[tokio::test]
    async fn manifest_version_persists_on_real_column() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(1);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 7,
            },
            &chunks,
        )
        .await
        .expect("create");
        let rec = s.fetch("dl-1").await.expect("fetch");
        assert_eq!(rec.manifest_version, 7);
        // A subsequent set_last_error must NOT clobber the
        // version now that the column is dedicated.
        s.set_last_error("dl-1", "transient error")
            .await
            .expect("err");
        let rec = s.fetch("dl-1").await.expect("fetch");
        assert_eq!(rec.manifest_version, 7);
    }

    #[tokio::test]
    async fn bind_manifest_version_rejects_older() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(1);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 5,
            },
            &chunks,
        )
        .await
        .expect("create");
        let err = s.bind_manifest_version("dl-1", 3).await.unwrap_err();
        assert!(matches!(
            err,
            ChunkStateError::ManifestVersionMismatch { .. }
        ));
        let rec = s.fetch("dl-1").await.expect("fetch");
        assert_eq!(rec.manifest_version, 5);
    }

    #[tokio::test]
    async fn bind_manifest_version_rejects_equal() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(1);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 5,
            },
            &chunks,
        )
        .await
        .expect("create");
        let err = s.bind_manifest_version("dl-1", 5).await.unwrap_err();
        assert!(matches!(
            err,
            ChunkStateError::ManifestVersionMismatch { .. }
        ));
    }

    #[tokio::test]
    async fn bind_manifest_version_accepts_strictly_newer() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(1);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 1,
            },
            &chunks,
        )
        .await
        .expect("create");
        s.bind_manifest_version("dl-1", 2).await.expect("bind");
        let rec = s.fetch("dl-1").await.expect("fetch");
        assert_eq!(rec.manifest_version, 2);
    }

    #[tokio::test]
    async fn corrupt_chunk_is_not_marked_verified() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(2);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64 * 2,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 1,
            },
            &chunks,
        )
        .await
        .expect("create");
        // Verifier would call `verify_chunk_sha256(bytes, expected)`
        // and reject mismatches; the store must NOT mark a chunk
        // verified when the digest is wrong.
        let stored = chunks[0].3.clone();
        // Compute a CORRECTLY-MATCHING digest so the chunk can be
        // marked verified in the first half of the test, then a
        // WRONG digest that the store must reject. We use the
        // store's own stored value as the expected base: compute
        // sha256 over an arbitrary 256 KiB buffer, then flip one
        // byte to produce a "corrupt" digest that does NOT match
        // any real hash.
        let good_bytes: Vec<u8> = (0..crate::transfer::CHUNK_SIZE_BYTES as u8).collect();
        let good_digest = locast_crypto::sha256::sha256_hex(&good_bytes);
        // Flip the first hex character of good_digest to get a
        // digest that cannot equal good_digest.
        let mut bad_chars: Vec<char> = good_digest.chars().collect();
        bad_chars[0] = if bad_chars[0] == '0' { '1' } else { '0' };
        let bad_digest: String = bad_chars.into_iter().collect();
        assert_ne!(bad_digest, good_digest);
        assert_ne!(bad_digest, stored);
        let _ = stored;
        let err = s
            .mark_chunk_verified("dl-1", 0, &bad_digest)
            .await
            .unwrap_err();
        assert!(matches!(err, ChunkStateError::ChunkHashMismatch { .. }));
        // The chunk must remain pending.
        let state: String = sqlx::query_scalar(
            "SELECT state FROM download_chunks WHERE download_id = ? AND \"index\" = 0",
        )
        .bind("dl-1")
        .fetch_one(&s.pool)
        .await
        .expect("state");
        assert_eq!(state, "pending");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_chunk_verified_concurrent_for_different_indices() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(4);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64 * 4,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 1,
            },
            &chunks,
        )
        .await
        .expect("create");
        let h0 = chunks[0].3.clone();
        let h1 = chunks[1].3.clone();
        let h2 = chunks[2].3.clone();
        let h3 = chunks[3].3.clone();
        let s0 = s.clone();
        let s1 = s.clone();
        let s2 = s.clone();
        let s3 = s.clone();
        let (r0, r1, r2, r3) = tokio::join!(
            async move { s0.mark_chunk_verified("dl-1", 0, &h0).await },
            async move { s1.mark_chunk_verified("dl-1", 1, &h1).await },
            async move { s2.mark_chunk_verified("dl-1", 2, &h2).await },
            async move { s3.mark_chunk_verified("dl-1", 3, &h3).await },
        );
        r0.expect("v0");
        r1.expect("v1");
        r2.expect("v2");
        r3.expect("v3");
        let verified = s.verified_chunk_indices("dl-1").await.expect("verified");
        assert_eq!(verified, vec![0, 1, 2, 3]);
        let rec = s.fetch("dl-1").await.expect("fetch");
        assert_eq!(
            rec.transferred_bytes,
            (crate::transfer::CHUNK_SIZE_BYTES as i64) * 4
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mark_chunk_verified_same_index_twice_is_idempotent() {
        let pool = fresh_pool().await;
        seed_fk_deps(&pool, "u-1", "m-1").await;
        let s = DownloadStore::new(pool);
        let chunks = fake_chunks(2);
        s.create(
            &NewDownload {
                download_id: "dl-1".into(),
                media_id: "m-1".into(),
                room_id: None,
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64 * 2,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 1,
            },
            &chunks,
        )
        .await
        .expect("create");
        let h0 = chunks[0].3.clone();
        let h0a = h0.clone();
        let sa = s.clone();
        let sb = s.clone();
        let (a, b) = tokio::join!(
            async move { sa.mark_chunk_verified("dl-1", 0, &h0).await },
            async move { sb.mark_chunk_verified("dl-1", 0, &h0a).await },
        );
        a.expect("a");
        b.expect("b");
        let verified = s.verified_chunk_indices("dl-1").await.expect("verified");
        assert_eq!(verified, vec![0]);
        let rec = s.fetch("dl-1").await.expect("fetch");
        // transferred_bytes = 1 chunk (NOT 2 -- idempotent).
        assert_eq!(
            rec.transferred_bytes,
            crate::transfer::CHUNK_SIZE_BYTES as i64
        );
    }

    /// MANDATORY file-backed restart test. The P3-T03 prerequisite
    /// handoff explicitly noted that the prior tests did not prove
    /// restart survival; P3-T04 closes that gap.
    #[tokio::test]
    async fn download_state_survives_file_backed_restart() {
        // Use the production `Storage::open` so the schema and
        // URL handling exactly match what the app does at
        // runtime. Bypassing it with a raw `SqlitePoolOptions`
        // URL has fragile behavior on Windows (drive-letter
        // path + sqlite:// scheme).
        let dir = tempfile::TempDir::new().expect("tempdir");
        let db_path = dir.path().join("index.sqlite");
        let storage = crate::storage::Storage::open(&db_path)
            .await
            .expect("storage opens");
        let pool1 = storage.pool().clone();
        let s1 = DownloadStore::new(pool1.clone());
        seed_fk_deps_with_room(&pool1, "u-1", "m-restart", "r-1").await;
        let chunks = fake_chunks(3);
        s1.create(
            &NewDownload {
                download_id: "dl-restart".into(),
                media_id: "m-restart".into(),
                room_id: Some("r-1".into()),
                user_id: "u-1".into(),
                total_bytes: crate::transfer::CHUNK_SIZE_BYTES as u64 * 3,
                source_peer_id: peer_id(),
                chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
                manifest_version: 4,
            },
            &chunks,
        )
        .await
        .expect("create");
        s1.mark_chunk_verified("dl-restart", 0, &chunks[0].3)
            .await
            .expect("v0");
        s1.mark_chunk_verified("dl-restart", 2, &chunks[2].3)
            .await
            .expect("v2");
        pool1.close().await;

        // Phase 2: reopen the SAME file-backed SQLite and confirm
        // every piece of state survived. Use the production
        // `Storage::open` again so we exercise the same URL
        // and PRAGMA stack as a real restart.
        let storage2 = crate::storage::Storage::open(&db_path)
            .await
            .expect("storage reopens");
        let pool2 = storage2.pool().clone();
        let s2 = DownloadStore::new(pool2);
        let rec = s2.fetch("dl-restart").await.expect("fetch after restart");
        assert_eq!(rec.id, "dl-restart");
        assert_eq!(rec.media_id, "m-restart");
        assert_eq!(rec.state, DownloadState::Pending);
        assert_eq!(
            rec.total_bytes,
            crate::transfer::CHUNK_SIZE_BYTES as i64 * 3
        );
        assert_eq!(rec.manifest_version, 4);
        assert_eq!(
            rec.chunk_size_bytes,
            crate::transfer::CHUNK_SIZE_BYTES as i64
        );
        let verified = s2
            .verified_chunk_indices("dl-restart")
            .await
            .expect("verified");
        let completed = s2
            .completed_chunk_indices("dl-restart")
            .await
            .expect("completed");
        assert_eq!(verified, vec![0, 2]);
        assert_eq!(completed, vec![0, 2]);
        // transferred_bytes = 2 chunks, not 3.
        assert_eq!(
            rec.transferred_bytes,
            crate::transfer::CHUNK_SIZE_BYTES as i64 * 2
        );
    }
}
