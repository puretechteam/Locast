//! P3-T15 (the host-side sender wire-up): spawns a
//! [`SenderSession`](super::session::SenderSession) per
//! authenticated viewer request that arrives over the
//! `files` DataChannel.
//!
//! # Authorization model
//!
//! The dispatch only serves a `Hello` from a viewer if all
//! of the following are true:
//!
//! 1. The viewer is an authenticated room participant. The
//!    DataChannel is bound to a specific peer entry, and the
//!    room-state poll guarantees the participant list is
//!    server-authoritative; any DataChannel that survives the
//!    poll loop therefore has a valid remote identity.
//! 2. The requested `media_id` exists in the host's verified
//!    manifest for the current room. The manifest is signed
//!    by the host and verified against the trust anchor
//!    carried in the invite URL; the dispatch reads the
//!    in-memory verified cache, so a malicious peer cannot
//!    trick the host into serving a media row that was never
//!    published.
//! 3. The `Source.peer_id` on the manifest entry matches the
//!    host's own canonical peer-id (the host is the only
//!    legitimate source for its own library in v1).
//!
//! # File resolution
//!
//! The host resolves the on-disk file path from the
//! `media_items.relative_path` row keyed by `media_id`. The
//! row was written by `library::scan` (and is upserted
//! idempotently from the manifest by `download_open` on the
//! viewer side and by the manifest build on the host side),
//! so the path is the same content-addressed layout every
//! other part of the codebase uses:
//!
//! ```text
//! <library_root>/<media_items.relative_path>
//! ```
//!
//! The viewer never gets to choose the on-disk path. The
//! `Hello.media_id` is checked against the verified manifest
//! and the manifest's `sources[].peer_id` is checked against
//! the host's own `peer_id`. The host then looks up the row
//! locally and reads from the absolute path it gets back.
//!
//! # Lifecycle
//!
//! One [`HostSenderDispatcher`] per process. It is referenced
//! by the `WebRtcManager` and consulted every time a new
//! `files` DataChannel reaches the `Open` state. Each viewer
//! peer gets at most one in-flight `SenderSession`; a second
//! `files` DataChannel from the same peer to the same room
//! is rejected by the dispatch (the WebRTC layer has one DC
//! per peer, so this is the only way a duplicate could
//! appear). A peer whose prior sender has already finished
//! is allowed to start a new one.
//!
//! All spawned senders are children of the dispatcher's
//! master cancellation token. The token is parented to the
//! room's master token (the `WebRtcManager::cancel` set
//! by `on_room_left`), so:
//!
//! - peer disconnect -> DC close -> existing sender
//!   observes transport close and exits cleanly,
//! - room leave -> dispatcher token fires -> every sender
//!   exits,
//! - app shutdown -> same path as room leave,
//! - sender completes (transport closed by the viewer or
//!   the receiver reports `have.len() == total_chunks`) -> the
//!   per-sender task returns and the per-peer slot is freed
//!   for the next request.
//!
//! # Concurrency
//!
//! The dispatch holds one `Mutex<HashMap>` keyed by
//! `peer_user_id` that tracks live senders so a second
//! inbound `files` DC from the same peer is rejected. The
//! lock is only held across the bookkeeping update; the
//! long-running `SenderSession::run` runs in its own
//! `tokio::spawn`'d task and never holds the lock across
//! `await` points. A per-sender `CancellationToken` is
//! created for each spawn and parented to the dispatcher
//! master; cancelling it stops the session via the
//! transport's own `close()` path (the DataChannel close
//! cascades into `Transport::recv` returning `None`).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use sqlx::Row;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use uuid::Uuid;
use webrtc::data_channel::{DataChannel, RTCDataChannelState};

use locast_manifest::MediaManifest;

use super::plan::plan_download;
use super::session::{SenderSession, SessionError};
use super::transport::Transport;
use super::webrtc_transport::WebRtcTransport;
use crate::core::paths;
use crate::room::peer_id::derive_peer_id;
use crate::storage::Storage;

/// Per-sender bookkeeping. The cancel token is what the
/// dispatch fires to tear down a sender that is somehow
/// stuck (e.g. the viewer disappeared without a DC close,
/// or the per-sender task is wedged in a non-cancel-aware
/// `await`).
struct LiveSender {
    cancel: CancellationToken,
}

