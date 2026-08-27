//! Authentication module.
//!
//! Houses the per-connection state machine
//! ([`state::ConnState`]), the signature verification helper
//! ([`verify::verify_auth`]), and the bearer token mint/hash
//! helpers ([`bearer::mint_bearer`], [`bearer::hash_bearer`]).
//!
//! The [`AuthError`] type is the closed set the WS layer maps onto
//! `AUTH_FAIL` reasons.

#![forbid(unsafe_code)]

pub mod bearer;
pub mod state;
pub mod verify;

use thiserror::Error;

use locast_protocol::handshake::AuthFailReason;

/// Errors raised by the auth path. The `WsError` and `AuthFailReason`
/// types in this crate close over the wire contract for these.
///
/// `Banned` is reserved for the `banned_pubkeys` table from §21.3;
/// P2-T02 does not implement the table or the lookup, so the
/// variant is reachable in the type system but not in the dispatch
/// path today. The P3+ banlist work will wire it in.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("bad signature")]
    BadSig,

    #[error("challenge expired")]
    Expired,

    #[error("pubkey is banned")]
    #[allow(dead_code)] // wired in with the §21.3 banlist in P3+
    Banned,

    #[error("rate limited")]
    RateLimited,

    #[error("internal auth error: {0}")]
    Internal(String),
}

impl From<AuthError> for AuthFailReason {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::BadSig => AuthFailReason::BadSig,
            AuthError::Expired => AuthFailReason::Expired,
            AuthError::Banned => AuthFailReason::Banned,
            AuthError::RateLimited => AuthFailReason::Rate,
            // The internal error case still needs a wire value;
            // we send BadSig as the closest stable option so
            // operators can correlate with logs.
            AuthError::Internal(_) => AuthFailReason::BadSig,
        }
    }
}
