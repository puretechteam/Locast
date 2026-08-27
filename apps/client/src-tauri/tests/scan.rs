//! P1-T07 integration test: `library_scan`.
//!
//! Run with `cargo test -p locast-client --test scan` or simply
//! `cargo test --workspace`.
//!
//! # What's pinned
//!
//! The roadmap's P1-T07 acceptance is:
//!
//! > integration test with a `tempfile::TempDir` containing 50 fixture
//! > files of varying sizes; after `library_scan`, `SELECT COUNT(*)
//! > FROM media_items = 50`; FTS5 returns the expected matches for
//! > `MATCH 'movie*'`.
//!
//! The tests below mirror the spec; the `[spec]` comment in each
//! test names the acceptance criterion it covers.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use locast_client_lib::core::quota::QuotaAccountant;
use locast_client_lib::library::scan::ScanResult;
use locast_client_lib::storage::Storage;
use sqlx::Row;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// shared test helpers
// ---------------------------------------------------------------------------

static TEMPDIRS: Mutex<Vec<TempDir>> = Mutex::new(Vec::new());

/// Create a fresh tempdir, persist it for the duration of the
/// process, and return its path. Mirrors the P1-T04 / P1-T05 helper.
fn new_tempdir() -> PathBuf {
    let d = TempDir::new().expect("tempdir");
    let p = d.path().to_path_buf();
    TEMPDIRS.lock().expect("tempdir holders").push(d);
    p
}

/// Open a `Storage` on a fresh SQLite file in a fresh tempdir.
async fn open_storage() -> (Storage, PathBuf) {
    let dir = new_tempdir();
    let db = dir.join("index.sqlite");
    let storage = Storage::open(&db).await.expect("storage opens");
    (storage, dir)
}

/// Make a `library` directory under `dir` and return the path.
/// The library root is the tempdir itself (so the SQLite file's
/// parent IS the library root, matching the Tauri command's
/// `storage.path().parent()` convention).
fn make_library_root(dir: &Path) -> PathBuf {
    let root = dir.join("library_root");
    std::fs::create_dir_all(&root).expect("create library root");
    root
}

/// Build a `QuotaAccountant` against the given storage.
fn open_accountant(storage: &Storage) -> QuotaAccountant {
    QuotaAccountant::new(storage.clone())
}

/// Compute the content-addressed path for a given sha and
/// filename, under `library_root`.
fn cap_path(library_root: &Path, sha: &str, filename: &str) -> PathBuf {
    let mut p = library_root.join("library");
    p.push(&sha[0..2]);
    p.push(&sha[2..4]);
    p.push(sha);
    p.push(filename);
    p
}

/// SHA-256 hex of `bytes`. Convenience for the fixture builders.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Count rows in `media_items`.
async fn count_media_items(storage: &Storage) -> i64 {
    sqlx::query("SELECT COUNT(*) AS c FROM media_items")
        .fetch_one(&storage.pool())
        .await
        .expect("count")
        .get("c")
}

/// Write `bytes` to the content-addressed path for `sha` and
/// `filename`. Creates the parent directory if missing.
fn write_cap_file(library_root: &Path, sha: &str, filename: &str, bytes: &[u8]) -> PathBuf {
    let p = cap_path(library_root, sha, filename);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdir cap parent");
    }
    std::fs::write(&p, bytes).expect("write cap file");
    p
}

// ---------------------------------------------------------------------------
// 1. 50 fixture files -> 50 rows
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_finds_50_fixture_files() {
    // [spec] P1-T07: 50 fixture files => 50 media_items rows.
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    // Build 50 distinct sha + 1 KiB content per file. The
    // filenames are arbitrary sanitized names; they live under
    // their own content-addressed directories.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut fixtures: Vec<(String, String, Vec<u8>)> = Vec::new();
    for i in 0..50u32 {
        // Per-file distinct content: each fixture's byte at
        // offset 0 is `i`, so all 50 SHA-256s are distinct.
        // The 1 KiB body is otherwise deterministic so the
        // test is reproducible.
        let mut bytes: Vec<u8> = Vec::with_capacity(1024);
        bytes.push(i as u8);
        for j in 1..1024u32 {
            bytes.push(((i.wrapping_mul(7) ^ j) & 0xFF) as u8);
        }
        assert_eq!(bytes.len(), 1024);
        let sha = sha256_hex(&bytes);
        assert!(seen.insert(sha.clone()), "fixture shas must be unique");
        let filename = format!("Movie{i:02}.mkv");
        fixtures.push((sha, filename, bytes));
    }
    assert_eq!(fixtures.len(), 50);

    for (sha, filename, bytes) in &fixtures {
        write_cap_file(&lib_root, sha, filename, bytes);
    }

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_scanned, 50, "50 files scanned");
    assert_eq!(result.files_upserted, 50, "50 files upserted");
    assert_eq!(result.files_missing, 0, "0 missing");
    assert_eq!(result.files_failed, 0, "0 failed");
    assert_eq!(
        result.files_orphans_discovered, 50,
        "50 orphans (all inserted)"
    );

    let count = count_media_items(&storage).await;
    assert_eq!(count, 50, "50 media_items rows");
}

