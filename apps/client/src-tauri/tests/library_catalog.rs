//! P1-T09 integration tests: the library catalog backend
//! (`commands::library`). They run the real import path
//! (`commands::import::import_one`) into a real `Storage` and library
//! root, then exercise list, search, make-permanent and delete.
//!
//! Run with `cargo test -p locast-client --test library_catalog -j 1`.

use std::path::{Path, PathBuf};

use locast_client_lib::commands::import::{import_one, AppError, ImportedMedia};
use locast_client_lib::commands::library::{delete_item, list_items, make_permanent};
use locast_client_lib::core::quota::QuotaAccountant;
use locast_client_lib::storage::Storage;
use sqlx::Row;
use tempfile::TempDir;

/// A fresh storage plus the library root it implies (the parent of the
/// SQLite file, the convention every command uses).
struct Fixture {
    storage: Storage,
    accountant: QuotaAccountant,
    root: PathBuf,
    _dir: TempDir,
    src: TempDir,
}

async fn fixture() -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let storage = Storage::open(dir.path().join("index.sqlite"))
        .await
        .expect("storage opens");
    let accountant = QuotaAccountant::new(storage.clone());
    Fixture {
        storage,
        accountant,
        root: dir.path().to_path_buf(),
        _dir: dir,
        src: TempDir::new().expect("source tempdir"),
    }
}

impl Fixture {
    /// Import a distinct file named `name` whose bytes depend on `seed`.
    async fn import(&self, name: &str, seed: u8) -> ImportedMedia {
        let bytes: Vec<u8> = (0..2048u32)
            .map(|i| (i as u8).wrapping_mul(seed).wrapping_add(seed))
            .collect();
        let path = self.src.path().join(name);
        std::fs::write(&path, &bytes).expect("write source");
        import_one(&self.accountant, &self.root, &self.storage, &path, name)
            .await
            .expect("import succeeds")
    }

    async fn set_status(&self, id: &str, status: &str) {
        sqlx::query("UPDATE media_items SET status = ?1 WHERE id = ?2")
            .bind(status)
            .bind(id)
            .execute(&self.storage.pool())
            .await
            .expect("update status");
    }

    async fn row_count(&self) -> i64 {
        sqlx::query("SELECT COUNT(*) AS c FROM media_items")
            .fetch_one(&self.storage.pool())
            .await
            .expect("count")
            .get("c")
    }
}

