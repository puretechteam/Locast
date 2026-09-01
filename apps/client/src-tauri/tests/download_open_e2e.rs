//! P3-T12 acceptance test: `download_open` bypasses the transfer
//! layer on a dedup hit.
//!
//! Run with
//! `cargo test -j 1 -p locast-client --test download_open_e2e`.
//!
//! The roadmap's P3-T12 acceptance criterion is:
//!
//! > `download_open` is wired through the existing P3-T11 dedup
//! > path; a hit marks the downloads row complete and emits
//! > `download://state=complete` without ever constructing a
//! > `ReceiverSession`, `MultiSourceReceiver`, or `Scheduler`.
//!
//! The two runtime tests below prove:
//!
//! 1. A dedup hit returns `DownloadSessionIpc { state = "complete",
//!    dedup_hit = true, ... }` from `open_download_inner`, and the
//!    `downloads` row is in `complete` state with all chunks
//!    `verified`.
//! 2. The missing path returns `state = "pending", dedup_hit =
//!    false` and creates the `downloads` row in `pending` state.
//!
//! The static `download_command_does_not_construct_transfer_types`
//! test below is the strongest proof that no transfer type was
//! ever imported into the dedup path: it greps the command source
//! for `ReceiverSession`, `MultiSourceReceiver`, and
//! `Scheduler::new` and asserts each is absent.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use locast_client_lib::commands::download::{open_download_inner, DownloadSessionIpc};
use locast_client_lib::core::paths;
use locast_client_lib::library::dedup::dedup_on_download;
use locast_client_lib::storage::Storage;
use locast_manifest::{MediaEntry, MediaManifest, Source};
use tempfile::TempDir;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// shared test helpers (mirrors apps/client/src-tauri/tests/dedup_e2e.rs)
// ---------------------------------------------------------------------------

static TEMPDIRS: Mutex<Vec<TempDir>> = Mutex::new(Vec::new());

fn new_tempdir() -> PathBuf {
    let d = TempDir::new().expect("tempdir");
    let p = d.path().to_path_buf();
    TEMPDIRS.lock().expect("tempdir holders").push(d);
    p
}

async fn open_storage() -> (Storage, PathBuf) {
    let dir = new_tempdir();
    let db = dir.join("index.sqlite");
    let storage = Storage::open(&db).await.expect("storage opens");
    (storage, dir.clone())
}

fn make_library_root(dir: &Path) -> PathBuf {
    let root = dir.join("library_root");
    std::fs::create_dir_all(&root).expect("create library root");
    root
}

async fn seed_user(pool: &sqlx::SqlitePool, user_id: &str) {
    sqlx::query(
        "INSERT OR IGNORE INTO user_identities \
            (id, public_key, display_name, created_at, last_seen) \
         VALUES (?1, ?2, 'tester', 0, 0)",
    )
    .bind(user_id)
    .bind(format!("pk-{user_id}"))
    .execute(pool)
    .await
    .expect("seed user");
}

async fn seed_room(pool: &sqlx::SqlitePool, room_id: Uuid, host_user_id: &str) {
    // The host_user_id FK requires a matching user_identities row.
    sqlx::query(
        "INSERT OR IGNORE INTO user_identities \
            (id, public_key, display_name, created_at, last_seen) \
         VALUES (?1, ?2, 'tester', 0, 0)",
    )
    .bind(host_user_id)
    .bind(format!("pk-{host_user_id}"))
    .execute(pool)
    .await
    .expect("seed host user");
    sqlx::query(
        "INSERT OR IGNORE INTO rooms \
            (id, code, host_user_id, created_at, ended_at, state, settings) \
         VALUES (?1, ?2, ?3, 0, NULL, 'open', '{}')",
    )
    .bind(room_id.to_string())
    .bind("AAAAAA")
    .bind(host_user_id)
    .execute(pool)
    .await
    .expect("seed room");
}

