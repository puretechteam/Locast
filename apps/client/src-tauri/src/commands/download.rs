//! P3-T12 / P3-T13: `download_open` Tauri command.
//!
//! Wires the viewer's "I want this media" click into:
//!   1. verified-manifest / media resolution (`RoomClient` cache)
//!   2. P3-T11 dedup-on-download (BEFORE any transfer machinery)
//!   3. `AlreadyLocal` / `PromotedFromTemporary` -> mark complete,
//!      emit `download://state` + `download://progress`, no transfer
//!   4. `Missing` -> create the `downloads` row in `pending`,
//!      build a `DownloadPlan`, attach a per-source WebRTC
//!      transport (via the `WebRtcManager`), and spawn
//!      `MultiSourceReceiver::run_multi_source` against those
//!      transports. The orchestrator's `JoinHandle` and
//!      `CancellationToken` are registered with the
//!      `TransferRegistry` so room_leave / shutdown can cancel
//!      them cleanly.
//!
//! The dedup path does NOT touch any of the transfer-pipeline
//! types; the missing path goes through
//! `transfer::plan / multi_source / scheduler / webrtc_transport`.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State as TauriState;
use tracing::{info, warn};
use uuid::Uuid;

use crate::commands::error::AppError;
use crate::identity::keystore::IdentityService;
use crate::library::dedup::{dedup_on_download, DedupOutcome};
use crate::net::room::RoomClient;
use crate::net::webrtc::WebRtcManager;
use crate::storage::Storage;
use crate::transfer::events::{DownloadProgressEvent, DownloadStateEvent};
use crate::transfer::multi_source::{run_multi_source, MultiSourceReceiver, SourceHandle};
use crate::transfer::plan::plan_download;
use crate::transfer::registry::TransferRegistry;
use crate::transfer::scheduler::Scheduler;
use crate::transfer::state::{DownloadStore, NewDownload};
use crate::transfer::transport::Transport;
use crate::transfer::webrtc_transport::WebRtcTransport;

use sqlx::error::ErrorKind;

/// SQLITE_CONSTRAINT_UNIQUE (matches libsqlite3's primary
/// error code for unique-index violations). Used as a
/// belt-and-suspenders fallback when the sqlx [`ErrorKind`]
/// enum does not name the variant (older sqlx releases).
#[allow(dead_code)]
const SQLITE_CONSTRAINT_UNIQUE: &str = "2067";

/// P3-T13 review fix F#23: detect a UNIQUE-index violation on
/// the partial `ux_downloads_active` index without relying on
/// fragile string matching. First tries the structured
/// [`ErrorKind::UniqueViolation`] discriminator; falls back to
/// either the SQLSTATE code (SQLITE_CONSTRAINT_UNIQUE = 2067)
/// or the human-readable message text.
///
/// The `DownloadStore::create` API wraps the underlying
/// [`sqlx::Error`] into a `ChunkStateError::Sqlx(String)`,
/// so the structured `sqlx::Error` is not available at the
/// call site. To preserve the structured error for
/// robustness we expose both shapes:
///
/// * `is_unique_violation_sqlx(&sqlx::Error)` -- structured
///   match against `sqlx::Error::Database`.
/// * `is_unique_violation_chunk(&ChunkStateError)` -- Display
///   fallback for the wrapped error string.
#[allow(dead_code)]
fn is_unique_violation_sqlx(e: &sqlx::Error) -> bool {
    if let sqlx::Error::Database(db_err) = e {
        if matches!(db_err.kind(), ErrorKind::UniqueViolation) {
            return true;
        }
        if db_err.code().as_deref() == Some(SQLITE_CONSTRAINT_UNIQUE) {
            return true;
        }
        let msg = db_err.message();
        return msg.contains("UNIQUE constraint failed") || msg.contains("ux_downloads_active");
    }
    false
}