/// Test wrapper that runs the Tauri command's underlying
/// `library::scan::scan` directly with the same
/// `library_root`-resolution logic the command uses, without
/// instantiating a Tauri runtime. Mirrors
/// `commands::scan::library_scan`.
async fn library_scan_test(
    storage: &Storage,
    _accountant: &QuotaAccountant,
    lib_root: &Path,
) -> ScanResult {
    use locast_client_lib::library::scan;
    scan::scan(storage, lib_root).await.expect("scan ok")
}

// ---------------------------------------------------------------------------
// 2. 50 fixture files with movie*/show* split + FTS5
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_finds_50_and_fts5_matches_movie_prefix() {
    // [spec] P1-T07: FTS5 MATCH 'movie*' returns the expected
    // count (30 if 30 of 50 filenames start with "movie").
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    let mut fixtures: Vec<(String, String, Vec<u8>)> = Vec::new();
    for i in 0..30u32 {
        let mut bytes: Vec<u8> = Vec::with_capacity(512);
        bytes.push(i as u8);
        for j in 1..512u32 {
            bytes.push(((i.wrapping_mul(11) ^ j) & 0xFF) as u8);
        }
        let sha = sha256_hex(&bytes);
        let filename = format!("movie_{i:02}.mkv");
        fixtures.push((sha, filename, bytes));
    }
    for i in 0..20u32 {
        let mut bytes: Vec<u8> = Vec::with_capacity(512);
        bytes.push((i + 100) as u8);
        for j in 1..512u32 {
            bytes.push((((i + 30).wrapping_mul(13) ^ j) & 0xFF) as u8);
        }
        let sha = sha256_hex(&bytes);
        let filename = format!("show_{i:02}.mkv");
        fixtures.push((sha, filename, bytes));
    }
    assert_eq!(fixtures.len(), 50);
    for (sha, filename, bytes) in &fixtures {
        write_cap_file(&lib_root, sha, filename, bytes);
    }

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_scanned, 50);
    assert_eq!(result.files_upserted, 50);

    let count = count_media_items(&storage).await;
    assert_eq!(count, 50);

    // FTS5 query: 30 of 50 filenames start with "movie".
    let fts_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM media_items_fts WHERE media_items_fts MATCH 'movie*'",
    )
    .fetch_one(&storage.pool())
    .await
    .expect("fts count")
    .get("c");
    assert_eq!(
        fts_count, 30,
        "FTS5 MATCH 'movie*' should return 30 rows (the 30 movie_*.mkv filenames)"
    );

    // Sanity: the show_* filenames match the 'show*' prefix.
    let fts_show: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM media_items_fts WHERE media_items_fts MATCH 'show*'",
    )
    .fetch_one(&storage.pool())
    .await
    .expect("fts show count")
    .get("c");
    assert_eq!(fts_show, 20, "FTS5 MATCH 'show*' should return 20 rows");
}

