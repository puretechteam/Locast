//! Per-connection authentication state machine.
//!
//! The state moves through:
//!
//! - `New` - a fresh WS connection; nothing has been received yet.
//! - `HelloReceived` - HELLO has been read; the server has not yet
//!   sent a WELCOME/CHALLENGE pair (this state is internal; it
//!   exists for symmetry and is collapsed into `ChallengeSent`
//!   after the WELCOME+CHALLENGE frames are written).
//! - `ChallengeSent` - WELCOME and CHALLENGE have been written;
//!   waiting for AUTH or AUTH_RESUME.
//! - `Authenticated` - AUTH (or AUTH_RESUME) has been verified; the
//!   connection has a `user_id` and `pubkey` for the duration of
//!   the session.
//! - `Closed` - the connection is being torn down; no further
//!   messages will be processed.
//!
//! State transitions are explicit; illegal transitions return
//! `false` from the transition helpers so the WS layer can
//! detect protocol violations and tear down the connection.
//!
//! P7-T01: every `ChallengeSent` carries a `connection_epoch`
//! minted from the server-wide [`crate::auth::EpochCounter`].
//! The epoch is stamped onto the bearer the AUTH_OK mint
//! produces so a stolen bearer cannot be replayed across a
//! new connection. The optional `resume_token` is set only on
//! a HELLO that included a valid `HelloResume`.

#![forbid(unsafe_code)]

use uuid::Uuid;

/// The per-connection auth state machine.
#[derive(Debug, Clone)]
pub enum ConnState {
    New,
    HelloReceived {
        session_id: Uuid,
        server_ts_ms: i64,
    },
    ChallengeSent {
        session_id: Uuid,
        server_ts_ms: i64,
        nonce: [u8; 32],
        expires_ms: i64,
        /// P7-T01: monotonic epoch counter assigned at HELLO
        /// time. Used to bind the issued bearer to this
        /// connection so it cannot be replayed on a future
        /// connection.
        connection_epoch: u64,
        /// P7-T01: the SHA-256 of the resume_token the client
        /// presented in HELLO, if any. `None` for a fresh
        /// HELLO. When set, AUTH_RESUME is accepted in place
        /// of AUTH (no fresh CHALLENGE round trip).
        resume_token_hash: Option<[u8; 32]>,
    },
    Authenticated {
        session_id: Uuid,
        user_id: Uuid,
        pubkey: [u8; 32],
        connection_epoch: u64,
    },
    Closed,
}

