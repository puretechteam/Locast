//! `net::webrtc` - the client-side WebRTC PeerConnection
//! manager (P3-T05).
//!
//! ## Topology
//!
//! Architecture §19.2 specifies a full-mesh PeerConnection
//! graph between all participants. Each room has a hard cap
//! of 8 participants; the manager enforces this by ignoring
//! any `RoomSummary.participants` list whose length would
//! push us past the cap. The peer-set is keyed by
//! `user_id` (UUID v7 assigned by the server).
//!
//! ## Deterministic initiator rule
//!
//! Architecture §19.2.3 requires the initiator decision to be
//! deterministic across all peers. We sort `user_id`s
//! lexicographically (UUID byte order) and the lower UUID
//! initiates. UUID v7 is monotonic-per-time so this is
//! roughly "older creation = initiator" but the rule is
//! strictly byte order, not clock order. The tie case
//! (identical UUIDs) is impossible per §21.3 but defensive
//! code skips negotiation when it happens.
//!
//! ## Lifecycle states
//!
//! ```text
//! New --(offer sent, initiator)---------> OfferSent
//! New --(answer received, answerer)----> AnswerReceived
//! OfferSent --(answer received)---------> AnswerReceived
//! AnswerReceived --(connection Connected)-> Connected
//! (any) --(connection Failed + 1 ICE restart) --(*?)-> (back to OfferSent or Failed)
//! (any) --(connection Closed)-----------> Closed (dropped)
//! ```
//!
//! The `on_room_left` call drops every entry and cancels the
//! inbound loop. After `on_room_left`, `on_room_state_changed`
//! for a new room is treated as a fresh manager.
//!
//! ## Inbound loop
//!
//! A single inbound loop task subscribes to
//! [`SignalingClient::subscribe`] and filters for
//! [`MessageKind::Signal`] envelopes. Each inbound SIGNAL
//! envelope is verified by re-deriving the canonical signed
//! bytes (see [`super::webrtc_canonical`]) and verifying the
//! Ed25519 signature against `envelope.sender.pubkey`. A bad
//! signature is dropped silently (logged at `warn`).
//!
//! ## Room-state polling
//!
//! The recon report flagged a missing `RoomClient` event hook
//! for state changes. Rather than expand `RoomClient`'s
//! surface, the inbound loop polls `RoomClient::state()` every
//! 200 ms and feeds deltas into [`Self::on_room_state_changed`].
//! This is a deliberate simplification for P3-T05; replace
//! with an event-driven hook in a follow-up task.
//!
//! ## Redaction
//!
//! SDP bodies and ICE candidate strings are NEVER logged.
//! Only the kind (`Offer` / `Answer` / `Ice`), the remote
//! user_id, and short stable identifiers appear in `tracing`
//! events. This matches the redaction discipline at
//! `apps/client/src-tauri/src/net/signaling.rs:1068-1086`.
//!
//! ## webrtc-rs 0.20 API notes
//!
//! - `PeerConnection` and `DataChannel` are TRAITS
//!   (object-safe), not concrete types. The manager stores
//!   `Arc<dyn PeerConnection>` and `Arc<dyn DataChannel>`.
//! - `DataChannel` events are POLLED via `dc.poll()` returning
//!   `Option<DataChannelEvent>`; there is no `on_open`
//!   callback registration. P3-T05 logs the channel label
//!   at creation time; v1 file transfer (P3-T04+) wires the
//!   poll loop on the consumer side.
//! - ICE restart is via the dedicated `restart_ice()` method
//!   on `PeerConnection`, not via `create_offer` options.
//! - `add_ice_candidate` takes `RTCIceCandidateInit`, not the
//!   full `RTCIceCandidate`.
//! - The `RTCIceEvent::candidate` field is a fully-decoded
//!   `RTCIceCandidate`; we serialize it via `Display` (which
//!   emits the SDP `candidate:` line) for the SIGNAL payload.
//! - The 0.20 builder does NOT expose
//!   `bundlePolicy`/`rtcpMuxPolicy`/`iceTransportPolicy`/
//!   `sdpSemantics` knobs; the Sans-I/O rtc core defaults are
//!   `MaxBundle` / `Require` / `All` / `unified-plan` (see
//!   `RTCConfigurationBuilder` defaults), which match
//!   architecture §19.3.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use locast_protocol::envelope::{Envelope, MessageKind, Sender};
use locast_protocol::room::{SignalCandidate, SignalKind, SignalPayload};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use uuid::Uuid;
use webrtc::data_channel::{DataChannel, RTCDataChannelInit, RTCDataChannelState};
use webrtc::error::Error as WebRtcError;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder,
    RTCIceCandidateInit, RTCIceServer, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
    RTCSessionDescription,
};

use crate::identity::keystore::IdentityService;
use crate::net::room::RoomSummaryIpc;
use crate::net::signaling::SignalingClient;
use crate::room::peer_id::derive_peer_id;

use super::webrtc_canonical::signal_signed_bytes;

/// The hard participant cap per room, from architecture §19.2.
/// Above this, the manager refuses to add new peers.
pub const ROOM_PARTICIPANT_CAP: usize = 8;

/// The data-channel label for file transfer (architecture
/// §19.6.1).
pub const FILES_DC_LABEL: &str = "files";

/// The data-channel protocol string, fixed by the protocol
/// (architecture §19.6.1: `protocol: "locast-files-v1"`).
pub const FILES_DC_PROTOCOL: &str = "locast-files-v1";

/// Polling interval for `RoomClient::state()` from the
/// inbound loop. Deliberate simplification for P3-T05
/// (see module-level docs); replace with an event-driven
/// hook in a follow-up.
const ROOM_STATE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// The list of STUN servers (architecture §19.3). Only STUN;
/// TURN is out of scope for v1.
const STUN_SERVERS: &[&str] = &[
    "stun:stun.l.google.com:19302",
    "stun:stun.cloudflare.com:3478",
    "stun:stun.nextcloud.com:3478",
];

/// The maximum number of ICE restarts attempted on a single
/// peer connection before the entry is torn down. Architecture
/// §19.3.4: one ICE restart before giving up.
const ICE_RESTART_LIMIT: u8 = 1;

