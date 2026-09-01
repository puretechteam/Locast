//! P3-T12: `download_open` Tauri command.
//!
//! Wires the viewer's "I want this media" click into:
//!   1. verified-manifest / media resolution (`RoomClient` cache)
//!   2. P3-T11 dedup-on-download (BEFORE any transfer machinery)
//!   3. `AlreadyLocal` / `PromotedFromTemporary` -> mark complete,
//!      emit `download://state` + `download://progress`, no transfer
//!   4. `Missing` -> create the `downloads` row in `pending`,
//!      emit a `download://state=pending` event so the P3-T10
//!      modal appears. The actual transport-bound transfer start
//!      (which requires WebRTC transports) is wired in P3-T13+.
//!
//! The transfer machinery (the receiver/sender session layer, the
//! multi-source orchestrator, and the per-source scheduler) is NOT
//! referenced from this module. The proof that the dedup path
//! bypasses those types lives in
//! `tests/download_open_e2e.rs`.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State as TauriState;
use uuid::Uuid;

use crate::commands::error::AppError;
use crate::identity::keystore::IdentityService;
use crate::library::dedup::{dedup_on_download, DedupOutcome};
use crate::net::room::RoomClient;
use crate::storage::Storage;
use crate::transfer::events::{DownloadProgressEvent, DownloadStateEvent};
use crate::transfer::state::{DownloadStore, NewDownload};

/// Public result type returned to the webview.
///
/// `state` is one of the `DownloadState` strings ("pending",
/// "complete", "failed", ...). `dedup_hit` is true when the
/// command resolved locally without a transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct DownloadSessionIpc {
    pub download_id: String,
    pub media_id: String,
    pub state: String,
    pub dedup_hit: bool,
    pub total_bytes: u64,
    pub transferred_bytes: u64,
    pub on_disk_path: Option<String>,
}

/// Tauri command: open (or resume) a download for one media item
/// in the current room.
///
/// # Steps
///
/// 1. Verify the caller is in a room and the manifest is verified.
/// 2. Resolve the `MediaEntry` from the verified manifest cache.
/// 3. Look up the local `media_items` row (or create it from the
///    manifest, idempotently).
/// 4. Call `library::dedup::dedup_on_download` BEFORE any
///    transfer machinery.
/// 5. Branch on the dedup outcome.
#[tauri::command]
#[specta::specta]
pub async fn download_open(
    room: TauriState<'_, Arc<RoomClient>>,
    identity: TauriState<'_, Arc<IdentityService>>,
    storage: TauriState<'_, Storage>,
    media_id: String,
) -> Result<DownloadSessionIpc, AppError> {
    // 1. Verify caller is in a room.
    let summary = room
        .state()
        .await
        .ok_or_else(|| AppError::other("not in a room".to_string()))?;
    let room_id = Uuid::parse_str(&summary.id)
        .map_err(|e| AppError::other(format!("bad cached room id: {e}")))?;
    let manifest = room
        .verified_manifest(room_id)
        .ok_or_else(|| AppError::other("verified manifest not in cache".to_string()))?;

    // 2. Ensure a user_identities row exists (FK on downloads.user_id).
    //    Use ensure_user_row (P3-T12) instead of get_or_create so we
    //    do NOT clobber the user's existing display_name on conflict.
    let user_id = identity
        .ensure_user_row()
        .await
        .map_err(|e| AppError::other(format!("identity: {e}")))?;

    // 3. Library root (parent of the storage file).
    let library_root = crate::core::paths::library_root_for(storage.path())
        .ok_or_else(|| AppError::other("library root has no parent".to_string()))?
        .to_path_buf();

    // 4. Compute a fresh download_id.
    let download_id = Uuid::new_v4().to_string();

    open_download_inner(
        manifest,
        room_id,
        &summary.host_user_id,
        &user_id,
        storage.inner(),
        &library_root,
        &media_id,
        &download_id,
    )
    .await
}