/// Inputs the dispatcher needs from the host's environment.
#[derive(Clone)]
pub struct HostDispatchContext {
    storage: Storage,
    library_root: PathBuf,
    /// Room client used to fetch the verified manifest for
    /// the room the inbound peer is in.
    room_client: Arc<crate::net::room::RoomClient>,
    /// Local 32-byte Ed25519 verifying key. Used to derive
    /// the host's canonical `peer_id` so the dispatch can
    /// match the manifest's `Source.peer_id` against itself.
    local_pubkey: [u8; 32],
    /// The room id the dispatcher is currently bound to.
    /// Set by the WebRtcManager when it knows the room;
    /// cleared on `on_room_left`.
    bound_room: Arc<Mutex<Option<Uuid>>>,
    /// Master cancellation token. Parented to the
    /// `WebRtcManager::cancel` token so room leave tears
    /// down every spawned sender.
    master_cancel: CancellationToken,
}

impl HostDispatchContext {
    /// Construct a dispatcher context. Caller wires the
    /// `master_cancel` to the WebRtcManager's room-level
    /// token (typically by cloning `WebRtcManager::cancel`).
    pub fn new(
        storage: Storage,
        library_root: PathBuf,
        room_client: Arc<crate::net::room::RoomClient>,
        local_pubkey: [u8; 32],
        master_cancel: CancellationToken,
    ) -> Self {
        Self {
            storage,
            library_root,
            room_client,
            local_pubkey,
            bound_room: Arc::new(Mutex::new(None)),
            master_cancel,
        }
    }

    /// Bind the dispatcher to a room id. Called by the
    /// `WebRtcManager` whenever it observes a new room. The
    /// binding is the only thing that decides which room's
    /// verified manifest the dispatcher will trust.
    pub async fn set_room(&self, room_id: Uuid) {
        *self.bound_room.lock().await = Some(room_id);
    }

    /// Clear the room binding. The master cancel token will
    /// already have fired, so this is mostly a hygiene step
    /// for any sender that survived a partial teardown.
    pub async fn clear_room(&self) {
        *self.bound_room.lock().await = None;
    }

    async fn current_room(&self) -> Option<Uuid> {
        *self.bound_room.lock().await
    }
}

/// One dispatcher per process. Consulted by the
/// `WebRtcManager` on every inbound `files` DataChannel that
/// reaches `Open`.
pub struct HostSenderDispatcher {
    ctx: HostDispatchContext,
    /// Per-peer live sender bookkeeping. The lock is only
    /// held across HashMap mutations; it is never held
    /// across `await` on the sender task.
    live: Mutex<HashMap<Uuid, LiveSender>>,
}

impl HostSenderDispatcher {
    pub fn new(ctx: HostDispatchContext) -> Arc<Self> {
        Arc::new(Self {
            ctx,
            live: Mutex::new(HashMap::new()),
        })
    }

    /// Context accessor for the WebRtcManager. Lets the
    /// manager reach the room-binding setters without
    /// re-wrapping the context.
    pub fn context(&self) -> &HostDispatchContext {
        &self.ctx
    }