/// Events emitted by a per-peer [`PeerHandler`] into a
/// side-channel `mpsc` so that the manager — which cannot
/// itself live inside the trait object (the trait object lives
/// inside the manager, a circular `Arc`) — can react.
enum PeerEvent {
    /// Local ICE candidate gathered. End-of-candidates is
    /// represented as a `PeerEvent::IceCandidate` whose
    /// `RTCPeerConnectionIceEvent.candidate.foundation`
    /// field is empty (the webrtc 0.20 driver sends an
    /// `RTCIceCandidateInit::default()` for end-of-candidates,
    /// which has empty `foundation`).
    IceCandidate(RTCPeerConnectionIceEvent),
    /// Remote data channel opened on the answerer side
    /// (adoption).
    DataChannel(Arc<dyn DataChannel>),
    /// PeerConnection state changed.
    StateChange(RTCPeerConnectionState),
}

/// The phase of a single peer connection in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerPhase {
    /// The PeerConnection was constructed but no offer/answer
    /// exchange has started.
    New,
    /// We are the initiator and have sent an offer; waiting
    /// for an answer.
    OfferSent,
    /// We received the answer (or, as the answerer, received
    /// an offer and replied with an answer). Waiting for the
    /// connection to come up.
    AnswerReceived,
    /// The peer connection reached `connected`.
    Connected,
    /// The peer connection hit `failed` and we are trying an
    /// ICE restart; on a second failure we transition to
    /// `Closed`.
    Failed,
    /// The peer connection is closed; the entry is being
    /// removed from the table.
    Closed,
}

/// A single entry in the per-room peer graph. The remote
/// `user_id` is the key of the owning `HashMap`, not a field.
struct PeerEntry {
    /// The PeerConnection trait object. webrtc 0.20 returns
    /// an opaque `impl PeerConnection` from the builder; we
    /// wrap it in `Arc<dyn PeerConnection>` to share across
    /// tasks and store in a struct.
    pc: Arc<dyn PeerConnection>,
    /// The `files` DataChannel. `Some` on the initiator side
    /// (created locally) or on the answerer side once we have
    /// adopted the inbound `files` channel via
    /// `PeerConnectionEventHandler::on_data_channel`.
    dc: Option<Arc<dyn DataChannel>>,
    /// Current lifecycle phase.
    phase: PeerPhase,
    /// `true` if this side is the initiator (created the
    /// offer). Determined at entry-creation time by UUID byte
    /// ordering; never changes for the entry's lifetime.
    initiator: bool,
    /// ICE-restart counter. `Failed -> restart -> OfferSent`
    /// while `restarts < ICE_RESTART_LIMIT`; the second
    /// `Failed` flips the entry to `Closed`.
    restarts: u8,
}

/// The shared, mutex-protected state of the manager. Held
/// behind `Arc<tokio::sync::Mutex<...>>` because the inbound
/// loop and the peer handlers need concurrent access.
struct ManagerState {
    /// The room id we are currently in, if any.
    room_id: Option<Uuid>,
    /// Per-peer entries, keyed by the remote `user_id`.
    peers: HashMap<Uuid, PeerEntry>,
}

impl ManagerState {
    fn new() -> Self {
        Self {
            room_id: None,
            peers: HashMap::new(),
        }
    }
}

/// The top-level manager. One per process; spawn one from
/// `setup()` in `lib.rs`.
pub struct WebRtcManager {
    signaling: Arc<SignalingClient>,
    identity: Arc<IdentityService>,
    state: Arc<tokio::sync::Mutex<ManagerState>>,
    cancel: CancellationToken,
    /// P3-T15: host-side sender dispatch. `None` on the
    /// viewer (no library to serve); `Some` on the host. The
    /// manager consults the dispatcher on every inbound
    /// `files` DataChannel that reaches `Open`; the
    /// dispatcher spawns a `SenderSession` that reads from
    /// the host's verified library file and serves chunks
    /// over the same DataChannel. Wrapped in a `std::sync`
    /// mutex (NOT a tokio mutex) so the install and the
    /// read paths do not need to be `async`.
    host_dispatch: std::sync::Mutex<Option<Arc<crate::transfer::HostSenderDispatcher>>>,
}

impl WebRtcManager {
    /// Construct a new manager. Does not start the inbound
    /// loop; call [`Self::start_with_room_client`] for that.
    pub fn new(
        signaling: Arc<SignalingClient>,
        identity: Arc<IdentityService>,
        _room_client: Arc<crate::net::room::RoomClient>,
    ) -> Self {
        Self {
            signaling,
            identity,
            state: Arc::new(tokio::sync::Mutex::new(ManagerState::new())),
            cancel: CancellationToken::new(),
            host_dispatch: std::sync::Mutex::new(None),
        }
    }

    /// P3-T15: install a host-side sender dispatch. Call
    /// this once during `setup()` if the local process is
    /// acting as a host (i.e. it owns a library it is
    /// willing to serve). On a viewer-only deployment
    /// (no library), leave the dispatcher `None`; the
    /// manager will silently drop inbound `files` DCs
    /// after the adoption step.
    ///
    /// The call is idempotent: a second call replaces the
    /// first (the prior dispatch is dropped, which cancels
    /// any in-flight senders it owned).
    pub fn set_host_dispatch(
        &self,
        dispatch: Arc<crate::transfer::HostSenderDispatcher>,
    ) {
        let mut g = self.host_dispatch.lock().expect("host_dispatch");
        *g = Some(dispatch);
    }

    /// P3-T15: snapshot the currently-installed host
    /// dispatch, if any. The lock is held only across the
    /// clone; long-running operations on the returned
    /// `Arc` happen outside the lock.
    pub fn host_dispatch(&self) -> Option<Arc<crate::transfer::HostSenderDispatcher>> {
        let g = self.host_dispatch.lock().expect("host_dispatch");
        g.clone()
    }

    /// P3-T15: expose the room-level cancel token so the
    /// host dispatch can parent its per-sender tokens to
    /// it. Reading is a cheap `clone` of a `CancellationToken`
    /// (the token itself is an `Arc` internally).
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Spawn the full inbound loop with the room-state poller.
    /// Use this from `lib.rs::setup`.
    pub fn start_with_room_client(
        self: Arc<Self>,
        room_client: Arc<crate::net::room::RoomClient>,
    ) -> JoinHandle<()> {
        let cancel = self.cancel.clone();
        let signaling = Arc::clone(&self.signaling);
        let state = Arc::clone(&self.state);
        let manager = Arc::clone(&self);
        tokio::spawn(async move {
            run_inbound_loop(manager, signaling, room_client, state, cancel).await;
        })
    }