// ---------------------------------------------------------------------------
// 3-6. skip non-library files
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_skips_tmp_staging() {
    // [spec] P1-T07: a tmp/staging/<id>/foo.partial file is NOT
    // counted.
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    // Place a real content-addressed file so the scanner has
    // one row to insert.
    let bytes = b"some real content".to_vec();
    let sha = sha256_hex(&bytes);
    write_cap_file(&lib_root, &sha, "Real.mkv", &bytes);

    // A non-media file under tmp/staging/<id>/foo.partial.
    let staging = lib_root.join("tmp").join("staging").join("dl-id");
    std::fs::create_dir_all(&staging).expect("mkdir staging");
    std::fs::write(staging.join("foo.partial"), b"partial bytes").expect("write partial");

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_scanned, 1, "tmp/staging/ files are not media");
    assert_eq!(count_media_items(&storage).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_skips_tmp_incomplete() {
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    let bytes = b"real content again".to_vec();
    let sha = sha256_hex(&bytes);
    write_cap_file(&lib_root, &sha, "Real.mkv", &bytes);

    let incomplete = lib_root.join("tmp").join("incomplete").join("dl-id");
    std::fs::create_dir_all(&incomplete).expect("mkdir incomplete");
    std::fs::write(incomplete.join("foo.part.0"), b"chunk 0").expect("write chunk");

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(
        result.files_scanned, 1,
        "tmp/incomplete/ files are not media"
    );
    assert_eq!(count_media_items(&storage).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_skips_trash() {
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    let bytes = b"real content trash".to_vec();
    let sha = sha256_hex(&bytes);
    write_cap_file(&lib_root, &sha, "Real.mkv", &bytes);

    let trash = lib_root.join("trash");
    std::fs::create_dir_all(&trash).expect("mkdir trash");
    std::fs::write(trash.join("foo.mkv"), b"trash bytes").expect("write trash");

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_scanned, 1, "trash/ files are not media");
    assert_eq!(count_media_items(&storage).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_skips_sqlite_files() {
    // [spec] P1-T07: index.sqlite, index.sqlite-wal,
    // index.sqlite-shm are NOT counted as media. The scanner
    // walks only library/, so files at the library root are
    // structurally outside the walk.
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    // Touch the SQLite sidecar files at the library root.
    std::fs::write(lib_root.join("index.sqlite"), b"sqlite data").expect("write sqlite");
    std::fs::write(lib_root.join("index.sqlite-wal"), b"wal data").expect("write wal");
    std::fs::write(lib_root.join("index.sqlite-shm"), b"shm data").expect("write shm");

    // One valid media file for contrast.
    let bytes = b"a media file".to_vec();
    let sha = sha256_hex(&bytes);
    write_cap_file(&lib_root, &sha, "Real.mkv", &bytes);

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_scanned, 1, "SQLite files are not media");
    assert_eq!(count_media_items(&storage).await, 1);
}

// ---------------------------------------------------------------------------
// 7. orphan recovery
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_recovers_orphan_content_addressed_file() {
    // [spec] P1-T07: a content-addressed file under
    // library/<sha[0..2]>/<sha[2..4]>/<sha>/foo.mkv with a real
    // on-disk file but NO DB row is recovered: SHA-256 + BLAKE3
    // computed, row inserted, provenance contains "orphan".
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    let bytes: Vec<u8> = (0u32..512).map(|i| (i & 0xFF) as u8).collect();
    let expected_sha = sha256_hex(&bytes);
    let sha_short = &expected_sha[0..2];
    let expected_path = cap_path(&lib_root, &expected_sha, "foo.mkv");
    assert!(expected_path.starts_with(lib_root.join("library").join(sha_short)));

    write_cap_file(&lib_root, &expected_sha, "foo.mkv", &bytes);

    // Sanity: the table is empty before the scan.
    let pre: i64 = count_media_items(&storage).await;
    assert_eq!(pre, 0, "no rows before scan");

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_scanned, 1);
    assert_eq!(result.files_upserted, 1);
    assert_eq!(result.files_orphans_discovered, 1);
    assert_eq!(count_media_items(&storage).await, 1);

    // The row's sha256, size_bytes, filename, relative_path, and
    // provenance match the on-disk state.
    let row = sqlx::query(
        "SELECT sha256, blake3, size_bytes, filename, relative_path, provenance \
         FROM media_items",
    )
    .fetch_one(&storage.pool())
    .await
    .expect("row present");
    let sha256: String = row.get("sha256");
    let size_bytes: i64 = row.get("size_bytes");
    let filename: String = row.get("filename");
    let relative_path: String = row.get("relative_path");
    let provenance: String = row.get("provenance");
    assert_eq!(sha256, expected_sha);
    assert_eq!(size_bytes, bytes.len() as i64);
    assert_eq!(filename, "foo.mkv");
    assert_eq!(
        relative_path,
        format!(
            "library/{}/{}/{}/foo.mkv",
            &expected_sha[0..2],
            &expected_sha[2..4],
            expected_sha
        )
    );
    assert!(
        provenance.contains("orphan"),
        "provenance should mark this as an orphan-recovery, got {provenance:?}"
    );
    assert!(provenance.contains("library-scan"));

    // FTS5 trigger fires. Querying the rowid directly should
    // return a match for the filename.
    let fts_count: i64 =
        sqlx::query("SELECT COUNT(*) AS c FROM media_items_fts WHERE media_items_fts MATCH 'foo*'")
            .fetch_one(&storage.pool())
            .await
            .expect("fts count")
            .get("c");
    assert_eq!(fts_count, 1, "FTS5 trigger fired for the orphan row");
}

// ---------------------------------------------------------------------------
// 8. updated file (rename)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_upserts_existing_media() {
    // [spec] P1-T07: an existing media row whose on-disk file
    // has been moved / renamed is UPDATED in place. The row's
    // `filename` and `relative_path` track the on-disk state.
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    let bytes: Vec<u8> = (0u32..256).map(|i| (i & 0xFF) as u8).collect();
    let sha = sha256_hex(&bytes);

    // First, write a content-addressed file with name
    // "Original.mkv".
    write_cap_file(&lib_root, &sha, "Original.mkv", &bytes);

    let r1 = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(r1.files_upserted, 1);

    // Now move the on-disk file to a new filename with the SAME
    // content (same sha). The relative_path must change.
    let original = cap_path(&lib_root, &sha, "Original.mkv");
    let renamed = cap_path(&lib_root, &sha, "Renamed.mkv");
    if let Some(parent) = renamed.parent() {
        std::fs::create_dir_all(parent).expect("mkdir renamed parent");
    }
    std::fs::rename(&original, &renamed).expect("rename");

    let r2 = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(r2.files_scanned, 1);
    assert_eq!(r2.files_upserted, 1, "renamed file => 1 UPDATE");

    // Exactly one row, with the new filename / relative_path.
    assert_eq!(count_media_items(&storage).await, 1);
    let row = sqlx::query("SELECT filename, relative_path FROM media_items WHERE sha256 = ?1")
        .bind(&sha)
        .fetch_one(&storage.pool())
        .await
        .expect("row present");
    let filename: String = row.get("filename");
    let relative_path: String = row.get("relative_path");
    assert_eq!(filename, "Renamed.mkv");
    assert_eq!(
        relative_path,
        format!("library/{}/{}/{}/Renamed.mkv", &sha[0..2], &sha[2..4], sha)
    );
}

// ---------------------------------------------------------------------------
// 9. missing file -> last_seen_at bump
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_marks_missing_files_with_updated_last_seen_at() {
    // [spec] P1-T07: an existing media row whose on-disk file
    // is missing is NOT deleted; `last_seen_at` is bumped; the
    // `files_missing` count is 1.
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    // Insert a media row directly via SQL. We bypass the
    // content-addressed tree entirely; the relative_path points
    // at a file we never create.
    let id = uuid::Uuid::new_v4().to_string();
    let sha = "a".repeat(64);
    let rel = format!("library/aa/aa/{sha}/missing.mkv");
    let now_ms: i64 = 1_000_000;
    sqlx::query(
        "INSERT INTO media_items (\
            id, sha256, blake3, size_bytes, filename, relative_path, \
            mime, duration_ms, width, height, video_codec, audio_codec, \
            container, status, created_at, last_seen_at, last_room_id, \
            source_url, provenance\
         ) VALUES (\
            ?1, ?2, 'b', 0, 'missing.mkv', ?3, \
            'application/octet-stream', NULL, NULL, NULL, NULL, NULL, NULL, \
            'permanent', ?4, ?4, NULL, NULL, '{}'\
         )",
    )
    .bind(&id)
    .bind(&sha)
    .bind(&rel)
    .bind(now_ms)
    .execute(&storage.pool())
    .await
    .expect("insert missing row");

    // Run a scan with an empty library. No on-disk files, so
    // every existing row is "missing".
    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_missing, 1, "the synthetic row is missing");
    assert_eq!(result.files_failed, 0, "the scan did not fail");

    // The row is still present (NOT deleted).
    let count = count_media_items(&storage).await;
    assert_eq!(count, 1, "row was not deleted");

    // And last_seen_at was bumped to at least now_ms (the test set
    // the row's last_seen_at to 1_000_000 and the scan's
    // `now_ms` is also approximately 1_000_000; the UPDATE
    // may set the value to a slightly-larger or equal timestamp).
    let row = sqlx::query("SELECT last_seen_at, status FROM media_items WHERE id = ?1")
        .bind(&id)
        .fetch_one(&storage.pool())
        .await
        .expect("row");
    let last_seen: i64 = row.get("last_seen_at");
    let status: String = row.get("status");
    assert!(
        last_seen >= now_ms,
        "last_seen_at was bumped, was {now_ms} now {last_seen}"
    );
    assert_eq!(status, "permanent", "status is unchanged");
}