    /// Spawn a `SenderSession` for the given authenticated
    /// peer. The DataChannel is the verified, authenticated
    /// `files` channel. The dispatch does NOT verify the
    /// inbound `Hello.peer_id` against a specific pubkey;
    /// the WebRTC layer's DTLS handshake is the
    /// connection-level authentication, and the room-state
    /// poll (in `WebRtcManager`) guarantees the DataChannel
    /// belongs to a current room member. Application-layer
    /// re-verification of `Hello.peer_id` against the
    /// verified manifest is intentionally NOT done at this
    /// layer: the manifest is bound to the host's identity
    /// (the host is the only legitimate source for its own
    /// library in v1) and the inbound `Hello.media_id` /
    /// `download_id` / `manifest_version` are checked
    /// against the bound plan instead.
    pub async fn spawn_for_dc(
        self: &Arc<Self>,
        dc: Arc<dyn DataChannel>,
        peer_user_id: Uuid,
    ) {
        // 1. Allocate the per-sender cancel token first so
        //    the open-wait loop can check it.
        let sender_cancel = self.ctx.master_cancel.child_token();

        // 2. Wait for the DC to reach the `Open` state. On
        //    the host (initiator) side, the DC is created
        //    locally as part of `add_peer` but the SCTP
        //    transport is only fully established once the
        //    remote answer has been processed and DTLS has
        //    completed. The manager hands the DC to us
        //    immediately after `create_data_channel`, so the
        //    first `ready_state` call typically returns
        //    `Connecting`. We loop on a short sleep until the
        //    DC is `Open` or the master token fires.
        let open_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match dc.ready_state().await {
                Ok(RTCDataChannelState::Open) => break,
                Ok(RTCDataChannelState::Closed) => {
                    warn!(
                        remote = %peer_user_id,
                        "host dispatch: DataChannel closed before reaching Open"
                    );
                    return;
                }
                Ok(other) => {
                    if std::time::Instant::now() >= open_deadline {
                        warn!(
                            remote = %peer_user_id,
                            state = ?other,
                            "host dispatch: DataChannel did not reach Open in 30s; dropping"
                        );
                        return;
                    }
                    if sender_cancel.is_cancelled() {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                Err(e) => {
                    warn!(
                        remote = %peer_user_id,
                        error = %e,
                        "host dispatch: DataChannel ready_state failed"
                    );
                    return;
                }
            }
        }

        // 2. Reject if there is already a live sender for
        //    this exact peer. The WebRTC layer only ever
        //    has one `files` DC per peer; a second request
        //    means a re-negotiation. We do not re-bind.
        {
            let g = self.live.lock().await;
            if g.contains_key(&peer_user_id) {
                warn!(
                    remote = %peer_user_id,
                    "host dispatch: a sender is already live for this peer; dropping duplicate"
                );
                return;
            }
        }

        // 3. Look up the verified manifest for the current
        //    room. If the room is not bound (we are not in a
        //    room yet, or already left) there is nothing to
        //    serve.
        let room_id = match self.ctx.current_room().await {
            Some(r) => r,
            None => {
                warn!(
                    remote = %peer_user_id,
                    "host dispatch: no room bound; dropping inbound files DC"
                );
                return;
            }
        };
        let manifest = match self.ctx.room_client.verified_manifest(room_id) {
            Some(m) => m,
            None => {
                warn!(
                    remote = %peer_user_id,
                    room_id = %room_id,
                    "host dispatch: no verified manifest for room; dropping"
                );
                return;
            }
        };

        // 4. Wrap the DC in a Transport. The cancellation
        //    token is per-sender; the transport's recv pump
        //    exits when the token fires OR the DC closes.
        let transport: Arc<dyn Transport> =
            Arc::new(WebRtcTransport::new(dc.clone(), sender_cancel.clone()));

        // 5. Reserve the per-peer slot. Hold the lock just
        //    long enough to insert; the spawned task does
        //    not touch this map.
        {
            let mut g = self.live.lock().await;
            // Re-check after re-acquiring the lock; another
            // spawn may have raced in.
            if g.contains_key(&peer_user_id) {
                warn!(
                    remote = %peer_user_id,
                    "host dispatch: race; a sender is already live for this peer; dropping"
                );
                sender_cancel.cancel();
                return;
            }
            g.insert(
                peer_user_id,
                LiveSender {
                    cancel: sender_cancel.clone(),
                },
            );
        }

        let dispatcher = Arc::clone(self);
        let storage = self.ctx.storage.clone();
        let library_root = self.ctx.library_root.clone();
        let host_peer_id = derive_peer_id(self.ctx.local_pubkey);
        let cancel = sender_cancel.clone();

        // 6. Spawn the long-running sender task. The task
        //    owns the per-sender transport; the dispatcher
        //    owns the bookkeeping. The `sanitized_filename`
        //    is derived inside the task from the verified
        //    manifest entry that matches the inbound
        //    `Hello.media_id`.
        tokio::spawn(async move {
            let res = run_sender(
                transport,
                manifest,
                storage,
                library_root,
                host_peer_id,
                cancel,
            )
            .await;
            if let Err(e) = res {
                warn!(
                    remote = %peer_user_id,
                    error = %e,
                    "host dispatch: sender exited with error"
                );
            } else {
                debug!(
                    remote = %peer_user_id,
                    "host dispatch: sender exited cleanly"
                );
            }
            // Free the per-peer slot so a follow-up
            // request from the same peer (after a clean
            // exit) is allowed.
            let mut g = dispatcher.live.lock().await;
            g.remove(&peer_user_id);
        });
    }

    /// Cancel every live sender. The master cancel token
    /// already cascades, so this is defense-in-depth (e.g. a
    /// future test that wants to assert no senders remain).
    pub async fn cancel_all(&self) {
        let g = self.live.lock().await;
        for v in g.values() {
            v.cancel.cancel();
        }
    }
}

/// Look up the host's on-disk path for `media_id` from the
/// `media_items` table. Returns the absolute path (joined
/// with the library root) or an error if the row is missing
/// or the path fails the content-addressed validator.
async fn resolve_host_source_path(
    storage: &Storage,
    library_root: &PathBuf,
    media_id: &str,
) -> Result<PathBuf, String> {
    // The row is keyed by `id` (the v4 UUID) and the
    // `relative_path` column is the library-root-relative
    // content-addressed path. Both the row and the path are
    // host-local data; the viewer never sees them.
    let row = sqlx::query("SELECT relative_path, sha256, filename FROM media_items WHERE id = ?")
        .bind(media_id)
        .fetch_optional(&storage.pool())
        .await
        .map_err(|e| format!("media_items lookup: {e}"))?;
    let Some(row) = row else {
        return Err(format!("media_id {media_id} not in local media_items"));
    };
    let rel_path: String = row
        .try_get("relative_path")
        .map_err(|e| format!("media_items relative_path: {e}"))?;
    let sha: String = row
        .try_get("sha256")
        .map_err(|e| format!("media_items sha256: {e}"))?;
    let filename: String = row
        .try_get("filename")
        .map_err(|e| format!("media_items filename: {e}"))?;
    // The relative_path the scanner wrote IS the
    // content-addressed layout, e.g.
    // `library/ed/bb/<sha>/smoke.bin`. We re-build the
    // expected content-addressed path from (sha, filename)
    // and assert it matches the stored `relative_path`.
    // `relative_path` is allowed to contain slashes
    // (it is a library-root-relative path, not a single
    // component); the path-containment check below is
    // the one that defends against escape.
    let expected = paths::content_addressed_path(library_root, &sha, &filename)
        .map_err(|e| format!("content_addressed_path: {e}"))?;
    let actual = library_root.join(&rel_path);
    // Symlink protection: require that the canonical form
    // of the file is contained in the canonical library
    // root. This is the same containment check
    // `library::fs::complete_download` uses for the
    // receiver side; the sender applies it to the source
    // file the host is about to read from.
    let canon_root = std::fs::canonicalize(library_root)
        .map_err(|e| format!("canonicalize library_root: {e}"))?;
    let canon_actual = match std::fs::canonicalize(&actual) {
        Ok(p) => p,
        Err(e) => {
            return Err(format!(
                "canonicalize source path {}: {e}",
                actual.display()
            ));
        }
    };
    if !canon_actual.starts_with(&canon_root) {
        return Err(format!(
            "source path {} escapes library root",
            actual.display()
        ));
    }
    // The expected path and the actual path are the same
    // file (both point to the same content-addressed leaf).
    // If they differ, the row's relative_path is stale.
    if expected != actual {
        return Err(format!(
            "relative_path {rel_path:?} does not match content-addressed path {}",
            expected.display()
        ));
    }
    Ok(canon_actual)
}

/// Inner loop of one spawned sender. The caller has already
/// verified the inbound `files` DataChannel, looked up the
/// verified manifest, and registered the per-peer slot.
///
/// The session is constructed lazily after the `Hello` is
/// read: we need the `download_id` and `media_id` from the
/// `Hello` to build the `DownloadPlan`, and the plan
/// requires `&mut self`-style binding. We do the read
/// inline here, then hand off to `SenderSession::run`.
async fn run_sender(
    transport: Arc<dyn Transport>,
    manifest: MediaManifest,
    storage: Storage,
    library_root: PathBuf,
    host_peer_id: String,
    cancel: CancellationToken,
) -> Result<(), SessionError> {
    use super::wire::codec;
    use super::wire::Frame;

    // 1. Read the first frame. We expect `Hello`. We use
    //    `Transport::recv` directly so we can extract the
    //    `download_id` and `media_id` before building the
    //    plan; the `SenderSession` itself does this
    //    internally too, but it requires the plan to be
    //    pre-built. Building the plan requires the `media_id`,
    //    so we read `Hello` first.
    let raw = match tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        r = transport.recv() => r,
    } {
        Ok(Some(b)) => b,
        Ok(None) => {
            debug!("host sender: transport closed before Hello");
            return Ok(());
        }
        Err(e) => {
            warn!(error = %e, "host sender: transport error before Hello");
            return Err(e.into());
        }
    };
    let (frame, _consumed) = codec::decode(&raw).map_err(SessionError::from)?;
    let Frame::Hello(hello) = frame else {
        warn!("host sender: first frame was not Hello");
        return Err(SessionError::Wire(super::wire::WireError::Malformed(
            "expected Hello".into(),
        )));
    };