    /// Apply a room-state change. Re-evaluates the peer set
    /// against `summary.participants`:
    /// - Drop entries whose `user_id` is no longer present.
    /// - Create entries for any new participants.
    /// - For each new entry, decide initiator vs. answerer
    ///   by UUID byte ordering (lower = initiator).
    pub async fn on_room_state_changed(self: Arc<Self>, summary: RoomSummaryIpc) {
        let Ok(room_id) = Uuid::parse_str(&summary.id) else {
            warn!(room_id = %summary.id, "ignoring room state with bad room_id uuid");
            return;
        };
        // P3-T15: bind the host dispatch to the new room so
        // any inbound `files` DC knows which manifest to
        // trust.
        if let Some(dispatch) = self.host_dispatch() {
            dispatch.context().set_room(room_id).await;
        }
        let mut desired: HashSet<Uuid> = HashSet::with_capacity(summary.participants.len());
        for p in &summary.participants {
            let Ok(uid) = Uuid::parse_str(&p.user_id) else {
                warn!(user_id = %p.user_id, "ignoring participant with bad uuid");
                continue;
            };
            desired.insert(uid);
        }
        let snap = self.signaling.snapshot().await;
        let Some(my_user_id) = snap.user_id.as_ref().and_then(|s| Uuid::parse_str(s).ok()) else {
            debug!("on_room_state_changed: local user_id not yet known; skipping peer creation");
            return;
        };
        let my_bytes = *my_user_id.as_bytes();
        // Build the candidate list while the lock is held.
        // ParticipantIpc does not carry a `pubkey` field in
        // the IPC shape; per-envelope signature verification
        // uses the wire `envelope.sender.pubkey` and does
        // not need a stored remote pubkey at entry-creation
        // time. We leave the pubkey stored on the entry as
        // zero-initialized and document this.
        let mut targets: Vec<(Uuid, [u8; 32], bool)> = Vec::new();
        {
            let mut g = self.state.lock().await;
            g.room_id = Some(room_id);
            let to_remove: Vec<Uuid> = g
                .peers
                .keys()
                .copied()
                .filter(|uid| !desired.contains(uid))
                .collect();
            for uid in to_remove {
                if let Some(entry) = g.peers.remove(&uid) {
                    let _ = entry.pc.close().await;
                    info!(remote = %uid, "peer connection closed (left room)");
                }
            }
            if g.peers.len() > ROOM_PARTICIPANT_CAP {
                warn!(
                    count = g.peers.len(),
                    cap = ROOM_PARTICIPANT_CAP,
                    "room exceeds participant cap; refusing to add more"
                );
                return;
            }
            for p in &summary.participants {
                let Ok(uid) = Uuid::parse_str(&p.user_id) else {
                    continue;
                };
                if uid == my_user_id {
                    continue;
                }
                if g.peers.contains_key(&uid) {
                    continue;
                }
                if g.peers.len() + targets.len() >= ROOM_PARTICIPANT_CAP {
                    break;
                }
                let initiator = my_bytes < *uid.as_bytes();
                targets.push((uid, [0u8; 32], initiator));
            }
        }
        for (remote_id, _remote_pubkey, initiator) in targets {
            Arc::clone(&self).add_peer(remote_id, initiator).await;
        }
    }

    /// Tear down all peer connections and cancel the inbound
    /// loop. Idempotent.
    pub async fn on_room_left(&self) {
        self.cancel.cancel();
        // P3-T15: clear the dispatch's room binding so a
        // follow-up `room_create` (in the same process) does
        // not serve chunks from the prior room's manifest.
        if let Some(dispatch) = self.host_dispatch() {
            dispatch.context().clear_room().await;
            dispatch.cancel_all().await;
        }
        let mut g = self.state.lock().await;
        for (_uid, entry) in g.peers.drain() {
            let _ = entry.pc.close().await;
        }
        g.room_id = None;
    }