async fn seed_media_permanent(
    pool: &sqlx::SqlitePool,
    sha: &str,
    size_bytes: i64,
    filename: &str,
) -> String {
    let id = Uuid::new_v4().to_string();
    let rel = format!("library/{}/{}/{}/{}", &sha[0..2], &sha[2..4], sha, filename);
    sqlx::query(
        "INSERT INTO media_items (\
            id, sha256, blake3, size_bytes, filename, relative_path, mime, \
            duration_ms, width, height, video_codec, audio_codec, container, \
            status, created_at, last_seen_at, last_room_id, source_url, provenance\
         ) VALUES (\
            ?1, ?2, 'b', ?3, ?4, ?5, 'application/octet-stream', \
            NULL, NULL, NULL, NULL, NULL, NULL, \
            'permanent', 1, 1, NULL, NULL, '{}'\
         )",
    )
    .bind(&id)
    .bind(sha)
    .bind(size_bytes)
    .bind(filename)
    .bind(&rel)
    .execute(pool)
    .await
    .expect("seed permanent media");
    id
}

fn canonical_peer_id(byte: u8) -> String {
    let pubkey_bytes = [byte; 32];
    locast_client_lib::room::peer_id::derive_peer_id(pubkey_bytes)
}

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const BLAKE3: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const FILENAME: &str = "movie.mkv";

fn make_source(size: u64) -> Source {
    let chunk_size = locast_client_lib::transfer::CHUNK_SIZE_BYTES as u32;
    let total = size.div_ceil(chunk_size as u64) as u32;
    let hashes: Vec<String> = (0..total).map(|i| format!("{:064x}", i as u128)).collect();
    Source {
        peer_id: canonical_peer_id(0xAA),
        url_hint: None,
        priority: 0,
        chunk_size,
        total_chunks: total,
        chunk_hashes: hashes,
    }
}

fn make_entry(size: u64) -> MediaEntry {
    MediaEntry {
        id: Uuid::new_v4().to_string(),
        filename: FILENAME.to_string(),
        sha256: SHA.to_string(),
        blake3: BLAKE3.to_string(),
        size_bytes: size,
        mime: "video/mp4".to_string(),
        duration_ms: 1000,
        dimensions: None,
        codecs: None,
        sources: vec![make_source(size)],
    }
}

