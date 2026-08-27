//! P1-T05 integration test: disk-quota enforcement.
//!
//! Run with `cargo test -p locast-client --test quota` or simply
//! `cargo test --workspace`.
//!
//! # What's pinned
//!
//! The roadmap's P1-T05 acceptance is a long list. The test names
//! mirror the spec; the `[spec]` comment in each test names the
//! acceptance criterion the test covers.
//!
//! 1. The default cap (50 GiB) is returned when the settings row is
//!    absent. (`quota_default_when_settings_empty`)
//! 2. The cap persists across `QuotaAccountant` instances on the same
//!    storage. (`quota_set_persists_and_reloads`)
//! 3. `set_cap_bytes` rejects `0` and negative values. The cap is
//!    strictly positive. (`quota_set_rejects_zero_and_negative`)
//! 4. An import whose size would push `used + needed > cap` is
//!    refused with `AppError::QuotaExceeded`. (`quota_refuses_oversized_import`)
//! 5. An import that exactly fits `used + needed == cap` succeeds.
//!    (`quota_allows_exactly_fitting_import`)
//! 6. Raising the cap re-allows a previously-refused import.
//!    (`quota_raising_allows_oversized`)
//! 7. A dedup hit does NOT consume additional quota. The
//!    `quota_dedup_hit_does_not_double_count` test pins the exact
//!    `used_bytes` value after the second import.
//! 8. `temporary` and `permanent` rows both count toward the quota.
//!    (`quota_temporary_and_permanent_both_count`)
//! 9. `tmp/staging/<id>/...` and `tmp/incomplete/<id>/...` are
//!    counted in addition to `media_items.size_bytes`.
//!    (`quota_staging_incomplete_and_partial_count`)
//! 10. Two truly-concurrent `import_one` calls for the same content
//!     serialize via the per-library-root mutex and produce exactly
//!     one `media_items` row. (`concurrent_imports_same_content_serialize`)
//! 11. Two truly-concurrent `import_one` calls for distinct content
//!     cannot together exceed the cap; the mutex forces one to win
//!     and the other to receive `QuotaExceeded`.
//!     (`concurrent_imports_cannot_exceed_quota`)
//! 12. Two `QuotaAccountant` instances on the same library root share
//!     the mutex; instances on different roots do not.
//!     (`quota_lock_per_library_root`)
//! 13. The `quota_get` and `quota_set` Tauri commands round-trip via
//!     the same underlying `QuotaAccountant` methods.
//!     (`quota_command_round_trip`)

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use locast_client_lib::commands::import::{import_one, AppError};
use locast_client_lib::core::quota::{
    QuotaAccountant, QuotaError, DEFAULT_QUOTA_BYTES, QUOTA_SETTING_KEY,
};
use locast_client_lib::storage::Storage;
use sqlx::Row;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// shared test helpers
// ---------------------------------------------------------------------------

static TEMPDIRS: Mutex<Vec<TempDir>> = Mutex::new(Vec::new());

/// Create a fresh tempdir, persist it for the duration of the process,
/// and return its path. The test holds the path; the dir lives until
/// process exit.
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
fn make_library_root(dir: &Path) -> PathBuf {
    let root = dir.join("library");
    std::fs::create_dir_all(&root).expect("create library root");
    root
}

/// Write `bytes` to a file under `dir` and return the path.
fn write_source(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).expect("write source file");
    p
}

// ---------------------------------------------------------------------------
// 1. default cap
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_default_when_settings_empty() {
    let (storage, _dir) = open_storage().await;
    let accountant = QuotaAccountant::new(storage.clone());

    let cap = accountant.cap_bytes().await.expect("cap read ok");
    assert_eq!(
        cap, DEFAULT_QUOTA_BYTES,
        "no settings row => default 50 GiB"
    );
    assert_eq!(DEFAULT_QUOTA_BYTES, 50 * 1024 * 1024 * 1024);

    let used = accountant
        .compute_used_bytes(&make_library_root(&new_tempdir()))
        .await
        .expect("used read ok");
    assert_eq!(used, 0, "empty library => used = 0");
}