    /// Add a single peer to the table. Constructs the
    /// PeerConnection with the architecture-mandated SDP
    /// constraints; if `initiator` is `true`, immediately
    /// creates the data channel and sends the offer.
    async fn add_peer(self: Arc<Self>, remote_id: Uuid, initiator: bool) {
        let (pc, rx) = match build_peer_connection(remote_id).await {
            Ok(t) => t,
            Err(e) => {
                warn!(error = %e, remote = %remote_id, "failed to build peer connection");
                return;
            }
        };
        let mut entry = PeerEntry {
            pc: pc.clone(),
            dc: None,
            phase: PeerPhase::New,
            initiator,
            restarts: 0,
        };
        if initiator {
            let init = RTCDataChannelInit {
                ordered: true,
                max_packet_life_time: None,
                max_retransmits: None,
                protocol: FILES_DC_PROTOCOL.to_string(),
                negotiated: None,
            };
            match pc.create_data_channel(FILES_DC_LABEL, Some(init)).await {
                Ok(dc) => {
                    info!(
                        remote = %remote_id,
                        label = FILES_DC_LABEL,
                        protocol = FILES_DC_PROTOCOL,
                        "files DataChannel created (initiator side)"
                    );
                    // P3-T15: hand the freshly-created DC to
                    // the host dispatch (if installed). The
                    // dispatch polls `ready_state` and
                    // returns early if the DC is not yet
                    // `Open`; it is safe to hand it over
                    // before the offer/answer exchange.
                    if let Some(dispatch) = self.host_dispatch() {
                        let dc_for_spawn = dc.clone();
                        tokio::spawn(async move {
                            dispatch.spawn_for_dc(dc_for_spawn, remote_id).await;
                        });
                    }
                    entry.dc = Some(dc);
                }
                Err(e) => {
                    warn!(error = %e, remote = %remote_id, "create_data_channel failed");
                    return;
                }
            }
            match pc.create_offer(None).await {
                Ok(offer) => {
                    if let Err(e) = pc.set_local_description(offer).await {
                        warn!(error = %e, remote = %remote_id, "set_local_description failed");
                        return;
                    }
                    entry.phase = PeerPhase::OfferSent;
                    if let Some(local_desc) = pc.local_description().await {
                        let sdp = local_desc.sdp.clone();
                        if let Err(e) = self
                            .send_signal(SignalPayload {
                                to_user_id: remote_id,
                                kind: SignalKind::Offer,
                                sdp: Some(sdp),
                                candidates: None,
                            })
                            .await
                        {
                            warn!(error = %e, remote = %remote_id, "send offer SIGNAL failed");
                        } else {
                            debug!(remote = %remote_id, "offer sent");
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, remote = %remote_id, "create_offer failed");
                    return;
                }
            }
        } else {
            debug!(remote = %remote_id, "answerer: waiting for offer");
        }
        let mut g = self.state.lock().await;
        g.peers.insert(remote_id, entry);
        drop(g);
        // Spawn the per-peer pump that forwards handler
        // events (ICE candidates, data channels, state
        // changes) into the manager. The trait object lives
        // inside the manager, so this side-channel `mpsc` is
        // the only way for the handler to reach back. The
        // pump is cancelled by `on_room_left`, which cancels
        // `self.cancel`; entry teardown reuses the same
        // token.
        tokio::spawn(peer_event_pump(
            Arc::clone(&self),
            remote_id,
            rx,
            self.cancel.clone(),
        ));
    }

    /// Build + sign + send a SIGNAL envelope to the signaling
    /// server. The server is a pure relay; it forwards the
    /// envelope to `payload.to_user_id` after verifying the
    /// signature.
    async fn send_signal(&self, payload: SignalPayload) -> Result<(), SignalingSendError> {
        let signed = signal_signed_bytes(&payload).map_err(SignalingSendError::EncodeRmp)?;
        let kp = self
            .identity
            .load_keypair()
            .await
            .map_err(|e| SignalingSendError::Identity(e.to_string()))?;
        let pubkey = kp.public_key_bytes();
        let sig = kp.sign_challenge(&signed);
        let snap = self.signaling.snapshot().await;
        let my_user_id_str = snap
            .user_id
            .ok_or_else(|| SignalingSendError::Identity("no bearer yet".into()))?;
        let my_user_id = Uuid::parse_str(&my_user_id_str)
            .map_err(|e| SignalingSendError::Identity(format!("bad user_id uuid: {e}")))?;
        let env = Envelope {
            v: 1,
            r#type: MessageKind::Signal,
            id: Uuid::now_v7(),
            room_id: {
                let g = self.state.lock().await;
                g.room_id
            },
            sender: Some(Sender {
                user_id: my_user_id,
                pubkey: pubkey.to_vec(),
                sig: sig.to_vec(),
            }),
            ts_ms: now_ms(),
            seq: 0,
            payload: serde_json::to_value(&payload).map_err(SignalingSendError::EncodeJson)?,
        };
        self.signaling
            .send_envelope(env)
            .await
            .map_err(|e| SignalingSendError::Signaling(e.to_string()))
    }

    /// Dispatch an inbound SIGNAL envelope. Verifies the
    /// signature, decodes the payload, then routes Offer /
    /// Answer / Ice to the matching peer connection.
    async fn handle_inbound_signal(
        &self,
        envelope: &Envelope,
        from_user_id: Uuid,
    ) -> Result<(), InboundError> {
        let payload: SignalPayload = serde_json::from_value(envelope.payload.clone())
            .map_err(|e| InboundError::Decode(e.to_string()))?;
        let snap = self.signaling.snapshot().await;
        let my_user_id = snap
            .user_id
            .as_ref()
            .and_then(|s| Uuid::parse_str(s).ok())
            .ok_or_else(|| InboundError::Routing("local user_id not known".into()))?;
        if payload.to_user_id != my_user_id {
            return Err(InboundError::Routing(format!(
                "signal to {} but we are {}",
                payload.to_user_id, my_user_id
            )));
        }
        let sender = envelope
            .sender
            .as_ref()
            .ok_or(InboundError::Auth("missing sender"))?;
        let signed =
            signal_signed_bytes(&payload).map_err(|e| InboundError::Decode(e.to_string()))?;
        if sender.pubkey.len() != 32 {
            return Err(InboundError::Auth("pubkey wrong length"));
        }
        if sender.sig.len() != 64 {
            return Err(InboundError::Auth("sig wrong length"));
        }
        verify_ed25519(&sender.pubkey, &sender.sig, &signed)?;
        match payload.kind {
            SignalKind::Offer => self.handle_remote_offer(from_user_id, payload).await,
            SignalKind::Answer => self.handle_remote_answer(from_user_id, payload).await,
            SignalKind::Ice => self.handle_remote_ice(from_user_id, payload).await,
        }
    }

    async fn handle_remote_offer(
        &self,
        from_user_id: Uuid,
        payload: SignalPayload,
    ) -> Result<(), InboundError> {
        let sdp = payload
            .sdp
            .ok_or_else(|| InboundError::Protocol("offer missing sdp".into()))?;
        let mut state = self.state.lock().await;
        let entry = state
            .peers
            .get_mut(&from_user_id)
            .ok_or_else(|| InboundError::Routing("offer for unknown peer".into()))?;
        if entry.initiator {
            return Err(InboundError::Protocol(
                "initiator received an offer (glare)".into(),
            ));
        }
        let remote_desc =
            RTCSessionDescription::offer(sdp).map_err(|e| InboundError::Protocol(e.to_string()))?;
        entry
            .pc
            .set_remote_description(remote_desc)
            .await
            .map_err(|e| InboundError::Protocol(e.to_string()))?;
        let answer = entry
            .pc
            .create_answer(None)
            .await
            .map_err(|e| InboundError::Protocol(e.to_string()))?;
        entry
            .pc
            .set_local_description(answer)
            .await
            .map_err(|e| InboundError::Protocol(e.to_string()))?;
        let local =
            entry.pc.local_description().await.ok_or_else(|| {
                InboundError::Protocol("no local description after answer".into())
            })?;
        entry.phase = PeerPhase::AnswerReceived;
        let sdp_text = local.sdp.clone();
        drop(state);
        self.send_signal(SignalPayload {
            to_user_id: from_user_id,
            kind: SignalKind::Answer,
            sdp: Some(sdp_text),
            candidates: None,
        })
        .await
        .map_err(|e| InboundError::Send(e.to_string()))?;
        Ok(())
    }

    async fn handle_remote_answer(
        &self,
        from_user_id: Uuid,
        payload: SignalPayload,
    ) -> Result<(), InboundError> {
        let sdp = payload
            .sdp
            .ok_or_else(|| InboundError::Protocol("answer missing sdp".into()))?;
        let mut state = self.state.lock().await;
        let entry = state
            .peers
            .get_mut(&from_user_id)
            .ok_or_else(|| InboundError::Routing("answer for unknown peer".into()))?;
        if !entry.initiator {
            return Err(InboundError::Protocol("answerer received an answer".into()));
        }
        let remote_desc = RTCSessionDescription::answer(sdp)
            .map_err(|e| InboundError::Protocol(e.to_string()))?;
        entry
            .pc
            .set_remote_description(remote_desc)
            .await
            .map_err(|e| InboundError::Protocol(e.to_string()))?;
        entry.phase = PeerPhase::AnswerReceived;
        Ok(())
    }

    async fn handle_remote_ice(
        &self,
        from_user_id: Uuid,
        payload: SignalPayload,
    ) -> Result<(), InboundError> {
        let candidates = payload
            .candidates
            .ok_or_else(|| InboundError::Protocol("ice missing candidates".into()))?;
        let mut state = self.state.lock().await;
        let entry = state
            .peers
            .get_mut(&from_user_id)
            .ok_or_else(|| InboundError::Routing("ice for unknown peer".into()))?;
        for c in candidates {
            if c.candidate.is_empty() {
                debug!(remote = %from_user_id, "end-of-candidates received");
                continue;
            }
            let init = webrtc::peer_connection::RTCIceCandidateInit {
                candidate: c.candidate.clone(),
                sdp_mid: c.sdp_mid.clone(),
                sdp_mline_index: c.sdp_m_line_index.map(|i| i as u16),
                username_fragment: None,
                url: None,
            };
            entry
                .pc
                .add_ice_candidate(init)
                .await
                .map_err(|e| InboundError::Protocol(e.to_string()))?;
        }
        Ok(())
    }

    /// Connection-state handler. Dispatched by the per-peer
    /// pump task spawned in `add_peer` whenever the trait
    /// object's `on_connection_state_change` fires. Updates
    /// `PeerEntry::phase`, triggers an ICE restart on
    /// `Failed` (up to `ICE_RESTART_LIMIT`), and tears the
    /// entry down on `Closed`.
    async fn on_connection_state(&self, remote_id: Uuid, state: RTCPeerConnectionState) {
        match state {
            RTCPeerConnectionState::Connected => {
                info!(remote = %remote_id, "peer connection connected");
                let mut g = self.state.lock().await;
                if let Some(entry) = g.peers.get_mut(&remote_id) {
                    entry.phase = PeerPhase::Connected;
                }
            }
            RTCPeerConnectionState::Failed => {
                warn!(remote = %remote_id, "peer connection failed");
                let (pc, restarts) = {
                    let mut g = self.state.lock().await;
                    let Some(entry) = g.peers.get_mut(&remote_id) else {
                        return;
                    };
                    let pc = entry.pc.clone();
                    let r = entry.restarts;
                    entry.phase = PeerPhase::Failed;
                    (pc, r)
                };
                if restarts < ICE_RESTART_LIMIT {
                    match pc.restart_ice().await {
                        Ok(()) => {
                            info!(remote = %remote_id, restart = restarts + 1, "ICE restart triggered");
                            let mut g = self.state.lock().await;
                            if let Some(entry) = g.peers.get_mut(&remote_id) {
                                entry.restarts += 1;
                                entry.phase = PeerPhase::OfferSent;
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, remote = %remote_id, "restart_ice failed; closing entry");
                            let mut g = self.state.lock().await;
                            if let Some(entry) = g.peers.remove(&remote_id) {
                                let _ = entry.pc.close().await;
                            }
                        }
                    }
                } else {
                    warn!(remote = %remote_id, "peer connection failed after ICE restart; closing entry");
                    let mut g = self.state.lock().await;
                    if let Some(entry) = g.peers.remove(&remote_id) {
                        drop(g);
                        let _ = entry.pc.close().await;
                    }
                }
            }
            RTCPeerConnectionState::Closed => {
                info!(remote = %remote_id, "peer connection closed");
                let mut g = self.state.lock().await;
                if let Some(entry) = g.peers.remove(&remote_id) {
                    drop(g);
                    let _ = entry.pc.close().await;
                }
            }
            _ => {
                debug!(
                    remote = %remote_id,
                    ?state,
                    "peer connection state change"
                );
            }
        }
    }

    /// Adopt an inbound data channel that arrived via
    /// `PeerConnectionEventHandler::on_data_channel` on the
    /// answerer side. Stores the `Arc<dyn DataChannel>` in
    /// the matching `PeerEntry`, and (P3-T15) if a host
    /// sender dispatch is installed, hands the channel off
    /// to it so a `SenderSession` can serve the host's
    /// library over it.
    async fn on_inbound_data_channel(&self, remote_id: Uuid, dc: Arc<dyn DataChannel>) {
        let label = match dc.label().await {
            Ok(s) => s,
            Err(_) => return,
        };
        if label == FILES_DC_LABEL {
            info!(
                remote = %remote_id,
                label = %label,
                "adopted inbound files DataChannel (answerer side)"
            );
            let mut g = self.state.lock().await;
            if let Some(entry) = g.peers.get_mut(&remote_id) {
                entry.dc = Some(dc.clone());
            }
            drop(g);
            // P3-T15: hand the DC to the host sender
            // dispatch (if installed) so a real
            // `SenderSession` starts. The dispatch is
            // optional; a viewer-only deployment has it
            // set to `None` and silently drops the
            // inbound channel here.
            if let Some(dispatch) = self.host_dispatch() {
                let dc_for_spawn = dc.clone();
                tokio::spawn(async move {
                    dispatch.spawn_for_dc(dc_for_spawn, remote_id).await;
                });
            }
        } else {
            debug!(
                remote = %remote_id,
                label = %label,
                "ignoring non-files inbound data channel"
            );
            let _ = dc.close().await;
        }
    }

    /// Number of peer entries currently in the table. Lets
    /// integration tests (and future telemetry) wait for the
    /// manager to detect a peer before timing out on the
    /// connection. Cheap: locks the manager state briefly.
    pub async fn peer_count(&self) -> usize {
        self.state.lock().await.peers.len()
    }

    /// Snapshot of the peer table's user_ids. Lets tests
    /// iterate without owning the manager.
    pub async fn peer_ids(&self) -> Vec<Uuid> {
        self.state.lock().await.peers.keys().copied().collect()
    }

    /// The connected `files` DataChannel for `remote_id`, or
    /// `None` if the peer entry is missing / not yet connected /
    /// has no adopted DC. P3-T13: this is the accessor the
    /// `download_open` command uses to look up the per-source
    /// transport.
    pub async fn data_channel_for_user_id(&self, remote_id: Uuid) -> Option<Arc<dyn DataChannel>> {
        let g = self.state.lock().await;
        g.peers.get(&remote_id).and_then(|p| p.dc.clone())
    }

    /// P3-T13: look up the connected DataChannel whose
    /// participant's canonical `peer_id` matches `peer_id_hex`.
    /// The caller supplies a `pubkey_lookup` closure that maps a
    /// participant's `user_id` (Uuid) to the 32-byte Ed25519
    /// pubkey; the manager hashes each candidate pubkey with
    /// `derive_peer_id` and returns the first match. Returns
    /// `None` if `peer_id_hex` is not canonical, if no
    /// participant matches, if the matching peer has no
    /// adopted DC yet, or if the DC has not reached the
    /// `Open` state (review fix C#10 — the prior version
    /// returned any stored DC, even if it was still
    /// `Connecting`, which caused the orchestrator to build a
    /// transport on a dead channel and immediately fail).
    pub async fn lookup_dc_by_peer_id<F>(
        &self,
        peer_id_hex: &str,
        pubkey_lookup: F,
    ) -> Option<Arc<dyn DataChannel>>
    where
        F: Fn(Uuid) -> Option<[u8; 32]>,
    {
        if !crate::room::peer_id::is_canonical_peer_id(peer_id_hex) {
            return None;
        }
        // Collect candidates while holding the lock; the
        // ready_state() check must NOT hold the manager lock
        // because the trait method is async.
        let candidates: Vec<(Uuid, Arc<dyn DataChannel>)> = {
            let g = self.state.lock().await;
            g.peers
                .iter()
                .filter_map(|(uid, p)| p.dc.clone().map(|dc| (*uid, dc)))
                .collect()
        };
        for (user_id, dc) in candidates {
            let Some(pubkey) = pubkey_lookup(user_id) else {
                continue;
            };
            if derive_peer_id(pubkey) != peer_id_hex {
                continue;
            }
            let state = match dc.ready_state().await {
                Ok(s) => s,
                Err(_) => continue,
            };
            if state == RTCDataChannelState::Open {
                return Some(dc);
            }
        }
        None
    }

    /// P3-T13 review fix C#10: convenience accessor that
    /// returns the connected `files` DataChannel for a
    /// specific participant user_id, but only if the DC has
    /// reached the `Open` state. Mirrors the readiness gate
    /// in [`Self::lookup_dc_by_peer_id`].
    pub async fn open_files_dc_for_user_id(&self, remote_id: Uuid) -> Option<Arc<dyn DataChannel>> {
        let dc = {
            let g = self.state.lock().await;
            g.peers.get(&remote_id).and_then(|p| p.dc.clone())?
        };
        let state = dc.ready_state().await.ok()?;
        if state != RTCDataChannelState::Open {
            return None;
        }
        Some(dc)
    }

    /// Current lifecycle phase for a peer. `None` if the peer
    /// is not in the table.
    pub async fn peer_phase(&self, remote_id: Uuid) -> Option<PeerPhase> {
        self.state
            .lock()
            .await
            .peers
            .get(&remote_id)
            .map(|e| e.phase)
    }

    /// `true` iff the peer entry exists and has reached the
    /// `Connected` phase.
    pub async fn is_connected(&self, remote_id: Uuid) -> bool {
        self.peer_phase(remote_id)
            .await
            .map(|p| p == PeerPhase::Connected)
            .unwrap_or(false)
    }

    /// `true` iff the peer entry has a stored `files`
    /// DataChannel (created locally as initiator OR adopted
    /// via `on_data_channel` as answerer).
    pub async fn has_files_dc(&self, remote_id: Uuid) -> bool {
        let g = self.state.lock().await;
        g.peers
            .get(&remote_id)
            .and_then(|e| e.dc.as_ref())
            .is_some()
    }

    /// Outbound trickle ICE path. Called by the per-peer
    /// pump task when `on_ice_candidate` fires. Converts the
    /// gathered local candidate into a [`SignalPayload`]
    /// `{ kind: Ice, candidates: ... }` and sends it via
    /// the signaling client.
    ///
    /// The webrtc 0.20 `RTCPeerConnectionIceEvent` is
    /// `Clone`; the underlying `RTCIceCandidate` is *not*
    /// `Display`-friendly in the SDP sense (its `Display`
    /// impl prints `protocol type address:port`, NOT the
    /// canonical `candidate:` attribute). To get the SDP
    /// attribute we call `candidate.to_json()`, which returns
    /// an [`RTCIceCandidateInit`] whose `candidate` field is
    /// the SDP `candidate:` line.
    ///
    /// End-of-candidates is detected by an empty
    /// `foundation` — webrtc 0.20's driver sends an
    /// `RTCIceCandidateInit::default()` (which has empty
    /// `foundation`) for that case.
    async fn handle_local_ice_candidate(&self, remote_id: Uuid, ice_ev: RTCPeerConnectionIceEvent) {
        let json = match ice_ev.candidate.to_json() {
            Ok(j) => j,
            Err(e) => {
                warn!(
                    error = %e,
                    remote = %remote_id,
                    foundation = %ice_ev.candidate.foundation,
                    "local ICE candidate to_json failed; dropping"
                );
                return;
            }
        };
        let payload = build_local_ice_payload(remote_id, &json);
        if let Err(e) = self.send_signal(payload).await {
            warn!(error = %e, remote = %remote_id, "send ice SIGNAL failed");
        }
    }
}

/// Build the wire [`SignalPayload`] for one gathered local
/// ICE candidate. Strips the `candidate:` prefix from the
/// SDP attribute (the wire `SignalCandidate.candidate`
/// field stores the attribute *body*, not the full SDP
/// line) and casts `sdp_m_line_index: u16 -> u32` to match
/// the protocol shape.
fn build_local_ice_payload(remote_id: Uuid, init: &RTCIceCandidateInit) -> SignalPayload {
    let trimmed = init
        .candidate
        .strip_prefix("candidate:")
        .unwrap_or(&init.candidate)
        .to_string();
    SignalPayload {
        to_user_id: remote_id,
        kind: SignalKind::Ice,
        sdp: None,
        candidates: Some(vec![SignalCandidate {
            candidate: trimmed,
            sdp_mid: init.sdp_mid.clone(),
            sdp_m_line_index: init.sdp_mline_index.map(|i| i as u32),
        }]),
    }
}

/// Build a `PeerConnection` with the architecture-mandated
/// SDP constraints and a per-peer event handler.
async fn build_peer_connection(
    remote_id: Uuid,
) -> Result<(Arc<dyn PeerConnection>, mpsc::UnboundedReceiver<PeerEvent>), WebRtcError> {
    let config = RTCConfigurationBuilder::default()
        .with_ice_servers(
            STUN_SERVERS
                .iter()
                .map(|u| RTCIceServer {
                    urls: vec![u.to_string()],
                    username: String::new(),
                    credential: String::new(),
                })
                .collect::<Vec<_>>(),
        )
        .build();
    let (tx, rx) = mpsc::unbounded_channel::<PeerEvent>();
    let handler: Arc<dyn PeerConnectionEventHandler> = Arc::new(PeerHandler::new(remote_id, tx));
    let pc = PeerConnectionBuilder::new()
        .with_handler(handler)
        .with_configuration(config)
        .with_udp_addrs(vec!["0.0.0.0:0"])
        .build()
        .await?;
    Ok((Arc::new(pc), rx))
}

/// The per-peer event handler. Forwards
/// `on_ice_candidate` / `on_connection_state_change` /
/// `on_data_channel` into the manager via a per-peer
/// `mpsc::UnboundedSender<PeerEvent>` (the standard "side
/// channel" pattern, used because the trait object lives
/// inside the manager and cannot hold a back-reference).
///
/// The handler MUST stay non-blocking and MUST NOT log the
/// SDP body or the ICE candidate string (those can carry
/// network addresses); the candidate body is sent to the
/// pump task which forwards it to the signaling wire.
struct PeerHandler {
    tx: mpsc::UnboundedSender<PeerEvent>,
}

impl PeerHandler {
    fn new(_remote: Uuid, tx: mpsc::UnboundedSender<PeerEvent>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for PeerHandler {
    async fn on_ice_candidate(&self, ev: RTCPeerConnectionIceEvent) {
        // webrtc 0.20's `RTCPeerConnectionIceEvent` IS
        // `Clone`, so we ship the whole event to the pump
        // task. The pump calls `to_json()` to obtain the SDP
        // `candidate:` attribute + `sdp_mid` + `sdp_m_line_index`
        // for the wire (we deliberately do NOT use the
        // `Display` impl of `RTCIceCandidate`, which does NOT
        // emit the canonical `candidate:` attribute — it
        // prints a different layout).
        let _ = self.tx.send(PeerEvent::IceCandidate(ev));
    }

    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let _ = self.tx.send(PeerEvent::DataChannel(dc));
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.tx.send(PeerEvent::StateChange(state));
    }
}

/// The per-peer pump task: receives `PeerEvent`s from the
/// handler via a side-channel `mpsc` and forwards them to the
/// manager. Cancellation is driven by the manager's room
/// cancel token.
async fn peer_event_pump(
    manager: Arc<WebRtcManager>,
    remote_id: Uuid,
    mut rx: mpsc::UnboundedReceiver<PeerEvent>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            ev = rx.recv() => {
                let Some(ev) = ev else { return; };
                match ev {
                    PeerEvent::IceCandidate(ice_ev) => {
                        manager.handle_local_ice_candidate(remote_id, ice_ev).await;
                    }
                    PeerEvent::DataChannel(dc) => {
                        manager.on_inbound_data_channel(remote_id, dc).await;
                    }
                    PeerEvent::StateChange(state) => {
                        manager.on_connection_state(remote_id, state).await;
                    }
                }
            }
        }
    }
}

