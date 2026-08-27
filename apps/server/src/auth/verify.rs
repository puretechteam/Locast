//! Auth-side cryptographic verification. Thin wrapper around
//! [`locast_crypto::ed25519::verify`] that maps verification
//! failures onto the closed [`AuthError`] set.

#![forbid(unsafe_code)]

use locast_crypto::ed25519;

use super::AuthError;

/// Verify the AUTH signature. The caller has already extracted the
/// raw 32-byte nonce from the connection's `ChallengeSent` state.
/// Returns `Ok(())` on success and `Err(AuthError::BadSig)` on
/// any failure (malformed pubkey, malformed signature,
/// signature does not verify, all-zero pubkey, etc.).
pub fn verify_auth(pubkey: &[u8; 32], nonce: &[u8; 32], sig: &[u8; 64]) -> Result<(), AuthError> {
    match ed25519::verify(pubkey, nonce, sig) {
        Ok(()) => Ok(()),
        Err(_) => Err(AuthError::BadSig),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::RngCore;

    fn random_seed() -> [u8; 32] {
        let mut s = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut s);
        s
    }

    #[test]
    fn good_signature_verifies() {
        let seed = random_seed();
        let signing = SigningKey::from_bytes(&seed);
        let public = signing.verifying_key().to_bytes();
        let nonce = [42u8; 32];
        let sig = ed25519::sign(&seed, &nonce);
        verify_auth(&public, &nonce, &sig).expect("good sig");
    }

    #[test]
    fn bad_signature_fails() {
        let seed = random_seed();
        let signing = SigningKey::from_bytes(&seed);
        let public = signing.verifying_key().to_bytes();
        let nonce = [42u8; 32];
        let sig = ed25519::sign(&seed, b"different message");
        assert_eq!(verify_auth(&public, &nonce, &sig), Err(AuthError::BadSig));
    }
}