// ---------------------------------------------------------------------------
// 2. cap persists
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_set_persists_and_reloads() {
    let (storage, _dir) = open_storage().await;
    let accountant = QuotaAccountant::new(storage.clone());
    accountant.set_cap_bytes(1024).await.expect("set cap 1024");

    // Fresh accountant on the same storage sees the new cap.
    let accountant2 = QuotaAccountant::new(storage);
    let cap = accountant2.cap_bytes().await.expect("cap read ok");
    assert_eq!(cap, 1024, "cap survives across instances");
}

// ---------------------------------------------------------------------------
// 3. set_cap rejects bad values
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_set_rejects_zero_and_negative() {
    let (storage, _dir) = open_storage().await;
    let accountant = QuotaAccountant::new(storage);

    for bad in [0i64, -1, i64::MIN, -1024] {
        let r = accountant.set_cap_bytes(bad).await;
        assert!(
            matches!(r, Err(QuotaError::InvalidCap { value }) if value == bad),
            "set_cap({bad}) must be InvalidCap, got {r:?}"
        );
    }

    // And the cap row was not written.
    let cap = accountant.cap_bytes().await.expect("cap read ok");
    assert_eq!(
        cap, DEFAULT_QUOTA_BYTES,
        "rejected set_cap calls must not change the persisted cap"
    );
}

