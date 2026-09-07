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

/// P7-T01: per-connection epoch counter for bearer binding.
/// Incremented server-wide on every fresh HELLO that does NOT
/// carry a valid resume_token. A bearer issued under epoch N
/// carries that epoch in its `bearer_tokens` row and is only
/// accepted on a connection whose `connection_epoch` (held
/// in the [`state::ConnState`]) is also N; an older bearer
/// presented on a newer connection (stolen-bearer replay) is
/// rejected by [`BearerBinding::matches`].
#[derive(Debug, Default)]
pub struct EpochCounter(pub u64);

impl EpochCounter {
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(1);
        self.0
    }
    pub fn current(&self) -> u64 {
        self.0
    }
}

/// P7-T01: how many AUTH failures an unauthenticated caller
/// may incur in [`AUTH_FAILURE_WINDOW_MS`] before the server
/// throttles subsequent AUTH frames. The throttle is
/// per-connection-state (NOT global) so a noisy client cannot
/// affect a different connection's path.
pub const AUTH_FAILURE_THRESHOLD: usize = 5;
pub const AUTH_FAILURE_WINDOW_MS: i64 = 60_000;

/// Errors raised by the auth path. The `WsError` and `AuthFailReason`
/// types in this crate close over the wire contract for these.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AuthError {
    #[error("bad signature")]
    BadSig,

    #[error("challenge expired")]
    Expired,

    #[error("pubkey is banned")]
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