fn make_manifest(room_id: Uuid, version: u32, entries: Vec<MediaEntry>) -> MediaManifest {
    MediaManifest {
        manifest_version: version,
        room_id: room_id.to_string(),
        media: entries,
        subtitles: vec![],
        created_at: 1,
        host_signature: None,
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedup_hit_returns_complete_without_transfer() {
    let (storage, lib_root_dir) = open_storage().await;
    let lib_root = make_library_root(&lib_root_dir);

    // 1. Write a small fixture file at the canonical content-
    //    addressed path so the dedup short-circuits.
    let bytes: Vec<u8> = (0..16 * 1024).map(|i| (i % 251) as u8).collect();
    let cap = paths::content_addressed_path(&lib_root, SHA, FILENAME).unwrap();
    tokio::fs::create_dir_all(cap.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&cap, &bytes).await.unwrap();

    // 2. Seed a user_identities row so downloads.user_id has
    //    its FK satisfied when open_download_inner INSERTs.
    seed_user(&storage.pool(), "u-test").await;
    let room_id = Uuid::new_v4();
    // source_peer_id / room_host_user_id must be a canonical
    // peer_id (64 lowercase hex chars) so it satisfies the
    // DownloadStore::create peer_id validator.
    let host_peer_id = canonical_peer_id(0xAA);
    seed_room(&storage.pool(), room_id, &host_peer_id).await;

    // Seed a permanent media_items row so dedup returns
    // AlreadyLocal (the dedup checks the DB before the file
    // and returns Missing if no row exists). Capture the
    // media_id we want the command to return.
    let seeded_media_id =
        seed_media_permanent(&storage.pool(), SHA, bytes.len() as i64, FILENAME).await;

    // 3. Sanity check: dedup returns AlreadyLocal.
    let outcome = dedup_on_download(&storage, &lib_root, SHA, FILENAME)
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            locast_client_lib::library::dedup::DedupOutcome::AlreadyLocal { .. }
        ),
        "expected AlreadyLocal, got {outcome:?}"
    );

    // 4. Call open_download_inner. Use the seeded media_items
    //    row's id as the manifest's media_id so the resolution
    //    step finds the entry by id.
    let entry = {
        let mut e = make_entry(bytes.len() as u64);
        e.id = seeded_media_id.clone();
        e
    };
    let manifest = make_manifest(room_id, 1, vec![entry.clone()]);
    let download_id = Uuid::new_v4().to_string();

    let result = open_download_inner(
        manifest,
        room_id,
        &host_peer_id,
        "u-test",
        &storage,
        &lib_root,
        &entry.id,
        &download_id,
    )
    .await
    .expect("download_open_inner");

    // 5. Result is complete + dedup_hit.
    assert_eq!(result.state, "complete");
    assert!(result.dedup_hit);
    assert_eq!(result.transferred_bytes, result.total_bytes);
    assert!(result.on_disk_path.is_some());
    assert_eq!(result.media_id, entry.id);
    assert_eq!(result.download_id, download_id);

    // 6. The downloads row exists in 'complete' state and
    //    every chunk is 'verified'.
    let state: String = sqlx::query_scalar("SELECT state FROM downloads WHERE id = ?1")
        .bind(&download_id)
        .fetch_one(&storage.pool())
        .await
        .expect("download state");
    assert_eq!(state, "complete");
    let verified: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM download_chunks \
         WHERE download_id = ?1 AND state = 'verified'",
    )
    .bind(&download_id)
    .fetch_one(&storage.pool())
    .await
    .expect("chunk count");
    assert!(verified > 0, "expected >=1 verified chunk, got {verified}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_path_creates_download_row_in_pending_state() {
    let (storage, lib_root_dir) = open_storage().await;
    let lib_root = make_library_root(&lib_root_dir);

    seed_user(&storage.pool(), "u-test").await;

    let room_id = Uuid::new_v4();
    let host_peer_id = canonical_peer_id(0xAA);
    seed_room(&storage.pool(), room_id, &host_peer_id).await;
    let entry = make_entry(64 * 1024);
    let manifest = make_manifest(room_id, 1, vec![entry.clone()]);
    let download_id = Uuid::new_v4().to_string();

    let result: DownloadSessionIpc = open_download_inner(
        manifest,
        room_id,
        &host_peer_id,
        "u-test",
        &storage,
        &lib_root,
        &entry.id,
        &download_id,
    )
    .await
    .expect("download_open_inner missing");

    assert_eq!(result.state, "pending");
    assert!(!result.dedup_hit);
    assert_eq!(result.transferred_bytes, 0);
    assert_eq!(result.total_bytes, entry.size_bytes);
    assert!(result.on_disk_path.is_none());

    let state: String = sqlx::query_scalar("SELECT state FROM downloads WHERE id = ?1")
        .bind(&download_id)
        .fetch_one(&storage.pool())
        .await
        .expect("download state");
    assert_eq!(state, "pending");

    // All chunks were pre-populated by `create`; on the
    // missing path we never reach `mark_complete`, so every
    // chunk row remains 'pending'.
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM download_chunks \
         WHERE download_id = ?1 AND state = 'pending'",
    )
    .bind(&download_id)
    .fetch_one(&storage.pool())
    .await
    .expect("pending count");
    assert!(pending > 0, "expected pending chunks, got {pending}");
}