/// Inner decision logic for `download_open`. Extracted so
/// integration tests can exercise the dedup-bypass path
/// without spinning up a Tauri `State` injection. The Tauri
/// command above is a thin wrapper that pulls the verified
/// manifest from `RoomClient` and calls this function.
///
/// This function never constructs any of the transfer-pipeline
/// types (the per-session receiver, the multi-source
/// orchestrator, or the per-source scheduler); the missing-path
/// branch leaves the download in `pending` for P3-T13 to
/// wire into the transfer pipeline.
#[allow(clippy::too_many_arguments)]
pub async fn open_download_inner(
    manifest: locast_manifest::MediaManifest,
    room_id: Uuid,
    room_host_user_id: &str,
    user_id: &str,
    storage: &Storage,
    library_root: &std::path::Path,
    media_id: &str,
    download_id: &str,
) -> Result<DownloadSessionIpc, AppError> {
    // 1. Resolve the verified MediaEntry from the manifest.
    let entry = manifest
        .media
        .iter()
        .find(|m| m.id == media_id)
        .ok_or_else(|| AppError::other(format!("media_id {media_id} not in verified manifest")))?
        .clone();

    // 2. Idempotently seed the media_items row from the
    //    verified manifest. If a row already exists, return
    //    its id unchanged.
    let resolved_media_id = upsert_media_item_from_manifest(storage, &entry).await?;

    // 3. The dedup check. THIS IS THE PIVOT: it runs BEFORE
    //    any transfer session creation.
    let dedup = dedup_on_download(storage, library_root, &entry.sha256, &entry.filename)
        .await
        .map_err(|e| AppError::other(format!("dedup: {e}")))?;

    let manifest_version: i64 = manifest.manifest_version as i64;
    let store = DownloadStore::new(storage.pool());
    let room_id_str = room_id.to_string();

    // P3-T12: a 0-byte media item is trivially "complete" (nothing
    // to fetch). DownloadStore::create requires >=1 chunk row, so
    // we bypass it with a direct INSERT.
    if entry.size_bytes == 0 {
        create_download_row_for_zero_bytes(
            &store,
            download_id,
            &resolved_media_id,
            Some(&room_id_str),
            user_id,
            &pick_primary_source_peer(&entry),
        )
        .await?;
        emit_state_and_progress(download_id, &resolved_media_id, "complete", 0, 0);
        return Ok(DownloadSessionIpc {
            download_id: download_id.to_string(),
            media_id: resolved_media_id,
            state: "complete".into(),
            dedup_hit: false,
            total_bytes: 0,
            transferred_bytes: 0,
            on_disk_path: None,
        });
    }

    match dedup {
        DedupOutcome::AlreadyLocal {
            on_disk_path,
            existing_status,
            ..
        } => {
            // Find-or-create the downloads row (P3-T12 idempotence).
            // If a prior downloads row already exists for this
            // (media, room, user), reuse it instead of creating a
            // duplicate.
            let (active_download_id, _created) = find_or_create_download_row(
                &store,
                download_id,
                &resolved_media_id,
                Some(&room_id_str),
                user_id,
                entry.size_bytes,
                room_host_user_id,
                manifest_version,
            )
            .await?;
            let chunk_hashes = pick_chunk_hashes_from_entry(&entry);
            store
                .mark_complete(
                    &active_download_id,
                    entry.size_bytes,
                    Some(room_host_user_id),
                    &chunk_hashes,
                )
                .await
                .map_err(|e| AppError::other(format!("mark_complete: {e}")))?;
            emit_state_and_progress(
                &active_download_id,
                &resolved_media_id,
                "complete",
                entry.size_bytes,
                entry.size_bytes,
            );
            let _ = existing_status;
            Ok(DownloadSessionIpc {
                download_id: active_download_id,
                media_id: resolved_media_id,
                state: "complete".into(),
                dedup_hit: true,
                total_bytes: entry.size_bytes,
                transferred_bytes: entry.size_bytes,
                on_disk_path: Some(on_disk_path),
            })
        }
        DedupOutcome::PromotedFromTemporary { on_disk_path, .. } => {
            let (active_download_id, _created) = find_or_create_download_row(
                &store,
                download_id,
                &resolved_media_id,
                Some(&room_id_str),
                user_id,
                entry.size_bytes,
                room_host_user_id,
                manifest_version,
            )
            .await?;
            let chunk_hashes = pick_chunk_hashes_from_entry(&entry);
            store
                .mark_complete(
                    &active_download_id,
                    entry.size_bytes,
                    Some(room_host_user_id),
                    &chunk_hashes,
                )
                .await
                .map_err(|e| AppError::other(format!("mark_complete: {e}")))?;
            emit_state_and_progress(
                &active_download_id,
                &resolved_media_id,
                "complete",
                entry.size_bytes,
                entry.size_bytes,
            );
            Ok(DownloadSessionIpc {
                download_id: active_download_id,
                media_id: resolved_media_id,
                state: "complete".into(),
                dedup_hit: true,
                total_bytes: entry.size_bytes,
                transferred_bytes: entry.size_bytes,
                on_disk_path: Some(on_disk_path),
            })
        }
        DedupOutcome::Missing => {
            // No local content. Find-or-create the downloads row
            // (P3-T12 idempotence) so a concurrent second call
            // for the same media collapses onto this row instead
            // of creating a duplicate.
            let (active_download_id, _created) = find_or_create_download_row(
                &store,
                download_id,
                &resolved_media_id,
                Some(&room_id_str),
                user_id,
                entry.size_bytes,
                &pick_primary_source_peer(&entry),
                manifest_version,
            )
            .await?;
            emit_state_and_progress(
                &active_download_id,
                &resolved_media_id,
                "pending",
                0,
                entry.size_bytes,
            );
            Ok(DownloadSessionIpc {
                download_id: active_download_id,
                media_id: resolved_media_id,
                state: "pending".into(),
                dedup_hit: false,
                total_bytes: entry.size_bytes,
                transferred_bytes: 0,
                on_disk_path: None,
            })
        }
    }
}

