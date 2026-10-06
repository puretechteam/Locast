//! Startup purge of orphaned `tmp/staging` and `tmp/incomplete` bytes.

use locast_client_lib::library::purge::{purge_stale_tmp, PurgeReport};
use locast_client_lib::storage::Storage;
use tempfile::TempDir;

async fn seed_download(pool: &sqlx::SqlitePool, id: &str, state: &str) {
    sqlx::query(
        "INSERT OR IGNORE INTO user_identities \
            (id, public_key, display_name, created_at, last_seen) \
         VALUES ('u', 'pk-u', 'tester', 0, 0)",
    )
    .execute(pool)
    .await
    .expect("user");
    let media_id = format!("m-{id}");
    let sha = format!("{:0>64}", id.replace('-', ""));
    sqlx::query(
        "INSERT INTO media_items (\
            id, sha256, blake3, size_bytes, filename, relative_path, mime, \
            status, created_at, last_seen_at, provenance\
         ) VALUES (?1, ?2, 'b', 10, 'f', ?3, 'application/octet-stream', \
            'temporary', 1, 1, '{}')",
    )
    .bind(&media_id)
    .bind(&sha)
    .bind(format!("library/{id}/f"))
    .execute(pool)
    .await
    .expect("media");
    sqlx::query(
        "INSERT INTO downloads (id, media_id, user_id, state, total_bytes) \
         VALUES (?1, ?2, 'u', ?3, 10)",
    )
    .bind(id)
    .bind(&media_id)
    .bind(state)
    .execute(pool)
    .await
    .expect("download");
}

#[tokio::test]
async fn purge_removes_orphans_and_keeps_resumable_downloads() {
    let dir = TempDir::new().expect("tempdir");
    let storage = Storage::open(&dir.path().join("index.sqlite"))
        .await
        .expect("storage");
    let root = dir.path().join("lib");
    let tmp = root.join("tmp");
    for sub in [
        "staging/aa11",
        "incomplete/paused-1",
        "incomplete/failed-1",
        "incomplete/done-1",
        "incomplete/orphan-1",
    ] {
        std::fs::create_dir_all(tmp.join(sub)).expect("mkdir");
        std::fs::write(tmp.join(sub).join("x.bin"), b"data").expect("write");
    }
    seed_download(&storage.pool(), "paused-1", "paused").await;
    seed_download(&storage.pool(), "failed-1", "failed").await;
    seed_download(&storage.pool(), "done-1", "complete").await;

    let report = purge_stale_tmp(&storage, &root).await.expect("purge");

    assert_eq!(
        report,
        PurgeReport {
            staging_removed: 1,
            incomplete_removed: 3
        }
    );
    assert!(!tmp.join("staging/aa11").exists());
    assert!(tmp.join("incomplete/paused-1/x.bin").exists());
    for gone in ["failed-1", "done-1", "orphan-1"] {
        assert!(!tmp.join("incomplete").join(gone).exists(), "{gone}");
    }
}

#[tokio::test]
async fn purge_without_tmp_dirs_is_a_noop() {
    let dir = TempDir::new().expect("tempdir");
    let storage = Storage::open(&dir.path().join("index.sqlite"))
        .await
        .expect("storage");
    let report = purge_stale_tmp(&storage, &dir.path().join("missing"))
        .await
        .expect("purge");
    assert_eq!(report, PurgeReport::default());
}
