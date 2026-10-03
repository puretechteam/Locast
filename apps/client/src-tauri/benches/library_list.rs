//! P8-T06 bench: library list query (ARCHITECTURE 28.10 hot path 2).
//!
//! 10k `media_items` rows are bulk-inserted into a real `Storage`
//! database. Two queries are timed:
//!
//! * `library_list/page_10k_rows` - the keyset-paginated browse page
//!   (newest first, 100 rows).
//! * `library_list/fts_search_10k_rows` - an FTS5 prefix search.
//!
//! There is no Rust "list library" function yet (the library page reads
//! through SQL), so these time the SQL against the real schema and
//! indexes. Gate from the architecture: p99 < 5 ms.

use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion};
use locast_client_lib::storage::Storage;
use sqlx::Row;
use tempfile::TempDir;

const ROWS: i64 = 10_000;
const PAGE: i64 = 100;

async fn seed(storage: &Storage) {
    let pool = storage.pool();
    let mut tx = pool.begin().await.expect("begin");
    for i in 0..ROWS {
        sqlx::query(
            "INSERT INTO media_items \
             (id, sha256, blake3, size_bytes, filename, relative_path, mime, status, \
              created_at, last_seen_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'video/mp4', 'permanent', ?7, ?7)",
        )
        .bind(format!("id-{i:08}"))
        .bind(format!("{i:064x}"))
        .bind(format!("{i:064x}"))
        .bind(1_000_000 + i)
        .bind(format!("Movie {i:05}.mp4"))
        .bind(format!("library/{i:05}/Movie {i:05}.mp4"))
        .bind(1_700_000_000_000_i64 + i)
        .execute(&mut *tx)
        .await
        .expect("insert row");
    }
    tx.commit().await.expect("commit");
}

fn bench_list(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let dir = TempDir::new().expect("tempdir");
    let storage = rt.block_on(async {
        let storage = Storage::open(dir.path().join("index.sqlite"))
            .await
            .expect("storage opens");
        seed(&storage).await;
        storage
    });
    let pool = storage.pool();

    let mut group = c.benchmark_group("library_list");
    group.sample_size(50);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(3));

    group.bench_function("page_10k_rows", |b| {
        b.to_async(&rt).iter(|| async {
            let rows = sqlx::query(
                "SELECT id, filename, size_bytes, status, last_seen_at FROM media_items \
                 WHERE (last_seen_at, id) < (?1, ?2) \
                 ORDER BY last_seen_at DESC, id DESC LIMIT ?3",
            )
            .bind(1_700_000_000_000_i64 + ROWS / 2)
            .bind("id-zzzzzzzz")
            .bind(PAGE)
            .fetch_all(&pool)
            .await
            .expect("page query");
            assert_eq!(rows.len() as i64, PAGE);
        });
    });

    group.bench_function("fts_search_10k_rows", |b| {
        b.to_async(&rt).iter(|| async {
            let row = sqlx::query(
                "SELECT COUNT(*) AS c FROM (SELECT rowid FROM media_items_fts \
                 WHERE media_items_fts MATCH 'movie*' LIMIT ?1)",
            )
            .bind(PAGE)
            .fetch_one(&pool)
            .await
            .expect("fts query");
            assert_eq!(row.get::<i64, _>("c"), PAGE);
        });
    });

    group.finish();
}

criterion_group!(benches, bench_list);
criterion_main!(benches);