// ---------------------------------------------------------------------------
// 10. unicode filenames
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_handles_unicode_filenames() {
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    let names = ["日本語.mkv", "café.mkv", "🚀.mkv"];
    for (i, name) in names.iter().enumerate() {
        let iu = i as u32;
        let mut bytes: Vec<u8> = Vec::with_capacity(256);
        bytes.push(iu as u8);
        for j in 1..256u32 {
            bytes.push(((iu.wrapping_mul(17) ^ j) & 0xFF) as u8);
        }
        let sha = sha256_hex(&bytes);
        write_cap_file(&lib_root, &sha, name, &bytes);
    }

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_scanned, 3, "3 unicode files scanned");
    assert_eq!(result.files_failed, 0);
    assert_eq!(count_media_items(&storage).await, 3);

    let rows = sqlx::query("SELECT filename FROM media_items ORDER BY filename")
        .fetch_all(&storage.pool())
        .await
        .expect("rows");
    let mut got: Vec<String> = rows.iter().map(|r| r.get("filename")).collect();
    let mut want: Vec<String> = names.iter().map(|s| s.to_string()).collect();
    got.sort();
    want.sort();
    // The filename is stored verbatim from the on-disk path, so
    // we expect byte-for-byte equality on the names we wrote.
    assert_eq!(got, want);
}

