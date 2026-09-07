//! Bearer token minting and hashing. The plaintext token is
//! only ever held by the client and the moment of minting; the
//! server stores only the SHA-256 hash (§21.3).
//!
//! P7-T01: a bearer is bound to the `(user_id, connection_epoch,
//! room_id)` triple it was minted for. The `connection_epoch` is
//! a server-assigned monotonic u64 that changes on every fresh
//! HELLO; reusing a bearer across epochs (stolen-bearer replay)
//! is rejected by [`BearerBinding::matches`]. `room_id` is the
//! room the bearer was issued in (or `None` for a transport-only
//! bearer).

#![forbid(unsafe_code)]

use locast_crypto::sha256;
use rand::RngCore;

/// P7-T01: the per-bearer binding the server stamps when it
/// mints a token. Stored alongside the SHA-256 hash so the
/// post-handshake path can reject stolen or replayed bearers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BearerBinding {
    pub user_id: uuid::Uuid,
    pub connection_epoch: u64,
    /// The room the bearer was issued in. `None` for a
    /// transport-only bearer (the user had not yet joined a
    /// room at mint time).
    pub room_id: Option<uuid::Uuid>,
}

/// Mint a fresh 32-byte bearer token using the supplied CSPRNG.
pub fn mint_bearer<R: RngCore>(rng: &mut R) -> [u8; 32] {
    let mut token = [0u8; 32];
    rng.fill_bytes(&mut token);
    token
}

/// Hash a bearer token. The server stores only this hash.
pub fn hash_bearer(token: &[u8; 32]) -> [u8; 32] {
    sha256::sha256(token)
}

impl BearerBinding {
    /// `true` when `self` matches `(user_id, epoch, room_id)`.
    /// Used by the WS layer to reject a bearer presented on the
    /// wrong connection_epoch or in the wrong room.
    pub fn matches(&self, user_id: uuid::Uuid, epoch: u64, room_id: Option<uuid::Uuid>) -> bool {
        self.user_id == user_id && self.connection_epoch == epoch && self.room_id == room_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;
    use uuid::Uuid;

    #[test]
    fn mint_bearer_is_32_bytes_and_random() {
        let a = mint_bearer(&mut OsRng);
        let b = mint_bearer(&mut OsRng);
        assert_eq!(a.len(), 32);
        assert_ne!(a, b);
    }

    #[test]
    fn hash_bearer_is_deterministic_and_distinct_from_input() {
        let tok = [1u8; 32];
        let h1 = hash_bearer(&tok);
        let h2 = hash_bearer(&tok);
        assert_eq!(h1, h2);
        assert_ne!(h1, tok);
    }

    #[test]
    fn binding_matches_only_when_all_fields_match() {
        let u = Uuid::now_v7();
        let r = Uuid::now_v7();
        let b = BearerBinding {
            user_id: u,
            connection_epoch: 7,
            room_id: Some(r),
        };
        assert!(b.matches(u, 7, Some(r)));
        assert!(!b.matches(u, 8, Some(r)));
        assert!(!b.matches(Uuid::now_v7(), 7, Some(r)));
        assert!(!b.matches(u, 7, None));
    }
}