// ----- helpers below -----

#[allow(clippy::too_many_arguments)]
async fn create_download_row(
    store: &DownloadStore,
    download_id: &str,
    media_id: &str,
    room_id: Option<&str>,
    user_id: &str,
    total_bytes: u64,
    source_peer_id: &str,
    manifest_version: i64,
) -> Result<(), AppError> {
    let chunks: Vec<(u32, u64, u32, String)> = (0..total_chunks_for(total_bytes))
        .map(|i| {
            let offset = (i as u64) * (crate::transfer::CHUNK_SIZE_BYTES as u64);
            let length = std::cmp::min(
                crate::transfer::CHUNK_SIZE_BYTES as u64,
                total_bytes - offset,
            ) as u32;
            (i, offset, length, format!("{:064x}", i as u128))
        })
        .collect();
    let nd = NewDownload {
        download_id: download_id.to_string(),
        media_id: media_id.to_string(),
        room_id: room_id.map(|s| s.to_string()),
        user_id: user_id.to_string(),
        total_bytes,
        source_peer_id: source_peer_id.to_string(),
        chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
        manifest_version,
    };
    store
        .create(&nd, &chunks)
        .await
        .map_err(|e| AppError::other(format!("create downloads row: {e}")))
}

/// P3-T12 (per-call idempotence): SELECT-first then CREATE for
/// the downloads row. If an active or completed downloads row
/// already exists for the same `(media_id, room_id, user_id)`,
/// return its id without creating a new one. This makes two
/// concurrent `download_open` calls for the same media collapse
/// onto a single downloads row instead of producing two rows
/// that both transition into `transferring`.
///
/// Returns the existing or newly-created `download_id` and a
/// boolean `true` when a new row was created, `false` when an
/// existing one was reused.
///
/// The SELECT-then-CREATE pattern has a small race window
/// between the two queries. Documented as a v1 limitation; a
/// future migration can add a partial UNIQUE index over the
/// active state set as the canonical guard.
#[allow(clippy::too_many_arguments)]
async fn find_or_create_download_row(
    store: &DownloadStore,
    new_download_id: &str,
    media_id: &str,
    room_id: Option<&str>,
    user_id: &str,
    total_bytes: u64,
    source_peer_id: &str,
    manifest_version: i64,
) -> Result<(String, bool), AppError> {
    let room_key = room_id.unwrap_or("");
    let existing: Option<(String,)> = sqlx::query_as(
        "SELECT id FROM downloads \
         WHERE media_id = ?1 AND COALESCE(room_id, '') = ?2 AND user_id = ?3 \
         ORDER BY started_at DESC LIMIT 1",
    )
    .bind(media_id)
    .bind(room_key)
    .bind(user_id)
    .fetch_optional(store.pool())
    .await
    .map_err(|e| AppError::other(format!("downloads SELECT: {e}")))?;
    if let Some((id,)) = existing {
        return Ok((id, false));
    }
    create_download_row(
        store,
        new_download_id,
        media_id,
        room_id,
        user_id,
        total_bytes,
        source_peer_id,
        manifest_version,
    )
    .await?;
    Ok((new_download_id.to_string(), true))
}

fn total_chunks_for(total_bytes: u64) -> u32 {
    if total_bytes == 0 {
        0
    } else {
        total_bytes.div_ceil(crate::transfer::CHUNK_SIZE_BYTES as u64) as u32
    }
}

/// P3-T12: insert a `downloads` row directly in `complete` state
/// for a 0-byte media item. `DownloadStore::create` requires at
/// least one chunk row, which is impossible for 0 bytes; this
/// helper short-circuits that path. No `download_chunks` rows are
/// inserted (none should exist for a zero-byte file).
#[allow(clippy::too_many_arguments)]
async fn create_download_row_for_zero_bytes(
    store: &DownloadStore,
    download_id: &str,
    media_id: &str,
    room_id: Option<&str>,
    user_id: &str,
    source_peer_id: &str,
) -> Result<(), AppError> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    sqlx::query(
        "INSERT OR IGNORE INTO downloads (
            id, media_id, room_id, user_id, state, total_bytes, transferred_bytes,
            started_at, completed_at, last_error, chunk_size_bytes, manifest_version, source_peer_id
         ) VALUES (?1, ?2, ?3, ?4, 'complete', 0, 0, ?5, ?5, NULL, ?6, 1, ?7)",
    )
    .bind(download_id)
    .bind(media_id)
    .bind(room_id)
    .bind(user_id)
    .bind(now_ms)
    .bind(262_144_i64)
    .bind(source_peer_id)
    .execute(store.pool())
    .await
    .map_err(|e| AppError::other(format!("create zero-byte downloads row: {e}")))?;
    Ok(())
}