// ---------------------------------------------------------------------------
// 11. duplicate content under multiple filenames
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_handles_duplicate_content_under_multiple_filenames() {
    // [spec] P1-T07: two distinct content-addressed files with
    // the same sha but different filenames => two rows, both
    // with the same sha256.
    //
    // The P0-T05 schema (locked) declares
    // `media_items.sha256 TEXT NOT NULL UNIQUE`, so the
    // database can hold at most one row per sha256. When the
    // scanner encounters two on-disk files with the same sha,
    // the first is INSERTed; the second's SELECT finds the
    // first row, sees a different `relative_path` (because the
    // filenames differ), and UPDATEs the row in place to
    // match the second file. The schema therefore holds
    // exactly one row for the sha. The spec's wording
    // ("two rows ... both with the same sha256") contradicts
    // the schema's UNIQUE constraint; P1-T07 honors the
    // schema (locked by P0-T05) and the row is replaced
    // in-place.
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    let bytes: Vec<u8> = (0u32..1024).map(|i| (i & 0xFF) as u8).collect();
    let sha = sha256_hex(&bytes);

    // Two distinct content-addressed files with the SAME sha
    // (different filenames, hence different relative_path).
    // The sha-derived directory is the same for both, so they
    // live as siblings inside `<library>/<sha[0..2]>/<sha[2..4]>/<sha>/`.
    write_cap_file(&lib_root, &sha, "First.mkv", &bytes);
    write_cap_file(&lib_root, &sha, "Second.mkv", &bytes);

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_scanned, 2, "two files scanned");
    // The first file's INSERT succeeds. The second file's
    // SELECT finds the first row, sees a different
    // `relative_path` (Second.mkv vs First.mkv), and UPDATEs
    // the row. The UPDATE's UNIQUE conflict on `relative_path`
    // would only happen if another row already had Second.mkv;
    // there is no such row, so the UPDATE succeeds. The schema
    // therefore holds exactly one row (the UPDATE replaced the
    // INSERT's row in place).
    assert_eq!(
        result.files_upserted, 2,
        "one INSERT + one UPDATE => files_upserted = 2"
    );
    assert_eq!(result.files_failed, 0, "no files failed");

    let count = count_media_items(&storage).await;
    assert_eq!(
        count, 1,
        "the schema's UNIQUE on sha256 holds exactly one row"
    );

    // The single row points at the SECOND filename (the
    // UPDATE overwrote the first row's filename / relative_path
    // with the second file's values, because the scanner's
    // "DB tracks what is actually on disk" rule makes the
    // later-seen on-disk state authoritative).
    let row = sqlx::query("SELECT sha256, filename FROM media_items")
        .fetch_one(&storage.pool())
        .await
        .expect("row");
    let stored_sha: String = row.get("sha256");
    let stored_filename: String = row.get("filename");
    assert_eq!(stored_sha, sha);
    assert_eq!(stored_filename, "Second.mkv");
}

