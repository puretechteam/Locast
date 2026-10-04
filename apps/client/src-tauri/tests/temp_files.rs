//! Leave-room Keep / Delete flow: `get_temp_files`, `mark_files_permanent`,
//! `delete_files_to_trash` (`commands::temp_files`).
//!
//! Root cause this file guards against: the three commands existed and the
//! frontend called them, but they were missing from the Tauri
//! `invoke_handler`, so every call failed with "command not found" and the
//! leave-room modal swallowed the error.
//!
//! - `registration_*` tests parse the sources and run everywhere (including
//!   Windows) so a regression is caught on every host.
//! - `mock_ipc_*` runs the real `locast_client_lib::invoke_handler()` through
//!   Tauri's invoke path on the mock runtime. Like `library_ipc.rs` it is
//!   compiled only on Linux and macOS (the Windows test binary cannot start
//!   where the bundled `WebView2Loader.dll` is incompatible).
//! - the remaining tests drive the storage layer behind the commands against
//!   a real SQLite file and real files in a temporary library root.
//!
//! Cancel has no backend path: the modal's Cancel button only calls
//! `onClose`, so there is nothing for a backend test to assert.

use std::path::{Path, PathBuf};

use locast_client_lib::commands::temp_files::{
    delete_room_temp_files, keep_room_temp_files, list_room_temp_files,
};
use locast_client_lib::storage::Storage;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Registration contract (source-level; runs on every host)
// ---------------------------------------------------------------------------

fn read_manifest_file(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Names of every `#[tauri::command]` function in a source file.
fn tauri_command_names(src: &str) -> Vec<String> {
    let mut names = Vec::new();
    let lines: Vec<&str> = src.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "#[tauri::command]" {
            continue;
        }
        for next in &lines[i + 1..] {
            let t = next.trim();
            if let Some(rest) = t
                .strip_prefix("pub async fn ")
                .or_else(|| t.strip_prefix("pub fn "))
                .or_else(|| t.strip_prefix("async fn "))
                .or_else(|| t.strip_prefix("fn "))
            {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                names.push(name);
                break;
            }
        }
    }
    names
}