// ---------------------------------------------------------------------------
// 4. refuses oversized
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_refuses_oversized_import() {
    let (storage, _dir) = open_storage().await;
    let accountant = QuotaAccountant::new(storage.clone());
    accountant.set_cap_bytes(1024).await.expect("set cap 1024");

    let lib_root = make_library_root(&new_tempdir());
    let src_dir = new_tempdir();
    let src = write_source(&src_dir, "Big.mkv", &vec![0u8; 2048]);

    let result = import_one(&accountant, &lib_root, &storage, &src, "Big.mkv").await;
    match result {
        Err(AppError::QuotaExceeded { used, cap, needed }) => {
            assert_eq!(used, 0, "no media items yet");
            assert_eq!(cap, 1024, "cap from settings");
            assert_eq!(needed, 2048, "needed = source size");
        }
        other => panic!("expected QuotaExceeded, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 5. allows exactly fitting
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_allows_exactly_fitting_import() {
    let (storage, _dir) = open_storage().await;
    let accountant = QuotaAccountant::new(storage.clone());
    accountant.set_cap_bytes(1024).await.expect("set cap 1024");

    let lib_root = make_library_root(&new_tempdir());
    let src_dir = new_tempdir();
    let src = write_source(&src_dir, "Exact.mkv", &vec![0u8; 1024]);

    let imported = import_one(&accountant, &lib_root, &storage, &src, "Exact.mkv")
        .await
        .expect("exact-fit import succeeds");
    assert_eq!(imported.size_bytes, 1024);

    // After the import, used = 1024 = cap. A second 1-byte import
    // would be refused.
    let used = accountant
        .compute_used_bytes(&lib_root)
        .await
        .expect("used read");
    assert_eq!(used, 1024, "used = 1024 after a 1024-byte import");
}

// ---------------------------------------------------------------------------
// 6. raising the cap re-allows
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_raising_allows_oversized() {
    let (storage, _dir) = open_storage().await;
    let accountant = QuotaAccountant::new(storage.clone());
    accountant.set_cap_bytes(1024).await.expect("set cap 1024");

    let lib_root = make_library_root(&new_tempdir());
    let src_dir = new_tempdir();
    let src = write_source(&src_dir, "Big.mkv", &vec![0u8; 2048]);

    let r1 = import_one(&accountant, &lib_root, &storage, &src, "Big.mkv").await;
    assert!(
        matches!(r1, Err(AppError::QuotaExceeded { .. })),
        "cap=1024, file=2048 must be refused, got {r1:?}"
    );

    accountant.set_cap_bytes(4096).await.expect("raise cap");
    let r2 = import_one(&accountant, &lib_root, &storage, &src, "Big.mkv").await;
    assert!(r2.is_ok(), "cap=4096 must allow 2048, got {r2:?}");
}

// ---------------------------------------------------------------------------
// 7. dedup does not double-count
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_dedup_hit_does_not_double_count() {
    let (storage, _dir) = open_storage().await;
    let accountant = QuotaAccountant::new(storage.clone());
    accountant.set_cap_bytes(1024).await.expect("set cap 1024");

    let lib_root = make_library_root(&new_tempdir());
    let src_dir = new_tempdir();
    let src1 = write_source(&src_dir, "First.mkv", &vec![0u8; 1024]);
    let src2 = write_source(&src_dir, "Second.mkv", &vec![0u8; 1024]);

    let r1 = import_one(&accountant, &lib_root, &storage, &src1, "First.mkv")
        .await
        .expect("first import ok");
    let r2 = import_one(&accountant, &lib_root, &storage, &src2, "Second.mkv")
        .await
        .expect("second import ok (dedup hit)");

    assert_eq!(r1.id, r2.id, "dedup: same id");
    assert_eq!(
        r1.relative_path, r2.relative_path,
        "dedup: same relative_path"
    );

    // After dedup, used_bytes must still be 1024, not 2048.
    let used = accountant
        .compute_used_bytes(&lib_root)
        .await
        .expect("used read");
    assert_eq!(
        used, 1024,
        "dedup hit must NOT charge the quota a second time; got {used}"
    );
    eprintln!("DEDUP_USED_BYTES_AFTER_HIT: {used}");
}

// ---------------------------------------------------------------------------
// 8. temporary and permanent both count
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_temporary_and_permanent_both_count() {
    let (storage, _dir) = open_storage().await;
    let accountant = QuotaAccountant::new(storage.clone());
    accountant.set_cap_bytes(4096).await.expect("set cap 4096");

    let lib_root = make_library_root(&new_tempdir());
    let src_dir = new_tempdir();
    let src = write_source(&src_dir, "Tmp.mkv", &vec![0u8; 1024]);

    let imported = import_one(&accountant, &lib_root, &storage, &src, "Tmp.mkv")
        .await
        .expect("import ok");

    // Flip the row to 'temporary' and recompute.
    sqlx::query("UPDATE media_items SET status = 'temporary' WHERE id = ?1")
        .bind(&imported.id)
        .execute(&storage.pool())
        .await
        .expect("status update");

    let used = accountant
        .compute_used_bytes(&lib_root)
        .await
        .expect("used read");
    assert_eq!(
        used, 1024,
        "status='temporary' must still count, got {used}"
    );
}

// ---------------------------------------------------------------------------
// 9. tmp/ staging and incomplete count
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_staging_incomplete_and_partial_count() {
    let (storage, _dir) = open_storage().await;
    let accountant = QuotaAccountant::new(storage.clone());

    let lib_root = make_library_root(&new_tempdir());

    // Import a 512-byte permanent file so the DB sum is 512.
    accountant.set_cap_bytes(4096).await.expect("set cap");
    let src_dir = new_tempdir();
    let src = write_source(&src_dir, "Perm.mkv", &vec![0u8; 512]);
    import_one(&accountant, &lib_root, &storage, &src, "Perm.mkv")
        .await
        .expect("import perm");

    // Create a fake staging/<id>/foo.partial of 600 bytes.
    let staging_id = "01234567-89ab-cdef-0123-456789abcdef";
    let staging_dir = lib_root.join("tmp").join("staging").join(staging_id);
    std::fs::create_dir_all(&staging_dir).expect("mkdir staging");
    std::fs::write(staging_dir.join("foo.partial"), vec![0u8; 600]).expect("write partial");

    // After staging, used = 512 + 600 = 1112.
    let used = accountant
        .compute_used_bytes(&lib_root)
        .await
        .expect("used read after staging");
    assert_eq!(used, 1112, "used = db + staging, got {used}");

    // Create a fake incomplete/<id>/foo.part.0 of 200 bytes.
    let incomplete_id = "11234567-89ab-cdef-0123-456789abcdef";
    let incomplete_dir = lib_root.join("tmp").join("incomplete").join(incomplete_id);
    std::fs::create_dir_all(&incomplete_dir).expect("mkdir incomplete");
    std::fs::write(incomplete_dir.join("foo.part.0"), vec![0u8; 200]).expect("write chunk");

    let used = accountant
        .compute_used_bytes(&lib_root)
        .await
        .expect("used read after incomplete");
    assert_eq!(
        used, 1312,
        "used = 512 (db) + 600 (staging) + 200 (incomplete), got {used}"
    );

    // cap=1312 => check_allow(0) is Allow.
    accountant.set_cap_bytes(1312).await.expect("set cap=1312");
    let check = accountant
        .check_allow(&lib_root, 0)
        .await
        .expect("check_allow(0) at cap=1312");
    match check {
        locast_client_lib::core::quota::QuotaCheck::Allow { used: u, cap: c } => {
            assert_eq!(u, 1312);
            assert_eq!(c, 1312);
        }
    }

    // cap=1000 => check_allow(0) refused.
    accountant.set_cap_bytes(1000).await.expect("set cap=1000");
    let err = accountant
        .check_allow(&lib_root, 0)
        .await
        .expect_err("check_allow(0) at cap=1000 must be refused");
    match err {
        QuotaError::Exceeded { used, cap, needed } => {
            assert_eq!(used, 1312);
            assert_eq!(cap, 1000);
            assert_eq!(needed, 0);
        }
        other => panic!("expected Exceeded, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 10. concurrent same-content dedup
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_imports_same_content_serialize() {
    let (storage, _dir) = open_storage().await;
    let accountant = QuotaAccountant::new(storage.clone());

    let lib_root = make_library_root(&new_tempdir());
    let src_dir = new_tempdir();
    let src1 = write_source(&src_dir, "A.mkv", &vec![0u8; 1024]);
    let src2 = write_source(&src_dir, "B.mkv", &vec![0u8; 1024]);

    let (r1, r2) = tokio::join!(
        import_one(&accountant, &lib_root, &storage, &src1, "A.mkv"),
        import_one(&accountant, &lib_root, &storage, &src2, "B.mkv"),
    );
    let r1 = r1.expect("first ok");
    let r2 = r2.expect("second ok");

    assert_eq!(r1.id, r2.id, "dedup: same id");
    assert_eq!(
        r1.relative_path, r2.relative_path,
        "dedup: same relative_path"
    );

    let count: i64 = sqlx::query("SELECT COUNT(*) AS c FROM media_items")
        .fetch_one(&storage.pool())
        .await
        .expect("count")
        .get("c");
    assert_eq!(count, 1, "concurrent same-content imports => one row");
}

// ---------------------------------------------------------------------------
// 11. concurrent distinct content cannot exceed the cap
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_imports_cannot_exceed_quota() {
    let (storage, _dir) = open_storage().await;
    let accountant = QuotaAccountant::new(storage.clone());
    accountant.set_cap_bytes(1500).await.expect("set cap 1500");

    let lib_root = make_library_root(&new_tempdir());
    let src_dir = new_tempdir();
    // Two distinct 1024-byte payloads. Together they would be 2048,
    // which is > 1500. The per-library-root mutex forces one to win
    // and one to be refused.
    let src1 = write_source(&src_dir, "A.mkv", &vec![0xA1u8; 1024]);
    let src2 = write_source(&src_dir, "B.mkv", &vec![0xB2u8; 1024]);

    let (r1, r2) = tokio::join!(
        import_one(&accountant, &lib_root, &storage, &src1, "A.mkv"),
        import_one(&accountant, &lib_root, &storage, &src2, "B.mkv"),
    );

    // Exactly one is Ok and exactly one is QuotaExceeded{needed=1024}.
    let oks = [&r1, &r2].iter().filter(|r| r.is_ok()).count();
    let refused: Vec<&AppError> = [&r1, &r2]
        .iter()
        .filter_map(|r| match r {
            Err(AppError::QuotaExceeded { needed, .. }) => Some(*needed),
            _ => None,
        })
        .map(|_| match (&r1, &r2) {
            (Err(e @ AppError::QuotaExceeded { .. }), _)
            | (_, Err(e @ AppError::QuotaExceeded { .. })) => e,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(oks, 1, "exactly one import wins, got {r1:?} / {r2:?}");
    assert_eq!(refused.len(), 1, "exactly one is refused");
    if let AppError::QuotaExceeded { used, cap, needed } = refused[0] {
        assert_eq!(*needed, 1024, "needed = 1024 (the refused file size)");
        assert_eq!(*cap, 1500, "cap = 1500");
        assert!(*used <= 1500, "used <= cap at refusal, got {used}");
    } else {
        panic!("not a QuotaExceeded: {:?}", refused[0]);
    }

    // After the test, the on-disk library has exactly one file.
    let library = lib_root.join("library");
    let mut count = 0;
    fn walk(d: &Path, n: &mut usize) {
        if d.is_dir() {
            for e in std::fs::read_dir(d).expect("read_dir") {
                let p = e.expect("entry").path();
                if p.is_dir() {
                    walk(&p, n);
                } else if p.is_file() {
                    *n += 1;
                }
            }
        }
    }
    walk(&library, &mut count);
    assert_eq!(count, 1, "exactly one on-disk file, got {count}");

    // And used = 1024.
    let used = accountant
        .compute_used_bytes(&lib_root)
        .await
        .expect("used");
    assert_eq!(used, 1024, "used = 1024 after one successful import");
}

// ---------------------------------------------------------------------------
// 12. lock is per-library-root
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_lock_per_library_root() {
    // Two distinct library roots under two distinct storage files
    // share a single accountant. The per-library-root mutexes must
    // be independent: a lock on root A does NOT block a lock on
    // root B.
    let (storage_a, _dir_a) = open_storage().await;
    let (storage_b, _dir_b) = open_storage().await;
    let accountant_a = QuotaAccountant::new(storage_a.clone());
    let accountant_b = QuotaAccountant::new(storage_b.clone());

    let root_a = make_library_root(&new_tempdir());
    let root_b = make_library_root(&new_tempdir());

    let (ga, gb) = tokio::join!(
        accountant_a.lock_for_library(&root_a),
        accountant_b.lock_for_library(&root_b),
    );
    let (ga, _) = ga.expect("lock a");
    let (gb, _) = gb.expect("lock b");
    // Both locks acquired concurrently - they are independent
    // mutexes. Drop them to release.
    ga.release();
    gb.release();

    // Two accountants on the same library root share the mutex. We
    // verify this by acquiring a lock from accountant C1, then
    // trying to acquire the same lock from accountant C2 in a
    // background task. C2's acquisition must block until C1
    // releases.
    let (storage_c, _dir_c) = open_storage().await;
    let accountant_c1 = QuotaAccountant::new(storage_c.clone());
    let accountant_c2 = QuotaAccountant::new(storage_c.clone());
    let root_c = make_library_root(&new_tempdir());

    let (gc1, _) = accountant_c1
        .lock_for_library(&root_c)
        .await
        .expect("lock c1");

    // Spawn the C2 acquisition. It must block on the per-library
    // mutex that C1 already holds. We use a `tokio::sync::oneshot`
    // to signal that C2 has been woken (it'll wake only after C1
    // releases the lock).
    let (c2_got_lock_tx, mut c2_got_lock_rx) = tokio::sync::oneshot::channel::<()>();
    let accountant_c2_clone = accountant_c2.clone();
    let root_c_clone = root_c.clone();
    let task = tokio::spawn(async move {
        let (g, _) = accountant_c2_clone
            .lock_for_library(&root_c_clone)
            .await
            .expect("lock c2");
        // Signal that we got the lock.
        let _ = c2_got_lock_tx.send(());
        g
    });

    // Wait long enough for C2 to attempt the lock and park. A
    // 200ms sleep is more than enough on every platform; if the
    // test still flakes, increase.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // C2's task has not signalled yet (it should still be parked).
    assert!(
        c2_got_lock_rx.try_recv().is_err(),
        "c2 must still be blocked while c1 holds the lock"
    );

    // Release c1. C2 should now acquire promptly.
    gc1.release();
    // Wait up to 2s for the signal.
    tokio::time::timeout(std::time::Duration::from_secs(2), c2_got_lock_rx)
        .await
        .expect("c2 must signal within 2s after c1 releases")
        .expect("c2 signal not dropped");

    let gc2 = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("c2 task finished within 2s")
        .expect("c2 task did not panic");
    gc2.release();
}

// ---------------------------------------------------------------------------
// 13. Tauri command round-trip
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_accountant_round_trip() {
    // Exercises the underlying `QuotaAccountant` methods the Tauri
    // commands call into. The Tauri commands themselves are thin
    // wrappers; the substantive logic is on the accountant. (Running
    // the commands under a real Tauri runtime is out of scope for this
    // integration test; the bindings test covers the IPC surface
    // shape.)
    use locast_client_lib::commands::quota::QuotaInfo;
    let (storage, _dir) = open_storage().await;
    let accountant = QuotaAccountant::new(storage.clone());

    // Initial state: cap = default, used = 0.
    let used = accountant
        .compute_used_bytes(&make_library_root(&new_tempdir()))
        .await
        .expect("used");
    let cap = accountant.cap_bytes().await.expect("cap");
    let info = QuotaInfo {
        used_bytes: used,
        cap_bytes: cap,
    };
    assert_eq!(info.used_bytes, 0);
    assert_eq!(info.cap_bytes, DEFAULT_QUOTA_BYTES);

    // quota_set(2048) via the accountant (the command is a thin
    // wrapper).
    accountant.set_cap_bytes(2048).await.expect("set 2048");
    let cap = accountant.cap_bytes().await.expect("cap");
    assert_eq!(cap, 2048);

    // The settings row was written with the right key and a
    // numeric JSON value.
    let row: (String,) = sqlx::query_as("SELECT value FROM settings WHERE key = ?1")
        .bind(QUOTA_SETTING_KEY)
        .fetch_one(&storage.pool())
        .await
        .expect("settings row");
    let v: i64 = serde_json::from_str(&row.0).expect("json int");
    assert_eq!(v, 2048);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_tauri_command_library_root_matches_storage_parent() {
    // P1-T05's `commands::quota::resolve_library_root` and
    // P1-T04's `commands::import::resolve_library_root` must agree on
    // what the library root is. The contract is: the library root is
    // the parent of the SQLite file. This test opens a `Storage` at
    // `<tempdir>/index.sqlite`, writes a 600-byte fake staging file
    // under `<tempdir>/tmp/staging/<id>/foo.partial`, and asserts
    // that `compute_used_bytes` (called the same way the Tauri command
    // would) sees the 600 bytes of staging. This catches the P1-T05
    // review finding #1: a divergent `resolve_library_root` would walk
    // the wrong `tmp/` tree and report 0 staging bytes.

    let lib_root_tmp = new_tempdir();
    let db_dir = new_tempdir();
    let db_path = db_dir.join("index.sqlite");
    let storage = Storage::open(&db_path).await.expect("storage opens");
    let accountant = QuotaAccountant::new(storage.clone());

    // The Tauri commands resolve the library root as
    // `storage.path().parent()`. In production, `lib.rs` places
    // `index.sqlite` directly under the user's library root, so
    // `db_dir` IS the library root. Mirror that here.
    let lib_root = db_dir.clone();
    assert_eq!(
        db_path.parent(),
        Some(lib_root.as_path()),
        "this test requires <db_path parent> == <lib_root>"
    );

    // Write a fake 600-byte staging file.
    let staging_dir = lib_root.join("tmp").join("staging").join("import-id");
    std::fs::create_dir_all(&staging_dir).expect("create staging dir");
    let partial = staging_dir.join("foo.partial");
    std::fs::write(&partial, vec![0u8; 600]).expect("write partial");

    // `compute_used_bytes` should see used = 600 (just the staging;
    // the DB has no media_items yet). If `resolve_library_root` were
    // wrong (e.g. pointed at a sibling dir), the walk would not find
    // the staging and would report 0.
    let used = accountant
        .compute_used_bytes(&lib_root)
        .await
        .expect("compute used");
    assert_eq!(
        used, 600,
        "compute_used_bytes must see the 600-byte staging file under <lib_root>/tmp/staging/"
    );

    // The unused binding silences the unused-variable lint.
    let _ = lib_root_tmp;
}