impl ConnState {
    /// Return the current state's stable name. Used for logging.
    pub fn name(&self) -> &'static str {
        match self {
            ConnState::New => "New",
            ConnState::HelloReceived { .. } => "HelloReceived",
            ConnState::ChallengeSent { .. } => "ChallengeSent",
            ConnState::Authenticated { .. } => "Authenticated",
            ConnState::Closed => "Closed",
        }
    }

    /// Move from `New` to `HelloReceived`. Returns the new state on
    /// success; `Err(self)` if the transition is illegal.
    pub fn transition_hello(self, session_id: Uuid, server_ts_ms: i64) -> Result<Self, Self> {
        match self {
            ConnState::New => Ok(ConnState::HelloReceived {
                session_id,
                server_ts_ms,
            }),
            other => Err(other),
        }
    }

    /// Move from `HelloReceived` to `ChallengeSent`.
    pub fn transition_challenge(
        self,
        nonce: [u8; 32],
        expires_ms: i64,
        connection_epoch: u64,
        resume_token_hash: Option<[u8; 32]>,
    ) -> Result<Self, Self> {
        match self {
            ConnState::HelloReceived {
                session_id,
                server_ts_ms,
            } => Ok(ConnState::ChallengeSent {
                session_id,
                server_ts_ms,
                nonce,
                expires_ms,
                connection_epoch,
                resume_token_hash,
            }),
            other => Err(other),
        }
    }

    /// Move from `ChallengeSent` to `Authenticated`.
    pub fn transition_authenticated(self, user_id: Uuid, pubkey: [u8; 32]) -> Result<Self, Self> {
        match self {
            ConnState::ChallengeSent {
                session_id,
                connection_epoch,
                ..
            } => Ok(ConnState::Authenticated {
                session_id,
                user_id,
                pubkey,
                connection_epoch,
            }),
            other => Err(other),
        }
    }

    /// Move to `Closed`. Always succeeds; the only valid state
    /// after a connection ends is `Closed`.
    pub fn close(self) -> Self {
        ConnState::Closed
    }

    /// Return the session id, if one has been assigned.
    pub fn session_id(&self) -> Option<Uuid> {
        match self {
            ConnState::HelloReceived { session_id, .. }
            | ConnState::ChallengeSent { session_id, .. }
            | ConnState::Authenticated { session_id, .. } => Some(*session_id),
            _ => None,
        }
    }

    /// Return the authenticated user id, if any.
    pub fn user_id(&self) -> Option<Uuid> {
        match self {
            ConnState::Authenticated { user_id, .. } => Some(*user_id),
            _ => None,
        }
    }

    /// Return the authenticated pubkey, if any.
    pub fn pubkey(&self) -> Option<[u8; 32]> {
        match self {
            ConnState::Authenticated { pubkey, .. } => Some(*pubkey),
            _ => None,
        }
    }

    /// P7-T01: the `connection_epoch` assigned to this
    /// connection. `None` for `New` and `HelloReceived`.
    pub fn connection_epoch(&self) -> Option<u64> {
        match self {
            ConnState::ChallengeSent {
                connection_epoch, ..
            } => Some(*connection_epoch),
            ConnState::Authenticated {
                connection_epoch, ..
            } => Some(*connection_epoch),
            _ => None,
        }
    }

    /// P7-T01: the SHA-256 of the resume_token this connection
    /// presented in HELLO. `None` for a fresh HELLO.
    pub fn resume_token_hash(&self) -> Option<[u8; 32]> {
        match self {
            ConnState::ChallengeSent {
                resume_token_hash, ..
            } => *resume_token_hash,
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sid() -> Uuid {
        Uuid::now_v7()
    }

    #[test]
    fn happy_path_transitions() {
        let s = ConnState::New;
        let s = s.transition_hello(sid(), 1).unwrap();
        let s = s.transition_challenge([7u8; 32], 2, 1, None).unwrap();
        let s = s.transition_authenticated(sid(), [9u8; 32]).unwrap();
        assert!(matches!(s, ConnState::Authenticated { .. }));
        assert_eq!(s.connection_epoch(), Some(1));
        let s = s.close();
        assert!(matches!(s, ConnState::Closed));
    }

    #[test]
    fn duplicate_hello_rejected() {
        let s = ConnState::New.transition_hello(sid(), 1).unwrap();
        let r = s.clone().transition_hello(sid(), 2);
        assert!(r.is_err());
    }

    #[test]
    fn auth_in_new_rejected() {
        let r = ConnState::New.transition_authenticated(sid(), [1u8; 32]);
        assert!(r.is_err());
    }

    #[test]
    fn session_id_visible_through_challenge() {
        let id = sid();
        let s = ConnState::New.transition_hello(id, 1).unwrap();
        let s = s.transition_challenge([0u8; 32], 2, 1, None).unwrap();
        assert_eq!(s.session_id(), Some(id));
        assert_eq!(s.connection_epoch(), Some(1));
    }

    #[test]
    fn resume_token_hash_visible_through_challenge() {
        let hash = [0xABu8; 32];
        let s = ConnState::New.transition_hello(sid(), 1).unwrap();
        let s = s.transition_challenge([0u8; 32], 2, 1, Some(hash)).unwrap();
        assert_eq!(s.resume_token_hash(), Some(hash));
    }
}