    // 2. The `Hello.peer_id` is recorded on the trace but
    //    not re-verified here. The connection-level authn
    //    is DTLS (the WebRTC layer), and the application-
    //    level authn is the manifest binding check below:
    //    the `Hello.media_id` must match a row in the
    //    verified manifest, and that row's source must be
    //    the host. The viewer cannot forge the manifest
    //    because the manifest is signed by the host's
    //    Ed25519 key and verified against the trusted
    //    pubkey from the invite URL.
    debug!(
        viewer_peer_id = %hello.peer_id,
        media_id = %hello.media_id,
        download_id = %hello.download_id,
        "host sender: hello received"
    );

    // 3. Find the manifest entry. The viewer's
    //    `media_id` must match a row in the verified
    //    manifest, AND that row's primary `Source.peer_id`
    //    must be the host's own peer_id (the host is the
    //    only legitimate source for its own library in v1).
    let entry = manifest
        .media
        .iter()
        .find(|m| m.id == hello.media_id)
        .ok_or_else(|| {
            warn!(
                media_id = %hello.media_id,
                "host sender: media_id not in verified manifest"
            );
            SessionError::Wire(super::wire::WireError::Malformed(format!(
                "media_id {} not in manifest",
                hello.media_id
            )))
        })?;
    if entry.sources.is_empty() {
        return Err(SessionError::Wire(super::wire::WireError::Malformed(
            "manifest entry has no sources".into(),
        )));
    }
    // The host is the preferred source (priority 0). The
    // dispatch refuses to serve from any non-host source.
    let _host_source = entry
        .sources
        .iter()
        .find(|s| s.peer_id == host_peer_id)
        .ok_or_else(|| {
            warn!(
                media_id = %hello.media_id,
                host_peer_id = %host_peer_id,
                "host sender: manifest entry has no host source"
            );
            SessionError::Wire(super::wire::WireError::Malformed(
                "manifest entry has no host source".into(),
            ))
        })?;