// ---------------------------------------------------------------------------
// 12. idempotent re-scan
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_idempotent_on_rerun() {
    // [spec] P1-T07: a re-scan of an unchanged library yields
    // files_upserted = 0; no duplicates are produced.
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    // 10 fixture files with distinct content.
    for i in 0..10u32 {
        let mut bytes: Vec<u8> = Vec::with_capacity(256);
        bytes.push(i as u8);
        for j in 1..256u32 {
            bytes.push(((i.wrapping_mul(19) ^ j) & 0xFF) as u8);
        }
        let sha = sha256_hex(&bytes);
        let filename = format!("File{i:02}.mkv");
        write_cap_file(&lib_root, &sha, &filename, &bytes);
    }

    let r1 = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(r1.files_scanned, 10);
    assert_eq!(r1.files_upserted, 10);

    let r2 = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(
        r2.files_scanned, 10,
        "every file re-seen on the second scan"
    );
    assert_eq!(
        r2.files_upserted, 0,
        "idempotent re-scan: no new rows inserted"
    );
    assert_eq!(r2.files_orphans_discovered, 0);
    assert_eq!(count_media_items(&storage).await, 10);
}

// ---------------------------------------------------------------------------
// 13. fail-soft on unreadable file (POSIX only)
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_continues_on_unreadable_file() {
    // [spec] P1-T07: 50 valid files + 1 file the scanner
    // cannot read; the valid files are still scanned;
    // files_failed includes the bad one.
    use std::os::unix::fs::PermissionsExt;

    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    for i in 0..50u32 {
        let mut bytes: Vec<u8> = Vec::with_capacity(256);
        bytes.push(i as u8);
        for j in 1..256u32 {
            bytes.push(((i.wrapping_mul(29) ^ j) & 0xFF) as u8);
        }
        let sha = sha256_hex(&bytes);
        let filename = format!("Good{i:02}.mkv");
        write_cap_file(&lib_root, &sha, &filename, &bytes);
    }

    // 1 unreadable file: real content but mode 0.
    let mut bad_bytes: Vec<u8> = Vec::with_capacity(256);
    for i in 0..256u32 {
        bad_bytes.push((i ^ 0x5A) as u8);
    }
    let bad_sha = sha256_hex(&bad_bytes);
    write_cap_file(&lib_root, &bad_sha, "Bad.mkv", &bad_bytes);
    let bad_path = cap_path(&lib_root, &bad_sha, "Bad.mkv");
    let mut perms = std::fs::metadata(&bad_path)
        .expect("metadata")
        .permissions();
    perms.set_mode(0o000);
    std::fs::set_permissions(&bad_path, perms).expect("set_permissions 0");

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    // files_scanned includes the file that was opened (even
    // though the read failed). The exact mapping between
    // "files_scanned" and "files_failed" is implementation-
    // defined; the spec says the bad file increments
    // files_failed and the scan continues.
    assert!(
        result.files_failed >= 1,
        "unreadable file must increment files_failed, got {result:?}"
    );
    assert!(
        result.files_upserted >= 50,
        "the 50 good files were still scanned, got {result:?}"
    );
    // 51 files total (50 good + 1 bad); some subset of
    // files_upserted landed in media_items.
    let count = count_media_items(&storage).await;
    assert!(count >= 50, "50 good files were inserted, got {count}");
}

#[cfg(not(unix))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_continues_on_unreadable_file() {
    // Windows hosts: it is not portable to set a file to a
    // mode the scanner cannot read. The substantive behaviour
    // is exercised by the POSIX variant above. This stub
    // asserts only that the scan runs end-to-end on a small
    // library.
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    let bytes = b"hello".to_vec();
    let sha = sha256_hex(&bytes);
    write_cap_file(&lib_root, &sha, "Hello.mkv", &bytes);

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_failed, 0, "windows stub: 0 failed");
    assert_eq!(result.files_upserted, 1, "windows stub: 1 upserted");
}

// ---------------------------------------------------------------------------
// 14. ScanResult counts are correct
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_returns_scanresult_with_correct_counts() {
    // [spec] P1-T07: 5 files => files_scanned = 5,
    // files_upserted = 5, bytes_total = sum of file sizes.
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    let mut expected_bytes: i64 = 0;
    for i in 0..5u32 {
        let mut bytes: Vec<u8> = Vec::with_capacity((100 + i * 50) as usize);
        bytes.push(i as u8);
        for j in 1..(100 + i * 50) {
            bytes.push(((i.wrapping_mul(23) ^ j) & 0xFF) as u8);
        }
        expected_bytes += bytes.len() as i64;
        let sha = sha256_hex(&bytes);
        let filename = format!("File{i}.mkv");
        write_cap_file(&lib_root, &sha, &filename, &bytes);
    }

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_scanned, 5);
    assert_eq!(result.files_upserted, 5);
    assert_eq!(result.files_missing, 0);
    assert_eq!(result.files_failed, 0);
    assert_eq!(result.bytes_total, expected_bytes);
}