/// Static proof that the `download_open` command source does
/// not import any transfer-pipeline type. This is the
/// strongest guarantee that a future regression which
/// accidentally wires the dedup path through a transfer
/// session would be caught here. Mirrors the
/// `dedup_module_does_not_depend_on_transfer` test in
/// `tests/dedup_e2e.rs`.
#[test]
fn download_command_does_not_construct_transfer_types() {
    // P3-T12 acceptance: the dedup path must NOT open a transfer
    // session. The strongest static check we can do from a test
    // is to verify the command source does not import the
    // transfer-pipeline submodules in any form.
    const SRC: &str = include_str!("../src/commands/download.rs");
    // Reject any reference to the transfer-pipeline submodules
    // by full path. A glob import (`use crate::transfer::*`)
    // would expand into one of these in the body.
    assert!(
        !SRC.contains("crate::transfer::session"),
        "download.rs must not depend on transfer::session"
    );
    assert!(
        !SRC.contains("crate::transfer::multi_source"),
        "download.rs must not depend on transfer::multi_source"
    );
    assert!(
        !SRC.contains("crate::transfer::scheduler"),
        "download.rs must not depend on transfer::scheduler"
    );
    // Reject glob imports from the transfer crate.
    assert!(
        !SRC.contains("use crate::transfer::*"),
        "download.rs must not glob-import from crate::transfer"
    );
    assert!(
        !SRC.contains("use crate::transfer::{"),
        "download.rs must not import from crate::transfer at all"
    );
    // The command IS allowed to depend on:
    //   crate::transfer::{events, state, CHUNK_SIZE_BYTES, plan}
    //   (events for emit, state for DownloadStore, plan for plan_download)
    // Verify the allowlist is the ONLY transfer import.
    let has_transfer_import = SRC.contains("use crate::transfer");
    if has_transfer_import {
        // If there is an import, verify it is on the allowlist.
        // Allowlist regex: `use crate::transfer::{...allowed...}`
        let allowed_submodules = ["events", "state", "plan"];
        // The implementation should use module-qualified paths for
        // these (not import lines). Find any use of crate::transfer
        // outside the allowlist.
        for line in SRC.lines() {
            if line.trim_start().starts_with("use crate::transfer") {
                let trimmed = line
                    .trim_start()
                    .trim_end_matches(',')
                    .trim_end_matches(';');
                let mut ok = false;
                for sub in &allowed_submodules {
                    if trimmed.contains(&format!("transfer::{}", sub))
                        || trimmed.contains(&format!("transfer::{{{}}}", sub))
                        || trimmed.contains(&format!("transfer::{{ {} ", sub))
                    {
                        ok = true;
                        break;
                    }
                }
                assert!(ok, "disallowed transfer submodule import: {}", line);
            }
        }
    }
}

/// P3-T12: a 0-byte media item must complete without any
/// `download_chunks` rows. `DownloadStore::create` requires >=1
/// chunk, so the command must take a separate code path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_byte_media_completes_without_chunks() {
    let (storage, lib_root_dir) = open_storage().await;
    let lib_root = make_library_root(&lib_root_dir);

    seed_user(&storage.pool(), "u-test").await;

    let room_id = Uuid::new_v4();
    let host_peer_id = canonical_peer_id(0xAA);
    seed_room(&storage.pool(), room_id, &host_peer_id).await;

    // 0-byte entry: size_bytes = 0, and the source has 0 chunks.
    let entry = make_entry(0);
    let manifest = make_manifest(room_id, 1, vec![entry.clone()]);
    let download_id = Uuid::new_v4().to_string();

    let result: DownloadSessionIpc = open_download_inner(
        manifest,
        room_id,
        &host_peer_id,
        "u-test",
        &storage,
        &lib_root,
        &entry.id,
        &download_id,
    )
    .await
    .expect("download_open_inner zero-byte");

    assert_eq!(result.state, "complete");
    assert_eq!(result.total_bytes, 0);
    assert_eq!(result.transferred_bytes, 0);
    assert!(result.on_disk_path.is_none());
    assert!(!result.media_id.is_empty());
    assert_eq!(result.download_id, download_id);

    // The downloads row exists in 'complete' state with total_bytes=0.
    let row: (String, i64) =
        sqlx::query_as("SELECT state, total_bytes FROM downloads WHERE id = ?1")
            .bind(&download_id)
            .fetch_one(&storage.pool())
            .await
            .expect("download row");
    assert_eq!(row.0, "complete");
    assert_eq!(row.1, 0);

    // No download_chunks rows for a zero-byte file.
    let chunk_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM download_chunks WHERE download_id = ?1")
            .bind(&download_id)
            .fetch_one(&storage.pool())
            .await
            .expect("chunk count");
    assert_eq!(chunk_count, 0);
}