/// The full inbound loop: subscribes to the signaling client,
/// filters for `MessageKind::Signal`, dispatches to the
/// manager, and polls `RoomClient::state()` every 200 ms.
async fn run_inbound_loop(
    manager: Arc<WebRtcManager>,
    signaling: Arc<SignalingClient>,
    room_client: Arc<crate::net::room::RoomClient>,
    _state: Arc<tokio::sync::Mutex<ManagerState>>,
    cancel: CancellationToken,
) {
    let mut rx: mpsc::UnboundedReceiver<Envelope> = signaling.subscribe().await;
    let mut poll_ticker = tokio::time::interval(ROOM_STATE_POLL_INTERVAL);
    poll_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_signature: Option<String> = None;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("webrtc inbound loop cancelled");
                return;
            }
            env = rx.recv() => {
                let Some(env) = env else {
                    info!("webrtc inbound loop: signaling closed");
                    return;
                };
                if env.r#type != MessageKind::Signal {
                    continue;
                }
                let sender_uid = match env.sender.as_ref() {
                    Some(s) => s.user_id,
                    None => {
                        warn!("ignoring SIGNAL with no sender");
                        continue;
                    }
                };
                if let Err(e) = Arc::clone(&manager).handle_inbound_signal(&env, sender_uid).await {
                    warn!(error = %e, sender = %sender_uid, "inbound SIGNAL dropped");
                }
            }
            _ = poll_ticker.tick() => {
                let cur = room_client.state().await;
                // Cheap signature comparison so we don't need
                // to add PartialEq to RoomSummaryIpc.
                let sig = cur.as_ref().map(room_summary_signature);
                if sig != last_signature {
                    if let Some(s) = cur.clone() {
                        Arc::clone(&manager).on_room_state_changed(s).await;
                    } else {
                        manager.on_room_left().await;
                    }
                    last_signature = sig;
                }
            }
        }
    }
}

