//! P0-T05 acceptance test: the storage layer opens a SQLite database on a
//! `tempfile::TempDir`, runs the embedded migrations, and exposes the
//! schema described in `docs/ARCHITECTURE.md` section 7.
//!
//! Run with `cargo test --workspace -p locast-client --test storage_init`
//! or simply `cargo test --workspace`.

use std::path::Path;

use locast_client_lib::storage::Storage;
use sqlx::Row;
use tempfile::TempDir;

const EXPECTED_TABLES: &[&str] = &[
    "media_items",
    "rooms",
    "room_manifests",
    "room_participants",
    "downloads",
    "download_chunks",
    "room_events",
    "presence",
    "user_identities",
    "room_invites",
    "settings",
    "media_subtitles",
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_init_creates_schema() {
    let tmp = TempDir::new().expect("create tempdir");
    let db_path = tmp.path().join("index.sqlite");
    let storage = Storage::open(&db_path).await.expect("storage should open");

    // The on-disk file must exist after open().
    assert!(
        Path::new(storage.path()).exists(),
        "storage database file should exist at {:?}",
        storage.path(),
    );

    // All 12 base tables must exist.
    let mut missing: Vec<&str> = Vec::new();
    for table in EXPECTED_TABLES {
        let row = sqlx::query("SELECT 1 AS x FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(table)
            .fetch_optional(&storage.pool())
            .await
            .expect("query sqlite_master");
        if row.is_none() {
            missing.push(table);
        }
    }
    assert!(missing.is_empty(), "missing expected tables: {missing:?}");

    // FTS5 virtual table for library search must exist.
    let fts = sqlx::query("SELECT 1 AS x FROM sqlite_master WHERE type = 'table' AND name = ?")
        .bind("media_items_fts")
        .fetch_optional(&storage.pool())
        .await
        .expect("query sqlite_master for fts");
    assert!(
        fts.is_some(),
        "FTS5 virtual table media_items_fts must exist"
    );

    // PRAGMA contract from docs/ARCHITECTURE.md section 7.
    let journal_mode: String = sqlx::query("PRAGMA journal_mode")
        .fetch_one(&storage.pool())
        .await
        .expect("query journal_mode")
        .get(0);
    assert_eq!(
        journal_mode.to_ascii_lowercase(),
        "wal",
        "journal_mode must be WAL (got {journal_mode:?})"
    );

    let foreign_keys: i64 = sqlx::query("PRAGMA foreign_keys")
        .fetch_one(&storage.pool())
        .await
        .expect("query foreign_keys")
        .get(0);
    assert_eq!(foreign_keys, 1, "foreign_keys must be ON");

    let busy_timeout: i64 = sqlx::query("PRAGMA busy_timeout")
        .fetch_one(&storage.pool())
        .await
        .expect("query busy_timeout")
        .get(0);
    assert_eq!(
        busy_timeout, 5000,
        "busy_timeout must be 5000 ms (got {busy_timeout})"
    );

    // Sanity: the migrator recorded the 0001_init migration.
    let applied: i64 = sqlx::query("SELECT COUNT(*) FROM _sqlx_migrations WHERE version = ?")
        .bind(1_i64)
        .fetch_one(&storage.pool())
        .await
        .expect("query _sqlx_migrations")
        .get(0);
    assert_eq!(applied, 1, "_sqlx_migrations must record 0001_init");
}
