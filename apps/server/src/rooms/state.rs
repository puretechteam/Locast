//! In-memory per-room state. The [`RoomState`] struct is held
//! behind a `tokio::sync::RwLock` inside the
//! [`super::registry::RoomRegistry`] so multiple connections
//! can read snapshots while one writer mutates.

#![forbid(unsafe_code)]

use locast_protocol::room::{Participant, ParticipantSelf, ParticipantStatus, RoomSummary};
use uuid::Uuid;

/// The mutable per-room state. Lives behind a
/// `tokio::sync::RwLock<RoomState>` so readers (snapshot
/// builds) don't block other readers and don't block writers
/// of other rooms.
#[derive(Debug)]
pub struct RoomState {
    pub id: Uuid,
    pub code: String,
    pub title: String,
    pub host_user_id: Uuid,
    pub host_pubkey: [u8; 32],
    pub host_migration_enabled: bool,
    pub created_ms: i64,
    pub state: RoomLifecycle,
    /// unix-ms when the host's transport was lost and the
    /// server started the 30s grace. `None` while the host is
    /// connected. The dispatch task waits up to the
    /// configured `host_disconnect_grace_ms` from this
    /// timestamp before electing a new host.
    pub host_disconnect_deadline_ms: Option<i64>,
    /// The current participants. The host is always first
    /// (or, more precisely, the entry whose `is_host` is
    /// `true` is the current host; the original creator is
    /// still the first joined).
    pub participants: Vec<ParticipantRecord>,
}

/// The room lifecycle. `Open` is the steady state. `Closing`
/// is the brief period between the server deciding to end
/// the room and the ROOM_CLOSED broadcast being flushed.
/// `Ended` is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomLifecycle {
    Open,
    Ended,
}

impl RoomState {
    /// Build a brand-new room in the `Open` state.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Uuid,
        code: String,
        title: String,
        host_user_id: Uuid,
        host_pubkey: [u8; 32],
        host_migration_enabled: bool,
        created_ms: i64,
        cap_set: u32,
    ) -> Self {
        let host = ParticipantRecord {
            user_id: host_user_id,
            pubkey: host_pubkey,
            display_name: String::new(),
            joined_ms: created_ms,
            status: ParticipantStatus::Connected,
            last_seen_ms: created_ms,
            is_host: true,
            cap_set,
        };
        Self {
            id,
            code,
            title,
            host_user_id,
            host_pubkey,
            host_migration_enabled,
            created_ms,
            state: RoomLifecycle::Open,
            host_disconnect_deadline_ms: None,
            participants: vec![host],
        }
    }

    /// The current host record, if any. Returns `None` only
    /// if the participants list is empty (impossible in the
    /// `Open` state).
    pub fn host(&self) -> Option<&ParticipantRecord> {
        self.participants.iter().find(|p| p.is_host)
    }

    /// The current host record (mutable), if any.
    pub fn host_mut(&mut self) -> Option<&mut ParticipantRecord> {
        self.participants.iter_mut().find(|p| p.is_host)
    }

    /// `true` if the host's transport is currently considered
    /// disconnected (i.e. the grace timer is running).
    pub fn host_disconnected(&self) -> bool {
        self.host_disconnect_deadline_ms.is_some()
    }

    /// Build a public-facing summary.
    pub fn snapshot(&self) -> RoomSummary {
        RoomSummary {
            id: self.id,
            code: self.code.clone(),
            title: self.title.clone(),
            host_user_id: self.host_user_id,
            host_migration_enabled: self.host_migration_enabled,
            created_ms: self.created_ms,
            participants: self
                .participants
                .iter()
                .filter(|p| p.status != ParticipantStatus::Left)
                .map(ParticipantRecord::to_public)
                .collect(),
            host_disconnected: self.host_disconnected(),
            host_disconnect_deadline_ms: self.host_disconnect_deadline_ms,
        }
    }

    /// View of the caller's own participant record, for
    /// `ROOM_CREATED` / `ROOM_JOINED`.
    pub fn self_view(&self, user_id: Uuid) -> Option<ParticipantSelf> {
        self.participants
            .iter()
            .find(|p| p.user_id == user_id)
            .map(|p| ParticipantSelf {
                user_id: p.user_id,
                cap_set: p.cap_set,
                joined_ms: p.joined_ms,
            })
    }
}

/// The server-side participant record. Lifted to the public
/// [`Participant`] wire shape via [`ParticipantRecord::to_public`].
#[derive(Debug, Clone)]
pub struct ParticipantRecord {
    pub user_id: Uuid,
    pub pubkey: [u8; 32],
    pub display_name: String,
    pub joined_ms: i64,
    pub status: ParticipantStatus,
    pub last_seen_ms: i64,
    pub is_host: bool,
    pub cap_set: u32,
}

impl ParticipantRecord {
    /// The wire shape that other clients see.
    pub fn to_public(&self) -> Participant {
        Participant {
            user_id: self.user_id,
            pubkey: self.pubkey.to_vec(),
            display_name: self.display_name.clone(),
            joined_ms: self.joined_ms,
            status: self.status,
            last_seen_ms: self.last_seen_ms,
            is_host: self.is_host,
        }
    }
}