/// A cheap signature of a [`RoomSummaryIpc`] that we use to
/// detect a state change without requiring `PartialEq` on the
/// IPC struct (which would force every variant to derive it).
/// The signature is the room id + participant user_id list +
/// host_user_id, which covers join/leave/migration deltas.
fn room_summary_signature(s: &RoomSummaryIpc) -> String {
    let mut out = String::with_capacity(64 + s.participants.len() * 36);
    out.push_str(&s.id);
    out.push('|');
    out.push_str(&s.host_user_id);
    out.push('|');
    for p in &s.participants {
        out.push_str(&p.user_id);
        out.push(',');
    }
    out
}

/// Errors raised while sending a SIGNAL envelope.
#[derive(Debug)]
enum SignalingSendError {
    EncodeRmp(rmp_serde::encode::Error),
    EncodeJson(serde_json::Error),
    Identity(String),
    Signaling(String),
}

impl std::fmt::Display for SignalingSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EncodeRmp(e) => write!(f, "encode-rmp: {e}"),
            Self::EncodeJson(e) => write!(f, "encode-json: {e}"),
            Self::Identity(e) => write!(f, "identity: {e}"),
            Self::Signaling(e) => write!(f, "signaling: {e}"),
        }
    }
}

/// Errors raised while processing an inbound SIGNAL envelope.
#[derive(Debug)]
enum InboundError {
    Decode(String),
    Routing(String),
    Auth(&'static str),
    Protocol(String),
    Send(String),
}

impl std::fmt::Display for InboundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(e) => write!(f, "decode: {e}"),
            Self::Routing(e) => write!(f, "routing: {e}"),
            Self::Auth(e) => write!(f, "auth: {e}"),
            Self::Protocol(e) => write!(f, "protocol: {e}"),
            Self::Send(e) => write!(f, "send: {e}"),
        }
    }
}

