//! P1-T04 integration test: `media_import`.
//!
//! Run with `cargo test -p locast-client --test media_import` or
//! simply `cargo test --workspace`.
//!
//! The roadmap's P1-T04 acceptance is:
//!
//! > an integration test (or a manual Tauri dev session) imports two
//! > files with identical bytes; the second one dedupes via
//! > hardlink/copy and the `media_items` table contains two rows
//! > pointing to the same on-disk file; the TS binding types match.
//!
//! The P0-T05 schema's UNIQUE INDEX on `media_items.relative_path`
//! makes "two rows pointing at the same on-disk file" literally
//! impossible. The P1-T04 contract resolves this by running the
//! dedup check BEFORE the copy, so the second import returns the
//! first row's data without inserting a second row. The tests below
//! therefore assert the actual contract: two calls with the same
//! bytes return `ImportedMedia`s that share `id` and `relative_path`,
//! the database has exactly one row, and the on-disk library has
//! exactly one file.

use std::path::Path;
use std::sync::Mutex;

use locast_client_lib::commands::import::{import_one, AppError, ImportedMedia};
use locast_client_lib::core::quota::QuotaAccountant;
use locast_client_lib::storage::Storage;
use sqlx::Row;
use tempfile::TempDir;

/// Build a `Storage` against a per-test SQLite file.
async fn open_storage(dir: &TempDir) -> Storage {
    let db = dir.path().join("index.sqlite");
    Storage::open(&db).await.expect("storage opens")
}

/// Build a `QuotaAccountant` against the same storage. P1-T05 threads
/// the accountant through `import_one`; the existing P1-T04 tests
/// instantiate one with the default 50 GiB cap and pass it through.
fn open_accountant(storage: &Storage) -> QuotaAccountant {
    QuotaAccountant::new(storage.clone())
}

/// Write `bytes` to a file under `dir` and return the path.
fn write_source(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).expect("write source file");
    p
}

/// Count regular files anywhere under `dir`, recursively.
fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let p = entry.path();
            if p.is_dir() {
                n += count_files(&p);
            } else if p.is_file() {
                n += 1;
            }
        }
    }
    n
}

/// Walk the on-disk library and return every regular file's path
/// (relative to the library root, using forward slashes).
fn library_files(library_root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk(library_root, library_root, &mut out);
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        if dir.is_dir() {
            for entry in std::fs::read_dir(dir).expect("read_dir") {
                let entry = entry.expect("dir entry");
                let p = entry.path();
                if p.is_dir() {
                    walk(root, &p, out);
                } else if p.is_file() {
                    let rel = p
                        .strip_prefix(root)
                        .expect("file under root")
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push(rel);
                }
            }
        }
    }
    out.sort();
    out
}

fn make_library_root() -> std::path::PathBuf {
    // The library root is a sibling of the SQLite file so the
    // `import_one` can copy the source into `<root>/tmp/staging/...`
    // and complete into `<root>/library/...` without crossing the
    // storage file.
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path().join("library");
    std::fs::create_dir_all(&root).expect("create library root");
    // Stash the TempDir in a side-table so it stays alive; the test
    // owns the path through this Arc.
    ROOT_HOLDERS.lock().expect("root holders").push(dir);
    root
}

static ROOT_HOLDERS: Mutex<Vec<TempDir>> = Mutex::new(Vec::new());

