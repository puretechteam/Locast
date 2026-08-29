//! Locast local storage layer.
//!
//! P0-T05 establishes the SQLite-backed storage module for the desktop
//! client. The module owns a `sqlx::SqlitePool`, sets the architecture's
//! PRAGMAs on every new connection, and applies the embedded migrations
//! at startup.
//!
//! The architecture's storage section (docs/ARCHITECTURE.md section 7)
//! calls for a single writer under a `tokio::sync::Mutex` plus a reader
//! pool with `max_connections = 8`. P0-T05 implements the pool and the
//! PRAGMAs; the application-level write mutex is added when the first
//! command lands in P0-T06.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;
use thiserror::Error;
use tracing::info;

/// P1-T05 added the settings repository (`storage::settings`). It is
/// a thin typed wrapper over the `settings` table that the P0-T05
/// migration introduced. The `Storage` type itself is unchanged.
pub mod settings;

/// P2-T08 added the recents repository (`storage::rooms`). It is a
/// thin typed wrapper over the `recent_rooms` table introduced by
/// the `0002_recent_rooms` migration. The `Storage` type itself is
/// unchanged.
pub mod rooms;

/// P3-T03 added the manifest repository (`storage::manifests`). It
/// is a thin typed wrapper over the `room_manifests` table that
/// the P0-T05 migration introduced. The `Storage` type itself is
/// unchanged.
pub mod manifests;

/// Default pool size per `docs/ARCHITECTURE.md` section 7.
pub const DEFAULT_POOL_SIZE: u32 = 8;

/// Default busy timeout per the architecture's PRAGMA table.
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Embedded migrations. `sqlx::migrate!` resolves the path relative to
/// `CARGO_MANIFEST_DIR`, which is `apps/client/src-tauri/`, so the
/// directory it points at is `apps/client/src-tauri/migrations/` per
/// `docs/ARCHITECTURE.md` section 26.2.1.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Storage handle. Owns the SQLite connection pool and is the only
/// object the rest of the Rust core should hold to talk to the
/// library's local database.
#[derive(Clone)]
pub struct Storage {
    pool: SqlitePool,
    path: PathBuf,
}

/// Errors raised by `Storage::open`.
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("storage path is empty")]
    EmptyPath,

    #[error("failed to create storage parent directory {path}: {source}")]
    CreateParent {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse storage path: {0}")]
    Options(#[from] sqlx::Error),

    #[error("failed to open SQLite pool: {0}")]
    Pool(#[source] sqlx::Error),

    #[error("storage migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
}

impl Storage {
    /// Open a SQLite database at `path`, set the architecture's PRAGMAs
    /// on every new connection, and apply the embedded migrations.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        if path.as_os_str().is_empty() {
            return Err(StorageError::EmptyPath);
        }

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    StorageError::CreateParent {
                        path: parent.to_path_buf(),
                        source: e,
                    }
                })?;
            }
        }

        let connect_url = format!("sqlite://{}", path.display());
        // `SqliteConnectOptions::pragma` is the documented per-connection
        // PRAGMA setter in sqlx 0.8. The builder applies each `pragma`
        // call to every new connection the pool opens. The value must
        // stringify into a `Cow<'static, str>`, so numeric PRAGMAs are
        // formatted here.
        let options = SqliteConnectOptions::from_str(&connect_url)
            .map_err(StorageError::Options)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(DEFAULT_BUSY_TIMEOUT)
            .foreign_keys(true)
            .pragma("temp_store", "MEMORY")
            .pragma("cache_size", "-64000")
            .pragma("mmap_size", "268435456");

        let pool = SqlitePoolOptions::new()
            .max_connections(DEFAULT_POOL_SIZE)
            .connect_with(options)
            .await
            .map_err(StorageError::Pool)?;

        info!(path = %path.display(), "locast storage pool opened; running migrations");
        MIGRATOR.run(&pool).await?;

        Ok(Self { pool, path })
    }

    /// Returns a clone of the underlying pool. Callers that need a
    /// `&Storage` for ad-hoc queries can use this; the canonical API
    /// lands when the repository layer is added in P1+.
    pub fn pool(&self) -> SqlitePool {
        self.pool.clone()
    }

    /// Returns the on-disk path of the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}