fn is_unique_violation_chunk(e: &crate::transfer::state::ChunkStateError) -> bool {
    let msg = e.to_string();
    msg.contains("UNIQUE constraint failed") || msg.contains("ux_downloads_active")
}

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
    webrtc: TauriState<'_, Arc<WebRtcManager>>,
    registry: TauriState<'_, Arc<TransferRegistry>>,
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

    // 4. P3-T13: pull the local pubkey only on the
    //    Missing-path. Review fix A#25: the dedup hits
    //    (AlreadyLocal / PromotedFromTemporary) and the
    //    zero-byte shortcut never need a keypair, so defer
    //    the load into the Missing arm of `open_download_inner`.
    //    The `identity` arg is still passed through so the
    //    inner helper can load it when it actually needs it.
    let _ = &identity;

    // 5. Compute a fresh download_id.
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
        &webrtc,
        &registry,
        identity.inner().clone(),
    )
    .await
}

/// Inner decision logic for `download_open`. Extracted so
/// integration tests can exercise the dedup-bypass path
/// without spinning up a Tauri `State` injection. The Tauri
/// command above is a thin wrapper that pulls the verified
/// manifest from `RoomClient` and calls this function.
///
/// P3-T13: the `Missing` branch now also wires the actual
/// transfer. The dedup-bypass branches still do NOT construct
/// any transfer-pipeline types.
///
/// P3-T13 review fix A#25: the keypair is loaded lazily
/// inside the `Missing` arm only. The outer signature now
/// takes `identity: Arc<IdentityService>` instead of a
/// pre-derived `local_pubkey: [u8; 32]` so the load happens
/// only on the path that actually needs it.
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
    webrtc: &Arc<WebRtcManager>,
    registry: &Arc<TransferRegistry>,
    identity: Arc<IdentityService>,
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
            // P3-T13 review fix A#25: load the keypair here,
            // only on the path that needs it (the dedup hits
            // never touch transport-layer code, so they don't
            // need the pubkey).
            let kp = identity
                .load_keypair()
                .await
                .map_err(|e| AppError::other(format!("identity load_keypair: {e}")))?;
            let local_pubkey: [u8; 32] = kp.signing.verifying_key().to_bytes();
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
            // P3-T13: wire the actual transfer on the Missing
            // path. We need:
            //   1. a `user_id -> [u8;32] pubkey` lookup table
            //      (built from `user_identities.public_key` for
            //      every room participant);
            //   2. a `WebRtcTransport` per `entry.sources[]`
            //      whose DataChannel can be located by the
            //      manifest `peer_id`;
            //   3. a `DownloadPlan` for `entry`;
            //   4. a `MultiSourceReceiver` and a spawn-and-register
            //      step.
            //
            // If we end up with zero authenticated transports
            // (the room is empty, all peers are still
            // negotiating, or the manifest references an
            // unknown participant) we transition the row to
            // `failed` and surface a structured error so the
            // webview can show the failure to the user.
            let participant_user_ids: Vec<String> =
                load_room_participant_user_ids(storage, room_id).await?;
            let user_pubkey_cache = build_user_pubkey_cache(storage, &participant_user_ids).await?;
            let mut source_handles: Vec<SourceHandle> = Vec::new();
            let mut skipped: Vec<String> = Vec::new();
                for src in &entry.sources {
                    if !crate::room::peer_id::is_canonical_peer_id(&src.peer_id) {
                        skipped.push(src.peer_id.clone());
                        continue;
                    }
                let lookup_cache = user_pubkey_cache.clone();
                let dc = webrtc
                    .lookup_dc_by_peer_id(&src.peer_id, move |uid| lookup_cache.get(&uid).copied())
                    .await;
                let Some(dc) = dc else {
                    skipped.push(src.peer_id.clone());
                    continue;
                };
                let cancel_token = tokio_util::sync::CancellationToken::new();
                let transport: Arc<dyn Transport> =
                    Arc::new(WebRtcTransport::new(dc, cancel_token.clone()));
                let scheduler = Arc::new(Scheduler::new(transport.clone(), cancel_token.clone()));
                source_handles.push(SourceHandle {
                    peer_id: src.peer_id.clone(),
                    transport,
                    priority: src.priority,
                    sched: scheduler,
                    demotion_count: 0,
                    unavailable: false,
                    unavailable_since: None,
                    cancel: cancel_token,
                    rtt_samples: std::collections::VecDeque::new(),
                });
            }

            if source_handles.is_empty() {
                // P3-T13: no transport available right now. The
                // row stays in `pending` (the peer may still be
                // negotiating) but we record the reason on
                // `last_error` for the webview modal. Cancelling
                // / transitioning to Failed would force the user
                // to re-open the download from scratch every time
                // a peer is briefly disconnected; v1 lets the row
                // wait for a future retry path.
                let msg = format!(
                    "no authenticated source transport available yet (manifest references peers {:?} but none are currently connected); download left in pending and will be retried by the follow-up path",
                    skipped
                );
                warn!(
                    download_id = %active_download_id,
                    sha256 = %entry.sha256,
                    skipped = ?skipped,
                    "missing path: zero transports"
                );
                if let Err(e) = store.set_last_error(&active_download_id, &msg).await {
                    warn!(error = %e, "set_last_error on no-source failed");
                }
                emit_state_and_progress(
                    &active_download_id,
                    &resolved_media_id,
                    "pending",
                    0,
                    entry.size_bytes,
                );
                return Ok(DownloadSessionIpc {
                    download_id: active_download_id,
                    media_id: resolved_media_id,
                    state: "pending".into(),
                    dedup_hit: false,
                    total_bytes: entry.size_bytes,
                    transferred_bytes: 0,
                    on_disk_path: None,
                });
            }

            let primary_peer_id = pick_primary_source_peer(&entry);
            let plan = plan_download(
                &active_download_id,
                &resolved_media_id,
                manifest_version,
                &entry,
                &primary_peer_id,
            )
            .map_err(|e| AppError::other(format!("plan_download: {e}")))?;
            let plan = Arc::new(plan);
            let receiver = MultiSourceReceiver::new(
                plan.clone(),
                store.clone(),
                library_root.to_path_buf(),
                local_pubkey,
                source_handles,
            )
            .map_err(|e| AppError::other(format!("MultiSourceReceiver::new: {e}")))?;
            let receiver = Arc::new(receiver);

            // Spawn the orchestrator. Register the
            // CancellationToken with the TransferRegistry so
            // room_leave / app shutdown can cancel_all. After
            // the orchestrator returns (Ok or Err) the spawn
            // closure unregisters the id, so the registry does
            // not leak entries for transfers that completed
            // gracefully. P3-T13 review fix A#4/D#19: the
            // JoinHandle is now owned by the closure (not the
            // registry), which makes the unregister step
            // straightforward.
            let registry_for_task = registry.clone();
            let download_id_for_task = active_download_id.clone();
            let cancel_for_registry = receiver.cancel_handle();
            let filename_for_task = entry.filename.clone();
            tokio::spawn(async move {
                registry_for_task
                    .register(download_id_for_task.clone(), cancel_for_registry)
                    .await;
                let result = run_multi_source(receiver, filename_for_task).await;
                registry_for_task.unregister(&download_id_for_task).await;
                match result {
                    Ok(state) => {
                        info!(?state, download_id = %download_id_for_task, "download complete")
                    }
                    Err(e) => {
                        warn!(download_id = %download_id_for_task, error = %e, "download failed")
                    }
                }
            });
            let _ = registry_for_task;

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