#[test]
fn registration_every_temp_files_command_is_in_the_invoke_handler() {
    let mut commands = tauri_command_names(&read_manifest_file("src/commands/temp_files.rs"));
    commands.sort();
    assert_eq!(
        commands,
        [
            "delete_files_to_trash",
            "get_temp_files",
            "mark_files_permanent"
        ],
        "unexpected command set in commands/temp_files.rs"
    );

    let lib = read_manifest_file("src/lib.rs");
    let start = lib
        .find("tauri::generate_handler![")
        .expect("lib.rs has a generate_handler! list");
    let list_end = lib[start..].find(']').map(|e| start + e).expect("list end");
    // Drop commented-out entries so a disabled line cannot satisfy the check.
    let handler: String = lib[start..list_end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for name in &commands {
        assert!(
            handler.contains(&format!("commands::temp_files::{name}")),
            "`{name}` is not registered in lib.rs invoke_handler()"
        );
    }

    // `run()` must use the shared list, not a second hand-written one.
    assert_eq!(
        lib.matches("generate_handler!").count(),
        1,
        "lib.rs must have exactly one generate_handler! list"
    );
    assert!(lib.contains(".invoke_handler(invoke_handler())"));
}

#[test]
fn get_temp_files_rejects_a_room_that_is_not_the_current_room() {
    use locast_client_lib::commands::temp_files::ensure_active_room;

    assert!(ensure_active_room(ROOM_A, ROOM_A).is_ok());
    let err = ensure_active_room(ROOM_A, ROOM_B).expect_err("other room is rejected");
    assert!(err.to_string().contains("not the current room"), "{err}");
}

#[test]
fn registration_typescript_invoke_names_match_rust_commands() {
    let bindings = read_manifest_file("../src/bindings/index.ts");
    // (TS wrapper, Rust command, argument key sent over IPC)
    for (wrapper, command, arg) in [
        ("getTempFiles", "get_temp_files", "roomId"),
        ("markFilesPermanent", "mark_files_permanent", "fileIds"),
        ("deleteFilesToTrash", "delete_files_to_trash", "fileIds"),
    ] {
        assert!(
            bindings.contains(&format!("async {wrapper}(")),
            "bindings are missing {wrapper}"
        );
        assert!(
            bindings.contains(&format!("\"{command}\", {{ {arg} }}")),
            "bindings invoke for {wrapper} is not \"{command}\" with {{ {arg} }}"
        );
    }
}

// ---------------------------------------------------------------------------
// Real SQLite + filesystem fixtures
// ---------------------------------------------------------------------------

const ROOM_A: &str = "11111111-1111-4111-8111-111111111111";
const ROOM_B: &str = "22222222-2222-4222-8222-222222222222";
const USER: &str = "user-local";

const T1: &str = "aaaaaaaa-0000-4000-8000-000000000001";
const T2: &str = "aaaaaaaa-0000-4000-8000-000000000002";
const OTHER_ROOM: &str = "bbbbbbbb-0000-4000-8000-000000000003";
const PERMANENT: &str = "cccccccc-0000-4000-8000-000000000004";
const IN_FLIGHT: &str = "dddddddd-0000-4000-8000-000000000005";
const NO_DOWNLOAD: &str = "eeeeeeee-0000-4000-8000-000000000006";
const UNKNOWN: &str = "ffffffff-0000-4000-8000-000000000007";

struct Fixture {
    dir: TempDir,
    storage: Storage,
}

fn sha_for(id: &str) -> String {
    // 64 hex chars derived from the id so every item is unique.
    let hex: String = id.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    hex.repeat(3)[..64].to_string()
}

fn rel_path(id: &str, filename: &str) -> String {
    let sha = sha_for(id);
    format!("library/{}/{}/{}/{}", &sha[0..2], &sha[2..4], sha, filename)
}

impl Fixture {
    async fn new() -> Self {
        let dir = TempDir::new().expect("tempdir");
        let storage = Storage::open(dir.path().join("index.sqlite"))
            .await
            .expect("storage opens");
        let pool = storage.pool();
        sqlx::query(
            "INSERT INTO user_identities (id, public_key, display_name, created_at, last_seen) \
             VALUES (?1, 'pk', 'Local', 1, 1)",
        )
        .bind(USER)
        .execute(&pool)
        .await
        .expect("user");
        for (id, code) in [(ROOM_A, "AAAAAA"), (ROOM_B, "BBBBBB")] {
            sqlx::query(
                "INSERT INTO rooms (id, code, host_user_id, created_at, state) \
                 VALUES (?1, ?2, ?3, 1, 'open')",
            )
            .bind(id)
            .bind(code)
            .bind(USER)
            .execute(&pool)
            .await
            .expect("room");
        }
        Self { dir, storage }
    }

    /// Insert a media row plus a real file on disk at its library path.
    async fn media(&self, id: &str, filename: &str, status: &str, created_at: i64) {
        let sha = sha_for(id);
        let rel = rel_path(id, filename);
        let full = self.dir.path().join(&rel);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
        std::fs::write(&full, format!("payload-{id}")).expect("write media file");
        sqlx::query(
            "INSERT INTO media_items (id, sha256, blake3, size_bytes, filename, relative_path, \
                mime, status, created_at, last_seen_at) \
             VALUES (?1, ?2, ?2, 1234, ?3, ?4, 'video/mp4', ?5, ?6, ?6)",
        )
        .bind(id)
        .bind(&sha)
        .bind(filename)
        .bind(&rel)
        .bind(status)
        .bind(created_at)
        .execute(&self.storage.pool())
        .await
        .expect("media row");
    }

    async fn download(&self, dl_id: &str, media_id: &str, room_id: &str, state: &str) {
        sqlx::query(
            "INSERT INTO downloads (id, media_id, room_id, user_id, state, total_bytes) \
             VALUES (?1, ?2, ?3, ?4, ?5, 1234)",
        )
        .bind(dl_id)
        .bind(media_id)
        .bind(room_id)
        .bind(USER)
        .bind(state)
        .execute(&self.storage.pool())
        .await
        .expect("download row");
    }

    /// Two room-A temporary items plus every kind of item that must NOT be
    /// affected when acting on room A.
    async fn seeded() -> Self {
        let f = Self::new().await;
        f.media(T1, "first.mp4", "temporary", 100).await;
        f.media(T2, "second.mkv", "temporary", 200).await;
        f.media(OTHER_ROOM, "other-room.mp4", "temporary", 300)
            .await;
        f.media(PERMANENT, "mine.mp4", "permanent", 400).await;
        f.media(IN_FLIGHT, "partial.mp4", "temporary", 500).await;
        f.media(NO_DOWNLOAD, "orphan.mp4", "temporary", 600).await;
        f.download("dl-1", T1, ROOM_A, "complete").await;
        f.download("dl-2", T2, ROOM_A, "complete").await;
        f.download("dl-3", OTHER_ROOM, ROOM_B, "complete").await;
        f.download("dl-4", PERMANENT, ROOM_A, "complete").await;
        f.download("dl-5", IN_FLIGHT, ROOM_A, "transferring").await;
        f
    }

    async fn status(&self, id: &str) -> Option<String> {
        sqlx::query_scalar::<_, String>("SELECT status FROM media_items WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.storage.pool())
            .await
            .expect("status query")
    }

    async fn download_rows(&self, media_id: &str) -> i64 {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM downloads WHERE media_id = ?1")
            .bind(media_id)
            .fetch_one(&self.storage.pool())
            .await
            .expect("count")
    }

    fn file(&self, id: &str, filename: &str) -> PathBuf {
        self.dir.path().join(rel_path(id, filename))
    }

    fn trashed(&self, filename: &str) -> Vec<PathBuf> {
        let trash = self.dir.path().join("trash");
        let Ok(entries) = std::fs::read_dir(&trash) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path().join(filename))
            .filter(|p| p.is_file())
            .collect()
    }
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

/// Every id the modal could plausibly send, including ones that must be
/// ignored.
fn mixed_ids() -> Vec<String> {
    ids(&[T1, OTHER_ROOM, PERMANENT, IN_FLIGHT, NO_DOWNLOAD, UNKNOWN])
}

// ---------------------------------------------------------------------------
// get_temp_files
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_temp_files_returns_only_the_rooms_completed_temporary_media() {
    let f = Fixture::seeded().await;

    let listed = list_room_temp_files(&f.storage, ROOM_A)
        .await
        .expect("list");
    let got: Vec<&str> = listed.iter().map(|t| t.file_id.as_str()).collect();
    assert_eq!(
        got,
        [T1, T2],
        "oldest first, room A temporary + complete only"
    );

    let first = &listed[0];
    assert_eq!(first.room_id, ROOM_A);
    assert_eq!(first.filename, "first.mp4");
    assert_eq!(first.size_bytes, 1234);
    assert_eq!(first.created_ms, 100);
    assert_eq!(first.owner_user_id, USER);

    let b = list_room_temp_files(&f.storage, ROOM_B)
        .await
        .expect("list B");
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].file_id, OTHER_ROOM);

    let none = list_room_temp_files(&f.storage, "99999999-9999-4999-8999-999999999999")
        .await
        .expect("list unknown room");
    assert!(none.is_empty());
}

