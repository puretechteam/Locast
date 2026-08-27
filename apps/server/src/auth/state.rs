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
//!   waiting for AUTH.
//! - `Authenticated` - AUTH has been verified; the connection
//!   has a `user_id` and `pubkey` for the duration of the
//!   session.
//! - `Closed` - the connection is being torn down; no further
//!   messages will be processed.
//!
//! State transitions are explicit; illegal transitions return
//! `false` from the transition helpers so the WS layer can
//! detect protocol violations and tear down the connection.

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
    },
    Authenticated {
        session_id: Uuid,
        user_id: Uuid,
        pubkey: [u8; 32],
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
    pub fn transition_challenge(self, nonce: [u8; 32], expires_ms: i64) -> Result<Self, Self> {
        match self {
            ConnState::HelloReceived {
                session_id,
                server_ts_ms,
            } => Ok(ConnState::ChallengeSent {
                session_id,
                server_ts_ms,
                nonce,
                expires_ms,
            }),
            other => Err(other),
        }
    }

    /// Move from `ChallengeSent` to `Authenticated`.
    pub fn transition_authenticated(self, user_id: Uuid, pubkey: [u8; 32]) -> Result<Self, Self> {
        match self {
            ConnState::ChallengeSent { session_id, .. } => Ok(ConnState::Authenticated {
                session_id,
                user_id,
                pubkey,
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
        let s = s.transition_challenge([7u8; 32], 2).unwrap();
        let s = s.transition_authenticated(sid(), [9u8; 32]).unwrap();
        assert!(matches!(s, ConnState::Authenticated { .. }));
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
        let s = s.transition_challenge([0u8; 32], 2).unwrap();
        assert_eq!(s.session_id(), Some(id));
    }
}