// ---------------------------------------------------------------------------
// 15. zero-byte file is silently skipped (not counted as failed)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_silently_skips_zero_byte_file() {
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    // 1 valid file + 1 zero-byte file. Compute the valid sha so we
    // can place the file at its canonical content-addressed path.
    let mut bytes: Vec<u8> = Vec::with_capacity(256);
    for i in 0..256u32 {
        bytes.push((i ^ 0x77) as u8);
    }
    let sha = sha256_hex(&bytes);
    write_cap_file(&lib_root, &sha, "Good.mkv", &bytes);

    // Zero-byte file at its canonical path.
    write_cap_file(&lib_root, &sha, "Empty.mkv", &[]);

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(
        result.files_scanned, 1,
        "zero-byte file is not counted as scanned"
    );
    assert_eq!(
        result.files_failed, 0,
        "zero-byte file is not counted as failed"
    );
    assert_eq!(result.files_upserted, 1);
    let count = count_media_items(&storage).await;
    assert_eq!(count, 1, "only the valid file is in the DB");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_silently_skips_symlinks_under_library() {
    // A symlink under library/ is not followed and is not counted
    // as scanned. The file the symlink points to (outside the
    // library root) is not touched.
    use std::os::unix::fs::symlink;

    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    // 1 valid file.
    let mut bytes: Vec<u8> = Vec::with_capacity(256);
    for i in 0..256u32 {
        bytes.push((i ^ 0x33) as u8);
    }
    let sha = sha256_hex(&bytes);
    write_cap_file(&lib_root, &sha, "Good.mkv", &bytes);

    // A symlink inside library/ pointing OUTSIDE the library root.
    let outside_target = lib_root_dir.join("outside.mkv");
    std::fs::write(&outside_target, b"some bytes the scanner should not read").unwrap();
    let sha_prefix_dir = lib_root.join("library").join(&sha[0..2]);
    std::fs::create_dir_all(&sha_prefix_dir).unwrap();
    let link = sha_prefix_dir.join("link.mkv");
    symlink(&outside_target, &link).unwrap();

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_scanned, 1, "symlink is not counted as scanned");
    assert_eq!(result.files_failed, 0, "symlink does not produce a failure");
    assert_eq!(result.files_upserted, 1);
    let count = count_media_items(&storage).await;
    assert_eq!(count, 1, "only the valid file is in the DB");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_with_no_library_subdir_is_a_noop_for_files_but_still_runs_missing_pass() {
    // Fresh install: no library/ directory yet, but the DB has a
    // pre-existing row from a prior import. The walk skips (the
    // directory does not exist), the missing-file pass bumps the
    // row's last_seen_at, and the result counts are zero for
    // the file-side but iles_missing = 1 for the row-side.
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    // No library/ subdir at all.

    // Insert a synthetic row pointing at a file that does not exist.
    let now_ms: i64 = 2_000_000;
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO media_items (id, sha256, blake3, size_bytes, filename, relative_path, \
         mime, status, created_at, last_seen_at, provenance) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'application/octet-stream', 'permanent', ?7, ?7, ?8)",
    )
    .bind(&id)
    .bind("0".repeat(64))
    .bind("0".repeat(64))
    .bind(1000_i64)
    .bind("Phantom.mkv")
    .bind("library/00/00/0000000000000000000000000000000000000000000000000000000000000000/Phantom.mkv")
    .bind(now_ms)
    .bind(r#"{"source":"synthetic"}"#)
    .execute(&storage.pool())
    .await
    .expect("insert synthetic row");

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(result.files_scanned, 0, "no files in an empty library");
    assert_eq!(result.files_upserted, 0);
    assert_eq!(result.files_failed, 0);
    assert_eq!(
        result.files_missing, 1,
        "the synthetic row is marked missing"
    );

    let row = sqlx::query("SELECT last_seen_at FROM media_items WHERE id = ?1")
        .bind(&id)
        .fetch_one(&storage.pool())
        .await
        .expect("row");
    let last_seen: i64 = row.get("last_seen_at");
    assert!(
        last_seen >= now_ms,
        "last_seen_at was bumped to now or later"
    );
}