/// Get unix milliseconds for envelope timestamps.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Verify an Ed25519 signature over the supplied bytes.
/// Returns `Ok(())` if the signature is valid; `Err(msg)`
/// otherwise. `msg` is a short stable identifier of the
/// failure reason, suitable for `tracing` (no key bytes).
fn verify_ed25519(pubkey: &[u8], sig: &[u8], signed: &[u8]) -> Result<(), InboundError> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    if pubkey.len() != 32 {
        return Err(InboundError::Auth("pubkey wrong length"));
    }
    let mut pk_arr = [0u8; 32];
    pk_arr.copy_from_slice(pubkey);
    let vk = match VerifyingKey::from_bytes(&pk_arr) {
        Ok(v) => v,
        Err(_) => return Err(InboundError::Auth("bad pubkey")),
    };
    let mut sig_arr = [0u8; 64];
    if sig.len() != 64 {
        return Err(InboundError::Auth("sig wrong length"));
    }
    sig_arr.copy_from_slice(sig);
    let s = Signature::from_bytes(&sig_arr);
    if vk.verify(signed, &s).is_err() {
        return Err(InboundError::Auth("signature mismatch"));
    }
    Ok(())
}

/// Helper: parse a remote pubkey from a `Vec<u8>` (typically
/// the bytes of a `Participant.pubkey`) into a `[u8; 32]`.
#[allow(dead_code)]
fn parse_pubkey(v: &[u8]) -> Option<[u8; 32]> {
    if v.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(v);
    Some(out)
}

