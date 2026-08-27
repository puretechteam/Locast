//! Bearer token minting and hashing. The plaintext token is
//! only ever held by the client and the moment of minting; the
//! server stores only the SHA-256 hash (§21.3).

#![forbid(unsafe_code)]

use locast_crypto::sha256;
use rand::RngCore;

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

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

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
}