// ---------------------------------------------------------------------------
// Keep (mark_files_permanent)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_marks_room_temporary_media_permanent_and_ignores_the_rest() {
    let f = Fixture::seeded().await;

    // Ask for T1 plus items that must be ignored. Nothing is an error.
    let changed = keep_room_temp_files(&f.storage, ROOM_A, &mixed_ids())
        .await
        .expect("keep");
    assert_eq!(changed, 1, "only T1 qualifies");

    assert_eq!(f.status(T1).await.as_deref(), Some("permanent"));
    // Not requested: still temporary.
    assert_eq!(f.status(T2).await.as_deref(), Some("temporary"));
    // Other room, in-flight, no download: untouched.
    assert_eq!(f.status(OTHER_ROOM).await.as_deref(), Some("temporary"));
    assert_eq!(f.status(IN_FLIGHT).await.as_deref(), Some("temporary"));
    assert_eq!(f.status(NO_DOWNLOAD).await.as_deref(), Some("temporary"));
    assert_eq!(f.status(PERMANENT).await.as_deref(), Some("permanent"));
    assert_eq!(f.status(UNKNOWN).await, None, "unknown id was not created");

    // Files stay exactly where they were and Keep never touches the trash.
    assert!(f.file(T1, "first.mp4").is_file());
    assert!(f.file(OTHER_ROOM, "other-room.mp4").is_file());
    assert!(!f.dir.path().join("trash").exists());

    // T1 no longer shows up as a temporary file; T2 still does.
    let left = list_room_temp_files(&f.storage, ROOM_A)
        .await
        .expect("list");
    let got: Vec<&str> = left.iter().map(|t| t.file_id.as_str()).collect();
    assert_eq!(got, [T2]);

    // Idempotent: a second Keep changes nothing.
    let again = keep_room_temp_files(&f.storage, ROOM_A, &mixed_ids())
        .await
        .expect("keep again");
    assert_eq!(again, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keep_in_one_room_does_not_affect_another_rooms_media() {
    let f = Fixture::seeded().await;
    // Acting as room A on a room-B item is a no-op.
    let changed = keep_room_temp_files(&f.storage, ROOM_A, &ids(&[OTHER_ROOM]))
        .await
        .expect("keep");
    assert_eq!(changed, 0);
    assert_eq!(f.status(OTHER_ROOM).await.as_deref(), Some("temporary"));
}

// ---------------------------------------------------------------------------
// Delete (delete_files_to_trash)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_moves_room_temporary_media_to_trash_and_ignores_the_rest() {
    let f = Fixture::seeded().await;

    let deleted = delete_room_temp_files(&f.storage, ROOM_A, &mixed_ids())
        .await
        .expect("delete");
    assert_eq!(deleted, 1, "only T1 qualifies");

    // T1: row gone (downloads cascade), file moved into <root>/trash.
    assert_eq!(f.status(T1).await, None);
    assert_eq!(f.download_rows(T1).await, 0);
    assert!(!f.file(T1, "first.mp4").exists(), "source file moved away");
    let in_trash = f.trashed("first.mp4");
    assert_eq!(in_trash.len(), 1, "exactly one trashed copy");
    assert_eq!(
        std::fs::read_to_string(&in_trash[0]).expect("read trashed"),
        format!("payload-{T1}")
    );
    let trash_dir = in_trash[0].parent().expect("trash entry dir");
    assert!(
        trash_dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(&format!("{}-", sha_for(T1)))),
        "trash entry is <sha256>-<unix_ms>: {}",
        trash_dir.display()
    );

    // Everything else is untouched: rows, download rows, and files.
    for (id, name, status) in [
        (T2, "second.mkv", "temporary"),
        (OTHER_ROOM, "other-room.mp4", "temporary"),
        (PERMANENT, "mine.mp4", "permanent"),
        (IN_FLIGHT, "partial.mp4", "temporary"),
        (NO_DOWNLOAD, "orphan.mp4", "temporary"),
    ] {
        assert_eq!(f.status(id).await.as_deref(), Some(status), "{id}");
        assert!(f.file(id, name).is_file(), "{id} file must stay in place");
        assert!(f.trashed(name).is_empty(), "{id} must not be trashed");
    }
    assert_eq!(f.download_rows(OTHER_ROOM).await, 1);
    assert_eq!(f.download_rows(PERMANENT).await, 1);

    // The deleted item no longer lists; the remaining one does.
    let left = list_room_temp_files(&f.storage, ROOM_A)
        .await
        .expect("list");
    let got: Vec<&str> = left.iter().map(|t| t.file_id.as_str()).collect();
    assert_eq!(got, [T2]);

    // Idempotent: deleting the same ids again changes nothing.
    let again = delete_room_temp_files(&f.storage, ROOM_A, &mixed_ids())
        .await
        .expect("delete again");
    assert_eq!(again, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_of_every_listed_file_empties_the_rooms_temp_list() {
    let f = Fixture::seeded().await;
    let listed = list_room_temp_files(&f.storage, ROOM_A)
        .await
        .expect("list");
    let all: Vec<String> = listed.into_iter().map(|t| t.file_id).collect();

    let deleted = delete_room_temp_files(&f.storage, ROOM_A, &all)
        .await
        .expect("delete all");
    assert_eq!(deleted, 2);
    assert!(list_room_temp_files(&f.storage, ROOM_A)
        .await
        .expect("list")
        .is_empty());
    assert_eq!(f.trashed("first.mp4").len(), 1);
    assert_eq!(f.trashed("second.mkv").len(), 1);
}

// ---------------------------------------------------------------------------
// Mock IPC through the real invoke_handler()
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
mod mock_ipc {
    use std::sync::Arc;

    use locast_client_lib::identity::keystore::IdentityService;
    use locast_client_lib::net::config::SignalingConfig;
    use locast_client_lib::net::room::RoomClient;
    use locast_client_lib::net::signaling::SignalingClient;
    use locast_client_lib::storage::Storage;
    use serde_json::{json, Value};
    use tauri::ipc::{CallbackFn, InvokeBody};
    use tauri::test::{get_ipc_response, mock_context, noop_assets, MockRuntime, INVOKE_KEY};
    use tauri::webview::InvokeRequest;
    use tauri::{WebviewWindow, WebviewWindowBuilder};
    use tempfile::TempDir;

    fn window(dir: &TempDir) -> WebviewWindow<MockRuntime> {
        let storage = tauri::async_runtime::block_on(async {
            Storage::open(dir.path().join("index.sqlite")).await
        })
        .expect("storage opens");
        let identity = Arc::new(IdentityService::new(storage.clone()));
        let signaling = Arc::new(SignalingClient::new(SignalingConfig::from_env(), identity));
        let room_client = Arc::new(RoomClient::new(signaling));

        // The exact handler list `run()` registers.
        let app = tauri::test::mock_builder()
            .manage(storage)
            .manage(room_client)
            .invoke_handler(locast_client_lib::invoke_handler())
            .build(mock_context(noop_assets()))
            .expect("mock app builds");
        let window = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("mock window builds");
        std::mem::forget(app);
        window
    }

    fn invoke(w: &WebviewWindow<MockRuntime>, cmd: &str, body: Value) -> Result<Value, Value> {
        get_ipc_response(
            w,
            InvokeRequest {
                cmd: cmd.into(),
                callback: CallbackFn(0),
                error: CallbackFn(1),
                url: "tauri://localhost".parse().expect("url"),
                body: InvokeBody::Json(body),
                headers: Default::default(),
                invoke_key: INVOKE_KEY.to_string(),
            },
        )
        .map(|b| b.deserialize::<Value>().expect("json response"))
    }

    /// Outside a room the commands run and reject with their own error. An
    /// unregistered command would instead be rejected with "not found".
    #[test]
    fn mock_ipc_temp_file_commands_are_registered_and_reachable() {
        let dir = TempDir::new().expect("tempdir");
        let w = window(&dir);
        let room = "11111111-1111-4111-8111-111111111111";
        let file = "aaaaaaaa-0000-4000-8000-000000000001";

        for (cmd, body) in [
            ("get_temp_files", json!({ "roomId": room })),
            ("mark_files_permanent", json!({ "fileIds": [file] })),
            ("delete_files_to_trash", json!({ "fileIds": [file] })),
        ] {
            let err = invoke(&w, cmd, body).expect_err("not in a room, so it rejects");
            let text = err.to_string();
            assert!(
                text.contains("not in a room"),
                "{cmd} was not reached as a registered command: {text}"
            );
        }
    }
}