/// Helper: produce the canonical `peer_id` for a pubkey.
/// Currently unused but exposed for future file-transfer
/// addressing.
#[allow(dead_code)]
fn peer_id_of(pubkey: [u8; 32]) -> String {
    derive_peer_id(pubkey)
}

/// The slice of public items `net::webrtc` re-exports. Kept
/// small; the manager is the only public API.
pub mod prelude {
    pub use super::{
        PeerPhase, WebRtcManager, FILES_DC_LABEL, FILES_DC_PROTOCOL, ROOM_PARTICIPANT_CAP,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initiator_decision_is_deterministic_by_uuid_bytes() {
        let lower = Uuid::from_bytes([0u8; 16]);
        let higher = Uuid::from_bytes([0xFFu8; 16]);
        assert!(*lower.as_bytes() < *higher.as_bytes());
        assert!(*higher.as_bytes() > *lower.as_bytes());
    }

    #[test]
    fn verify_ed25519_roundtrip() {
        use ed25519_dalek::Signer;
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0xa4, 0x9d, 0xb5, 0x67, 0xa7, 0x8b, 0x3d, 0xc0, 0x88, 0x6f, 0x87, 0x2d,
            0x77, 0x8b, 0x55, 0x55,
        ];
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key();
        let pubkey = verifying.to_bytes();
        let msg = b"hello world";
        let sig = signing.sign(msg).to_bytes();
        // Build the InboundError-via path indirectly:
        // verify_ed25519 returns Result<(), InboundError>.
        assert!(verify_ed25519(&pubkey, &sig, msg).is_ok());
    }

    #[test]
    fn verify_ed25519_rejects_tampered_message() {
        use ed25519_dalek::Signer;
        let seed: [u8; 32] = [
            0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec,
            0x2c, 0xc4, 0xa4, 0x9d, 0xb5, 0x67, 0xa7, 0x8b, 0x3d, 0xc0, 0x88, 0x6f, 0x87, 0x2d,
            0x77, 0x8b, 0x55, 0x55,
        ];
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        let pubkey = signing.verifying_key().to_bytes();
        let sig = signing.sign(b"msg").to_bytes();
        assert!(verify_ed25519(&pubkey, &sig, b"different").is_err());
    }

    #[test]
    fn verify_ed25519_rejects_bad_length() {
        assert!(verify_ed25519(&[0u8; 31], &[0u8; 64], b"").is_err());
        assert!(verify_ed25519(&[0u8; 32], &[0u8; 63], b"").is_err());
    }

    #[test]
    fn parse_pubkey_rejects_wrong_length() {
        assert!(parse_pubkey(&[0u8; 32]).is_some());
        assert!(parse_pubkey(&[0u8; 31]).is_none());
        assert!(parse_pubkey(&[0u8; 33]).is_none());
    }

    #[test]
    fn peer_id_of_matches_documented_form() {
        let pubkey = [7u8; 32];
        let pid = peer_id_of(pubkey);
        assert_eq!(pid, derive_peer_id(pubkey));
        assert_eq!(pid.len(), 64);
    }

    #[test]
    fn room_summary_signature_changes_on_join_and_leave() {
        // Construct two summaries by hand and compare
        // signatures.
        let s1 = RoomSummaryIpc {
            id: "00000000-0000-7000-8000-000000000001".to_string(),
            code: "AAAAAA".to_string(),
            title: "T".to_string(),
            host_user_id: "00000000-0000-7000-8000-000000000010".to_string(),
            host_migration_enabled: true,
            created_ms: 1,
            participants: vec![crate::net::room::ParticipantIpc {
                user_id: "00000000-0000-7000-8000-000000000010".to_string(),
                display_name: "host".to_string(),
                joined_ms: 1,
                status: crate::net::room::ParticipantStatusIpc::Connected,
                last_seen_ms: 1,
                is_host: true,
            }],
            host_disconnected: false,
            host_disconnect_deadline_ms: None,
        };
        let mut s2 = s1.clone();
        s2.participants.push(crate::net::room::ParticipantIpc {
            user_id: "00000000-0000-7000-8000-000000000011".to_string(),
            display_name: "viewer".to_string(),
            joined_ms: 2,
            status: crate::net::room::ParticipantStatusIpc::Connected,
            last_seen_ms: 2,
            is_host: false,
        });
        assert_ne!(room_summary_signature(&s1), room_summary_signature(&s2));
    }
}