fn names(items: &[locast_client_lib::commands::library::LibraryItem]) -> Vec<&str> {
    items.iter().map(|i| i.filename.as_str()).collect()
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files_under(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imported_files_are_listed_with_their_metadata() {
    let f = fixture().await;
    let a = f.import("MovieNight.mkv", 3).await;
    let b = f.import("Holiday.mkv", 5).await;

    let items = list_items(&f.storage, None, None, None)
        .await
        .expect("list");
    assert_eq!(items.len(), 2);
    let item_a = items.iter().find(|i| i.id == a.id).expect("a listed");
    assert_eq!(item_a.filename, "MovieNight.mkv");
    assert_eq!(item_a.sha256, a.sha256);
    assert_eq!(item_a.size_bytes, 2048);
    assert_eq!(item_a.status, "permanent", "imports are permanent");
    assert!(item_a.created_at > 0);
    assert!(items.iter().any(|i| i.id == b.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_library_lists_nothing() {
    let f = fixture().await;
    assert!(list_items(&f.storage, None, None, None)
        .await
        .expect("list")
        .is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_matches_filename_prefixes_case_insensitively() {
    let f = fixture().await;
    f.import("MovieNight.mkv", 3).await;
    f.import("Holiday.mkv", 5).await;
    f.import("MovieTrailer.mkv", 7).await;

    let movies = list_items(&f.storage, Some("movie"), None, None)
        .await
        .expect("search");
    let mut found = names(&movies);
    found.sort_unstable();
    assert_eq!(found, vec!["MovieNight.mkv", "MovieTrailer.mkv"]);

    let holiday = list_items(&f.storage, Some("  HOL "), None, None)
        .await
        .expect("search");
    assert_eq!(names(&holiday), vec!["Holiday.mkv"]);

    let none = list_items(&f.storage, Some("zzz"), None, None)
        .await
        .expect("search");
    assert!(none.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blank_query_lists_everything_and_syntax_only_query_matches_nothing() {
    let f = fixture().await;
    f.import("MovieNight.mkv", 3).await;
    f.import("Holiday.mkv", 5).await;

    let all = list_items(&f.storage, Some("   "), None, None)
        .await
        .expect("blank");
    assert_eq!(all.len(), 2);

    // Characters that are FTS5 syntax must never be interpreted as such.
    for q in [
        "\"",
        "-",
        "*",
        "a OR",
        "movie AND (",
        "NEAR(",
        "col:x",
        "\"unterminated",
    ] {
        list_items(&f.storage, Some(q), None, None)
            .await
            .unwrap_or_else(|e| panic!("query {q:?} must not error: {e:?}"));
    }
    let punctuation = list_items(&f.storage, Some("-- \"\" *"), None, None)
        .await
        .expect("punctuation");
    assert!(punctuation.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limit_and_offset_page_through_the_library() {
    let f = fixture().await;
    for (i, name) in ["A1.mkv", "A2.mkv", "A3.mkv"].iter().enumerate() {
        f.import(name, 3 + i as u8 * 2).await;
    }
    let first = list_items(&f.storage, None, Some(2), Some(0))
        .await
        .expect("page 1");
    let second = list_items(&f.storage, None, Some(2), Some(2))
        .await
        .expect("page 2");
    assert_eq!(first.len(), 2);
    assert_eq!(second.len(), 1);
    assert!(first.iter().all(|a| second.iter().all(|b| a.id != b.id)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn make_permanent_flips_the_row_and_is_idempotent() {
    let f = fixture().await;
    let a = f.import("Clip.mkv", 3).await;
    f.set_status(&a.id, "temporary").await;
    let before = list_items(&f.storage, None, None, None)
        .await
        .expect("list");
    assert_eq!(before[0].status, "temporary");

    make_permanent(&f.storage, &a.id).await.expect("promote");
    let after = list_items(&f.storage, None, None, None)
        .await
        .expect("list");
    assert_eq!(after[0].status, "permanent");

    make_permanent(&f.storage, &a.id)
        .await
        .expect("promoting twice is fine");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn make_permanent_on_an_unknown_id_is_not_found() {
    let f = fixture().await;
    let err = make_permanent(&f.storage, "no-such-id")
        .await
        .expect_err("unknown id");
    assert!(matches!(err, AppError::NotFound { .. }), "{err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_moves_the_file_to_trash_and_removes_the_row_and_search_entry() {
    let f = fixture().await;
    let a = f.import("MovieNight.mkv", 3).await;
    f.import("Holiday.mkv", 5).await;
    let on_disk = f.root.join(&a.relative_path);
    assert!(on_disk.is_file(), "imported file exists");

    delete_item(&f.storage, &f.root, &a.id)
        .await
        .expect("delete");

    assert!(!on_disk.exists(), "file left the library tree");
    let trashed = files_under(&f.root.join("trash"));
    assert_eq!(trashed.len(), 1, "exactly one file in trash: {trashed:?}");
    assert_eq!(trashed[0].file_name().unwrap(), "MovieNight.mkv");
    assert!(trashed[0].to_string_lossy().contains(&a.sha256));

    assert_eq!(f.row_count().await, 1);
    let listed = list_items(&f.storage, None, None, None)
        .await
        .expect("list");
    assert_eq!(names(&listed), vec!["Holiday.mkv"]);
    let searched = list_items(&f.storage, Some("movie"), None, None)
        .await
        .expect("search");
    assert!(searched.is_empty(), "FTS entry was removed with the row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deleted_media_can_be_imported_again() {
    let f = fixture().await;
    let a = f.import("Clip.mkv", 3).await;
    delete_item(&f.storage, &f.root, &a.id)
        .await
        .expect("delete");
    let again = f.import("Clip.mkv", 3).await;
    assert_eq!(again.sha256, a.sha256);
    assert_eq!(f.row_count().await, 1);
    assert!(f.root.join(&again.relative_path).is_file());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_tolerates_a_file_that_is_already_gone() {
    let f = fixture().await;
    let a = f.import("Clip.mkv", 3).await;
    std::fs::remove_file(f.root.join(&a.relative_path)).expect("remove file");

    delete_item(&f.storage, &f.root, &a.id)
        .await
        .expect("stale row is removed");
    assert_eq!(f.row_count().await, 0);
    assert!(files_under(&f.root.join("trash")).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_on_an_unknown_id_is_not_found() {
    let f = fixture().await;
    let err = delete_item(&f.storage, &f.root, "no-such-id")
        .await
        .expect_err("unknown id");
    assert!(matches!(err, AppError::NotFound { .. }), "{err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_refuses_a_row_whose_path_escapes_the_library() {
    let f = fixture().await;
    let a = f.import("Clip.mkv", 3).await;
    sqlx::query("UPDATE media_items SET relative_path = '../outside.mkv' WHERE id = ?1")
        .bind(&a.id)
        .execute(&f.storage.pool())
        .await
        .expect("corrupt row");

    let err = delete_item(&f.storage, &f.root, &a.id)
        .await
        .expect_err("traversal must be refused");
    assert!(matches!(err, AppError::OutOfLibrary { .. }), "{err:?}");
    assert_eq!(f.row_count().await, 1, "row is left alone");
}

/// Regression (P1-T10): files with Unicode names import fine, so they must
/// also play through `locast://` and delete. The importer keeps the name
/// (NFC-normalized); the library path validator must accept it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unicode_named_files_import_play_and_delete() {
    use locast_client_lib::library::protocol::{resolve_media_url, ProtocolHandler};

    let f = fixture().await;
    let handler = ProtocolHandler::new(f.storage.clone(), f.root.clone());
    // "Amelie.mp4" with an accented e, a Japanese katakana name, a mixed
    // Latin + CJK name with spaces, and a decomposed "Amelie" (e +
    // combining acute) that the importer stores as NFC.
    let cases = [
        ("Am\u{e9}lie.mp4", "Am\u{e9}lie.mp4"),
        (
            "\u{30b9}\u{30dd}\u{30ef}.mp4",
            "\u{30b9}\u{30dd}\u{30ef}.mp4",
        ),
        (
            "caf\u{e9} \u{96fb}\u{5f71} 2.mp4",
            "caf\u{e9} \u{96fb}\u{5f71} 2.mp4",
        ),
        ("Ame\u{301}lie 2.mp4", "Am\u{e9}lie 2.mp4"),
    ];
    for (seed, (source_name, stored_name)) in (1u8..).zip(cases) {
        let imported = f.import(source_name, seed).await;
        assert_eq!(
            imported.filename, stored_name,
            "importer keeps the name as NFC"
        );

        let url = resolve_media_url(&f.storage, &imported.id)
            .await
            .expect("resolve");
        let resp = handler
            .handle(&url, "GET", None)
            .await
            .unwrap_or_else(|e| panic!("{stored_name:?} must play: {e:?}"));
        assert_eq!(resp.status, 200, "{stored_name:?}");

        delete_item(&f.storage, &f.root, &imported.id)
            .await
            .unwrap_or_else(|e| panic!("{stored_name:?} must delete: {e:?}"));
        let trashed: Vec<PathBuf> = files_under(&f.root.join("trash"))
            .into_iter()
            .filter(|p| p.file_name().and_then(|n| n.to_str()) == Some(stored_name))
            .collect();
        assert_eq!(trashed.len(), 1, "{stored_name:?} moved to the trash");
        assert!(
            handler.handle(&url, "GET", None).await.is_err(),
            "{stored_name:?} no longer served after delete"
        );
    }
    assert_eq!(f.row_count().await, 0);
}