/// P3-T13: load the `user_id` (string) of every participant in
/// `room_id`. We need this list to fetch each participant's
/// Ed25519 pubkey from `user_identities`, which is what lets the
/// WebRtcManager turn a manifest `peer_id` (sha256(pubkey) hex)
/// into a participant `user_id` and then into a connected
/// DataChannel.
async fn load_room_participant_user_ids(
    storage: &Storage,
    room_id: Uuid,
) -> Result<Vec<String>, AppError> {
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT user_id FROM room_participants WHERE room_id = ?1")
            .bind(room_id.to_string())
            .fetch_all(&storage.pool())
            .await
            .map_err(|e| AppError::other(format!("room_participants SELECT: {e}")))?;
    Ok(rows.into_iter().map(|(u,)| u).collect())
}

/// P3-T13: build the `user_id -> [u8;32] pubkey` cache the
/// `WebRtcManager::lookup_dc_by_peer_id` closure needs. The
/// `user_identities.public_key` column is base64-encoded 32
/// bytes; we decode it here. Participants without a row
/// (e.g. we never met them) are silently skipped.
async fn build_user_pubkey_cache(
    storage: &Storage,
    user_ids: &[String],
) -> Result<HashMap<Uuid, [u8; 32]>, AppError> {
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    let mut out: HashMap<Uuid, [u8; 32]> = HashMap::new();
    for uid_str in user_ids {
        let Ok(uid) = Uuid::parse_str(uid_str) else {
            continue;
        };
        let row: Option<(String,)> =
            sqlx::query_as("SELECT public_key FROM user_identities WHERE id = ?1")
                .bind(uid_str)
                .fetch_optional(&storage.pool())
                .await
                .map_err(|e| AppError::other(format!("user_identities SELECT: {e}")))?;
        let Some((pk_b64,)) = row else { continue };
        let pk_bytes = match BASE64.decode(pk_b64.as_bytes()) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if pk_bytes.len() != 32 {
            continue;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&pk_bytes);
        out.insert(uid, arr);
    }
    Ok(out)
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
/// P3-T13: a partial UNIQUE index
/// `ux_downloads_active(media_id, COALESCE(room_id,''), user_id)
/// WHERE state IN ('pending', 'connecting', 'transferring',
/// 'verifying', 'paused')` is the canonical guard against two
/// in-flight active rows. The SELECT-then-CREATE pattern still
/// has a small race window between the two queries; on a race,
/// the INSERT raises a `UNIQUE constraint failed:
/// ux_downloads_active` error, which we catch and SELECT the
/// existing row.
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
    // Build the NewDownload / chunks and try to insert. If the
    // partial UNIQUE index fires (another caller won the race
    // between our SELECT and our INSERT), fall through to the
    // race-recovery SELECT.
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
        download_id: new_download_id.to_string(),
        media_id: media_id.to_string(),
        room_id: room_id.map(|s| s.to_string()),
        user_id: user_id.to_string(),
        total_bytes,
        source_peer_id: source_peer_id.to_string(),
        chunk_size_bytes: crate::transfer::CHUNK_SIZE_BYTES as u32,
        manifest_version,
    };
    match store.create(&nd, &chunks).await {
        Ok(()) => Ok((new_download_id.to_string(), true)),
        Err(e) if is_unique_violation_chunk(&e) => {
            // Race: another caller created the active row
            // between our SELECT and our INSERT. SELECT it.
            let existing: Option<(String,)> = sqlx::query_as(
                "SELECT id FROM downloads
                 WHERE media_id = ?1 AND COALESCE(room_id, '') = ?2 AND user_id = ?3
                   AND state IN ('pending','connecting','transferring','verifying','paused','complete','failed','cancelled')
                 ORDER BY started_at DESC LIMIT 1",
            )
            .bind(media_id)
            .bind(room_key)
            .bind(user_id)
            .fetch_optional(store.pool())
            .await
            .map_err(|e| AppError::other(format!("downloads SELECT post-race: {e}")))?;
            if let Some((id,)) = existing {
                Ok((id, false))
            } else {
                Err(AppError::other(format!(
                    "UNIQUE violated but no existing row found: {e}"
                )))
            }
        }
        Err(e) => Err(AppError::other(format!("create downloads row: {e}"))),
    }
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
    // P3-T15: use the manifest entry's `id` as the new
    // row's primary key so the viewer's local `media_id`
    // matches the host's manifest entry `id`. The
    // host-side sender dispatch looks up the source file
    // by `media_items.id` and matches it against the
    // inbound `Hello.media_id`, which the viewer derives
    // from `plan.media_id` (which is the row's id). If
    // the two sides used different ids, the host's
    // `manifest.media[i].iter().find(|m| m.id == hello.media_id)`
    // would never match. The host's id is itself a
    // `Uuid::new_v4()` minted at scan time and is
    // guaranteed to be unique; using it on the viewer
    // side preserves the round-trip identity.
    let new_id = entry.id.clone();
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