// ===========================================================================
// acceptance: two identical files dedup to one on-disk file and one row
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_import_dedup_two_identical_files() {
    let lib_root = make_library_root();
    let storage_holder = TempDir::new().expect("storage tempdir");
    let storage = open_storage(&storage_holder).await;
    let accountant = open_accountant(&storage);

    let src_dir = TempDir::new().expect("src tempdir");
    let payload: Vec<u8> = (0u32..4096).map(|i| (i & 0xFF) as u8).collect();
    let src1 = write_source(src_dir.path(), "MovieA.mkv", &payload);
    let src2 = write_source(src_dir.path(), "MovieB.mkv", &payload);

    let imported1 = import_one(&accountant, &lib_root, &storage, &src1, "MovieA.mkv")
        .await
        .expect("first import succeeds");
    let imported2 = import_one(&accountant, &lib_root, &storage, &src2, "MovieB.mkv")
        .await
        .expect("second import succeeds");

    // Both `ImportedMedia` returns share the same id and relative_path.
    assert_eq!(imported1.id, imported2.id, "dedup: same id");
    assert_eq!(
        imported1.relative_path, imported2.relative_path,
        "dedup: same relative_path"
    );
    assert_eq!(imported1.sha256, imported2.sha256);
    assert_eq!(imported1.size_bytes, payload.len() as i64);

    // The on-disk library has exactly one file under
    // <root>/library/<sha[0..2]>/<sha[2..4]>/<sha>/<sanitized>.
    let content_files: Vec<_> = library_files(&lib_root)
        .into_iter()
        .filter(|p| p.starts_with("library/"))
        .collect();
    assert_eq!(
        content_files.len(),
        1,
        "dedup: one on-disk file under library/, got {content_files:?}"
    );
    eprintln!("DEDUP_ON_DISK_FILE_COUNT: {}", content_files.len());
    eprintln!("DEDUP_ON_DISK_FILES: {content_files:?}");

    // And exactly one row in media_items.
    let count: i64 = sqlx::query("SELECT COUNT(*) AS c FROM media_items")
        .fetch_one(&storage.pool())
        .await
        .expect("count media_items")
        .get("c");
    assert_eq!(count, 1, "dedup: one media_items row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_import_two_distinct_files() {
    let lib_root = make_library_root();
    let storage_holder = TempDir::new().expect("storage tempdir");
    let storage = open_storage(&storage_holder).await;
    let accountant = open_accountant(&storage);

    let src_dir = TempDir::new().expect("src tempdir");
    let bytes1: Vec<u8> = (0u32..1024).map(|i| (i & 0xFF) as u8).collect();
    let bytes2: Vec<u8> = (0u32..2048).map(|i| ((i * 7) & 0xFF) as u8).collect();
    assert_ne!(bytes1, bytes2);

    let src1 = write_source(src_dir.path(), "First.mkv", &bytes1);
    let src2 = write_source(src_dir.path(), "Second.mkv", &bytes2);

    let imported1 = import_one(&accountant, &lib_root, &storage, &src1, "First.mkv")
        .await
        .expect("first distinct import");
    let imported2 = import_one(&accountant, &lib_root, &storage, &src2, "Second.mkv")
        .await
        .expect("second distinct import");

    assert_ne!(imported1.id, imported2.id);
    assert_ne!(imported1.relative_path, imported2.relative_path);
    assert_ne!(imported1.sha256, imported2.sha256);

    let content_files: Vec<_> = library_files(&lib_root)
        .into_iter()
        .filter(|p| p.starts_with("library/"))
        .collect();
    assert_eq!(
        content_files.len(),
        2,
        "two distinct files => two on-disk files, got {content_files:?}"
    );

    let count: i64 = sqlx::query("SELECT COUNT(*) AS c FROM media_items")
        .fetch_one(&storage.pool())
        .await
        .expect("count media_items")
        .get("c");
    assert_eq!(count, 2, "two distinct files => two rows");
}

// ===========================================================================
// error paths
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_import_missing_source() {
    let lib_root = make_library_root();
    let storage_holder = TempDir::new().expect("storage tempdir");
    let storage = open_storage(&storage_holder).await;
    let accountant = open_accountant(&storage);

    let ghost = storage_holder.path().join("does-not-exist.mkv");
    // The file does not exist.

    let result = import_one(
        &accountant,
        &lib_root,
        &storage,
        &ghost,
        "does-not-exist.mkv",
    )
    .await;
    assert!(
        matches!(result, Err(AppError::SourceMissing { .. })),
        "missing source must be SourceMissing, got {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_import_source_is_a_directory() {
    let lib_root = make_library_root();
    let storage_holder = TempDir::new().expect("storage tempdir");
    let storage = open_storage(&storage_holder).await;
    let accountant = open_accountant(&storage);

    let dir = storage_holder.path().join("is-a-dir");
    std::fs::create_dir_all(&dir).expect("mkdir");

    let result = import_one(&accountant, &lib_root, &storage, &dir, "is-a-dir").await;
    assert!(
        matches!(result, Err(AppError::SourceMissing { .. })),
        "directory source must be SourceMissing, got {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_import_invalid_filename() {
    let lib_root = make_library_root();
    let storage_holder = TempDir::new().expect("storage tempdir");
    let storage = open_storage(&storage_holder).await;
    let accountant = open_accountant(&storage);

    let src_dir = TempDir::new().expect("src tempdir");
    let bytes = b"some bytes for invalid filename".to_vec();
    // The source file gets a real on-disk name (Windows would refuse
    // a literal `....`); the sanitizer-rejecting input is the
    // `display_filename` parameter, which is what `media_import` will
    // see after the user types a bad name into the dialog.
    let src = write_source(src_dir.path(), "real_name.mkv", &bytes);

    let result = import_one(&accountant, &lib_root, &storage, &src, "....").await;
    assert!(
        matches!(result, Err(AppError::InvalidFilename)),
        "sanitizer-rejecting display filename must be InvalidFilename, got {result:?}"
    );
}

// ===========================================================================
// failure: no orphan staging on failure
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_import_no_partial_file_after_failure() {
    // After a failed import, no orphan tmp/staging/<id>/ directory
    // should remain. For an `InvalidFilename` failure (the test we
    // use here), the failure happens BEFORE the staging copy, so the
    // assertion is trivially true. The test still pins the invariant.
    let lib_root = make_library_root();
    let storage_holder = TempDir::new().expect("storage tempdir");
    let storage = open_storage(&storage_holder).await;
    let accountant = open_accountant(&storage);

    let src_dir = TempDir::new().expect("src tempdir");
    let bytes = b"orphan-staging test".to_vec();
    let src = write_source(src_dir.path(), "real_name.mkv", &bytes);

    let _ = import_one(&accountant, &lib_root, &storage, &src, "....").await;

    let staging = lib_root.join("tmp").join("staging");
    let count = if staging.exists() {
        count_files(&staging)
    } else {
        0
    };
    assert_eq!(
        count, 0,
        "no orphan staging files after a failed import, got {count} files under {staging:?}"
    );
}

// ===========================================================================
// database row shape
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_import_database_row_present() {
    let lib_root = make_library_root();
    let storage_holder = TempDir::new().expect("storage tempdir");
    let storage = open_storage(&storage_holder).await;
    let accountant = open_accountant(&storage);

    let src_dir = TempDir::new().expect("src tempdir");
    let bytes: Vec<u8> = (0u32..512).map(|i| (i & 0xFF) as u8).collect();
    let src = write_source(src_dir.path(), "DatabaseRow.mkv", &bytes);

    let imported = import_one(&accountant, &lib_root, &storage, &src, "DatabaseRow.mkv")
        .await
        .expect("import succeeds");

    // size_bytes must equal the source's on-disk size (the implementation
    // accumulates the chunked-read total rather than trusting
    // metadata().len(); the test pins that the two agree).
    let on_disk_size = std::fs::metadata(&src).expect("source metadata").len() as i64;
    assert_eq!(
        imported.size_bytes, on_disk_size,
        "size_bytes must equal the on-disk source size"
    );

    let row = sqlx::query(
        "SELECT sha256, blake3, size_bytes, filename, relative_path, status, \
         provenance, mime \
         FROM media_items WHERE id = ?1",
    )
    .bind(&imported.id)
    .fetch_one(&storage.pool())
    .await
    .expect("row present");

    let sha256: String = row.get("sha256");
    let blake3: String = row.get("blake3");
    let size_bytes: i64 = row.get("size_bytes");
    let filename: String = row.get("filename");
    let relative_path: String = row.get("relative_path");
    let status: String = row.get("status");
    let provenance: String = row.get("provenance");
    let mime: String = row.get("mime");

    assert_eq!(sha256, imported.sha256);
    assert_eq!(blake3, imported.blake3);
    assert_eq!(size_bytes, imported.size_bytes);
    assert_eq!(filename, imported.filename);
    assert_eq!(relative_path, imported.relative_path);
    assert_eq!(status, "permanent");
    assert_eq!(mime, "application/octet-stream");
    assert!(provenance.contains("user-import"));

    // The relative_path must have the architecture's content-addressed
    // shape: library/<sha[0..2]>/<sha[2..4]>/<sha>/<sanitized>.
    assert!(
        relative_path.starts_with("library/"),
        "relative_path must be under library/, got {relative_path:?}"
    );
    let comps: Vec<&str> = relative_path.split('/').collect();
    assert_eq!(
        comps.len(),
        5,
        "expected 5 components (library / aabb / ccdd / <sha> / <file>), got {comps:?}"
    );
    assert_eq!(comps[1], &sha256[0..2]);
    assert_eq!(comps[2], &sha256[2..4]);
    assert_eq!(comps[3], sha256);
    assert_eq!(comps[4], filename);
}

// ===========================================================================
// extra: ImportedMedia Clone/PartialEq (used by TS for re-render checks)
// ===========================================================================

#[test]
fn imported_media_clone_eq_round_trip() {
    let m = ImportedMedia {
        id: "id".to_string(),
        sha256: "a".repeat(64),
        blake3: "b".repeat(64),
        size_bytes: 123,
        filename: "Movie.mkv".to_string(),
        relative_path: "library/aa/bb/aa..bb/Movie.mkv".to_string(),
    };
    let n = m.clone();
    assert_eq!(m, n);
}

// ===========================================================================
// extra: ensure that cloning the Storage handle and calling import_one
// sequentially still dedupes. Truly-concurrent dedup races are P1-T05's
// concern (the per-library-root mutex); P1-T04 only commits to the
// sequential case.
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn import_one_with_cloned_storage() {
    let lib_root = make_library_root();
    let storage_holder = TempDir::new().expect("storage tempdir");
    let storage = open_storage(&storage_holder).await;
    let accountant = open_accountant(&storage);
    let storage2 = storage.clone();

    let src_dir = TempDir::new().expect("src tempdir");
    let bytes = b"cloned storage".to_vec();
    let src = write_source(src_dir.path(), "Cloned.mkv", &bytes);
    let src2 = write_source(src_dir.path(), "ClonedAgain.mkv", &bytes);

    let r1 = import_one(&accountant, &lib_root, &storage, &src, "Cloned.mkv")
        .await
        .expect("first import ok");
    let r2 = import_one(&accountant, &lib_root, &storage2, &src2, "ClonedAgain.mkv")
        .await
        .expect("second import ok");
    assert_eq!(r1.id, r2.id, "cloned storage: dedup still applies");
}