    // 4. Resolve the on-disk file. This is host-local data
    //    (the `media_items` table); the viewer never gets
    //    to name a path.
    let source_path = match resolve_host_source_path(&storage, &library_root, &entry.id).await {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "host sender: could not resolve source path");
            return Err(SessionError::Storage(e));
        }
    };

    // 5. Build the plan. `plan_download` validates chunk
    //    size, chunk-hash count, and per-chunk SHA-256
    //    shape; a bad manifest row would have been rejected
    //    upstream at `build_manifest` time, so this is a
    //    belt-and-braces check.
    let plan = match plan_download(
        &hello.download_id,
        &entry.id,
        manifest.manifest_version as i64,
        entry,
        &host_peer_id,
    ) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = ?e, media_id = %entry.id, "host sender: plan_download failed");
            return Err(SessionError::Storage(format!("plan_download: {e}")));
        }
    };
    // The `plan.source.peer_id` must match the host. The
    // `plan_download` builder picks the source whose
    // `peer_id` matches the arg; we pass the host's
    // peer_id, so the source chosen is the host source.
    debug_assert_eq!(plan.source.peer_id, host_peer_id);
    debug_assert_eq!(plan.source.source.peer_id, host_peer_id);

    // 6. Hand off to `SenderSession::run_after_hello` for
    //    the rest of the transfer. The session sends
    //    `Offer` and then loops on `Request` / `Nak` /
    //    `Cancel` / `Error` / `Ack`. We pass the
    //    `sanitized_filename` from the manifest so the
    //    `Offer.frame` carries the real host-published
    //    filename.
    let sanitized_filename = entry.filename.clone();
    SenderSession::run_after_hello(
        &plan,
        transport,
        source_path,
        hello,
        sanitized_filename,
        cancel,
    )
    .await
}