/// Pick the lowest-priority source's chunk hashes. If the
/// manifest has no sources, return an empty Vec (the caller
/// is in the dedup hit path and won't use these).
fn pick_chunk_hashes_from_entry(entry: &locast_manifest::MediaEntry) -> Vec<(u32, String)> {
    if entry.sources.is_empty() {
        return Vec::new();
    }
    let best = entry
        .sources
        .iter()
        .min_by_key(|s| s.priority)
        .expect("non-empty");
    best.chunk_hashes
        .iter()
        .enumerate()
        .map(|(i, h)| (i as u32, h.clone()))
        .collect()
}

fn pick_primary_source_peer(entry: &locast_manifest::MediaEntry) -> String {
    entry
        .sources
        .iter()
        .min_by_key(|s| s.priority)
        .map(|s| s.peer_id.clone())
        .unwrap_or_default()
}

fn emit_state_and_progress(
    download_id: &str,
    media_id: &str,
    state: &str,
    transferred_bytes: u64,
    total_bytes: u64,
) {
    let emitter = crate::get_download_event_emitter();
    emitter.record_state(DownloadStateEvent {
        v: 1,
        id: download_id.to_string(),
        media_id: media_id.to_string(),
        state: state.to_string(),
        error_message: None,
    });
    emitter.record_progress(DownloadProgressEvent {
        v: 1,
        id: download_id.to_string(),
        state: state.to_string(),
        transferred_bytes,
        total_bytes,
        bytes_per_sec_ema: 0.0,
        eta_seconds: None,
    });
}

/// Idempotently ensure a `media_items` row exists for
/// `entry.sha256`. Returns the existing-or-newly-created
/// `media_id`. The schema's `CHECK(status IN
/// ('permanent','temporary'))` requires status; new rows
/// start as `temporary` (architecture section 23.2 — a
/// download only promotes a row to `permanent` after the
/// dedup shortcut OR a normal transfer completes).
async fn upsert_media_item_from_manifest(
    storage: &Storage,
    entry: &locast_manifest::MediaEntry,
) -> Result<String, AppError> {
    let pool = storage.pool();
    let existing: Option<(String,)> =
        sqlx::query_as("SELECT id FROM media_items WHERE sha256 = ?1")
            .bind(&entry.sha256)
            .fetch_optional(&pool)
            .await
            .map_err(|e| AppError::other(format!("media_items SELECT: {e}")))?;
    if let Some((id,)) = existing {
        return Ok(id);
    }
    let new_id = Uuid::new_v4().to_string();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let sanitized = crate::core::library::sanitize::sanitize(&entry.filename)
        .map_err(|e| AppError::other(format!("filename sanitize: {e}")))?;
    let rel_path = format!(
        "library/{}/{}/{}/{}",
        &entry.sha256[0..2],
        &entry.sha256[2..4],
        entry.sha256,
        sanitized
    );
    sqlx::query(
        "INSERT OR IGNORE INTO media_items (\
            id, sha256, blake3, size_bytes, filename, relative_path, mime, \
            duration_ms, width, height, video_codec, audio_codec, container, \
            status, created_at, last_seen_at, last_room_id, source_url, provenance\
         ) VALUES (\
            ?1, ?2, ?3, ?4, ?5, ?6, 'application/octet-stream', \
            NULL, NULL, NULL, NULL, NULL, NULL, \
            'temporary', ?7, ?7, NULL, NULL, ?8\
         )",
    )
    .bind(&new_id)
    .bind(&entry.sha256)
    .bind(&entry.blake3)
    .bind(entry.size_bytes as i64)
    .bind(&sanitized)
    .bind(&rel_path)
    .bind(now_ms)
    .bind(r#"{"source":"room-manifest"}"#)
    .execute(&pool)
    .await
    .map_err(|e| AppError::other(format!("media_items INSERT: {e}")))?;
    let id: (String,) = sqlx::query_as("SELECT id FROM media_items WHERE sha256 = ?1")
        .bind(&entry.sha256)
        .fetch_one(&pool)
        .await
        .map_err(|e| AppError::other(format!("media_items SELECT post-INSERT: {e}")))?;
    Ok(id.0)
}