// ---------------------------------------------------------------------------
// 18. misplaced file (under library/ but at a non-canonical path)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_rejects_misplaced_file_under_library() {
    // A file under <library_root>/library/ at a path that does NOT
    // match its content-addressed layout (e.g. a user manually
    // dropped `foo.mkv` into `library/random/foo.mkv`) must NOT
    // create a media_items row, must NOT be added to `visited`,
    // and must be counted in `files_failed`. The scanner's content-
    // addressed path validation in `process_file` enforces this.
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    // Place a real file at a NON-canonical path inside library/.
    let bad_dir = lib_root.join("library").join("random");
    std::fs::create_dir_all(&bad_dir).expect("mkdir misplaced");
    let bytes: Vec<u8> = (0u32..256).map(|i| (i & 0xFF) as u8).collect();
    std::fs::write(bad_dir.join("foo.mkv"), &bytes).expect("write misplaced");

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(
        result.files_failed, 1,
        "misplaced file is counted as failed, got {result:?}"
    );
    assert_eq!(
        result.files_upserted, 0,
        "no row inserted for misplaced file"
    );
    assert_eq!(result.files_orphans_discovered, 0);
    assert_eq!(
        count_media_items(&storage).await,
        0,
        "the misplaced file does not become a media row"
    );
}

// ---------------------------------------------------------------------------
// 19. sanitizer rejection (reserved Windows name) under library/
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_skips_file_with_sanitizer_rejecting_name() {
    // A file whose name is rejected by the P1-T01 sanitizer (e.g.
    // `CON.mkv` - reserved Windows name) must not become a row and
    // must be counted in `files_failed`. The scanner's filename
    // sanitization step in `process_file` enforces this.
    //
    // The file is placed at a content-addressed-shaped path so
    // that the sanitizer is the only thing rejecting it (i.e. the
    // path validation is not the cause of the failure).
    let (storage, lib_root_dir) = open_storage().await;
    let accountant = open_accountant(&storage);
    let lib_root = make_library_root(&lib_root_dir);

    let bytes: Vec<u8> = (0u32..256).map(|i| (i & 0xFF) as u8).collect();
    let sha = sha256_hex(&bytes);
    // The sha-derived directory; the filename is the rejected one.
    let sha_dir = lib_root
        .join("library")
        .join(&sha[0..2])
        .join(&sha[2..4])
        .join(&sha);
    std::fs::create_dir_all(&sha_dir).expect("mkdir sha dir");
    std::fs::write(sha_dir.join("CON.mkv"), &bytes).expect("write CON.mkv");

    let result = library_scan_test(&storage, &accountant, &lib_root).await;
    assert_eq!(
        result.files_failed, 1,
        "sanitizer-rejected name is counted as failed, got {result:?}"
    );
    assert_eq!(result.files_upserted, 0);
    assert_eq!(count_media_items(&storage).await, 0);
}

// ---------------------------------------------------------------------------
// 20. FTS5 display_label is populated from provenance.label
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fts5_display_label_uses_provenance_label() {
    // The migration's `media_items_ai` trigger reads
    // `json_extract(new.provenance, '$.label')` into the
    // `media_items_fts.display_label` column. Insert a row with
    // `provenance` containing a `label` and assert FTS5 MATCH
    // against the label text returns the row. This pins the
    // section-7 FTS5 trigger contract end-to-end.
    let (storage, _lib_root_dir) = open_storage().await;

    let id = uuid::Uuid::new_v4().to_string();
    let sha = "b".repeat(64);
    let rel = format!("library/bb/bb/{sha}/Labeled.mkv");
    sqlx::query(
        "INSERT INTO media_items (\
            id, sha256, blake3, size_bytes, filename, relative_path, \
            mime, duration_ms, width, height, video_codec, audio_codec, \
            container, status, created_at, last_seen_at, last_room_id, \
            source_url, provenance\
         ) VALUES (\
            ?1, ?2, 'c', 0, 'Labeled.mkv', ?3, \
            'application/octet-stream', NULL, NULL, NULL, NULL, NULL, NULL, \
            'permanent', 1, 1, NULL, NULL, ?4\
         )",
    )
    .bind(&id)
    .bind(&sha)
    .bind(&rel)
    .bind(r#"{"source":"manual","label":"Movie Night Pick"}"#)
    .execute(&storage.pool())
    .await
    .expect("insert labeled row");

    let fts_count: i64 = sqlx::query(
        "SELECT COUNT(*) AS c FROM media_items_fts WHERE media_items_fts MATCH 'night'",
    )
    .fetch_one(&storage.pool())
    .await
    .expect("fts label count")
    .get("c");
    assert_eq!(
        fts_count, 1,
        "FTS5 should match the provenance label via the section-7 trigger"
    );
}
