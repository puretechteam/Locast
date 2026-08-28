//! `identity::types` - pure types and helpers.
//!
//! This file holds the byte-level helpers that are easiest to test
//! in isolation: Ed25519 keypair generation, `sha256(public_key)`
//! user-id derivation, and display-name validation. None of these
//! touch the filesystem, the keyring, or the clock; they are pure
//! functions of their inputs and can be tested without `tempfile`,
//! `tokio`, or any IO at all.
//!
//! See `docs/ARCHITECTURE.md` section 10.6 for the design.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::fmt;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

/// A generated Ed25519 keypair. The private half (`signing`) is
/// the value that lives in the OS keyring; the public half is the
/// stable identity the rest of the app uses.
///
/// The struct is `pub` so the `keystore` module can hand it to the
/// keyring, but downstream consumers (`commands::identity`) only
/// ever see the [`Identity`] view, which is the public half plus
/// the display name.
#[derive(Debug)]
pub struct Keypair {
    /// The Ed25519 signing key (private). 32 bytes.
    pub signing: SigningKey,
}

impl Keypair {
    /// Encode the 32-byte private key as standard base64. The keyring
    /// stores it as a UTF-8 string; base64 is the canonical encoding
    /// and is what [`Self::from_base64`] decodes.
    pub fn to_base64(&self) -> String {
        BASE64.encode(self.signing.to_bytes())
    }

    /// Decode a base64 string produced by [`Self::to_base64`] back
    /// into a `Keypair`. Returns `None` on any decoding error
    /// (wrong length, invalid base64, all-zero key).
    pub fn from_base64(s: &str) -> Option<Self> {
        let bytes = BASE64.decode(s).ok()?;
        if bytes.len() != 32 {
            return None;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&bytes);
        SigningKey::try_from(&arr[..])
            .ok()
            .map(|signing| Self { signing })
    }

    /// The 32-byte public key. Mirrors
    /// `ed25519_dalek::SigningKey::verifying_key().to_bytes()`;
    /// provided as a stable helper so the network layer does not
    /// need to import `ed25519_dalek` directly.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    /// Construct a fresh `Keypair` using `OsRng`. Equivalent
    /// to the free function [`generate`]; provided as a
    /// method so call sites can write `Keypair::generate()`.
    pub fn generate() -> Keypair {
        generate()
    }

    /// Sign the 32-byte nonce from a server CHALLENGE. P2-T02.
    /// The signature is over the raw nonce with no domain tag
    /// (architecture §20.4.4: "Client signs the nonce"). The
    /// returned 64-byte signature is the value the client puts
    /// in the `AUTH` envelope's `sig` field.
    ///
    /// This is the ONLY way the private key ever produces a
    /// signature for the wire. There is intentionally no
    /// `sign_arbitrary` helper: the keypair should never sign
    /// anything except a server nonce, and the dedicated helper
    /// makes that intent obvious in code review.
    pub fn sign_challenge(&self, nonce: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer;
        let sig = self.signing.sign(nonce);
        sig.to_bytes()
    }
}

/// The public view of the local identity. This is the only shape
/// the Tauri command surface returns to the webview and the only
/// shape that ever crosses the IPC boundary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct Identity {
    /// `sha256(public_key)` hex (64 lowercase chars). The stable
    /// `user_identities.id` value.
    pub user_id: String,
    /// The 32-byte Ed25519 public key, standard base64. The stable
    /// `user_identities.public_key` value.
    pub public_key: String,
    /// The user-set display name. 1-32 chars, no leading/trailing
    /// whitespace, no control characters.
    pub display_name: String,
}

impl Identity {
    /// Construct an `Identity` from a signing key + display name.
    /// Derives `user_id` from `sha256(public_key)` and base64-encodes
    /// the public key.
    pub fn from_signing_key(signing: &SigningKey, display_name: &str) -> Self {
        let public = signing.verifying_key().to_bytes();
        let public_b64 = BASE64.encode(public);
        let user_id = derive_user_id(public);
        Self {
            user_id,
            public_key: public_b64,
            display_name: display_name.to_string(),
        }
    }
}

/// Generate a fresh Ed25519 keypair using `OsRng`.
///
/// This is the only place a keypair is minted. The returned value
/// must be persisted to the keyring by the caller; it is not
/// stored anywhere else and the `signing` field is sensitive.
pub fn generate() -> Keypair {
    let mut csprng = OsRng;
    let signing = SigningKey::generate(&mut csprng);
    Keypair { signing }
}

/// Derive the stable `user_id` from a 32-byte Ed25519 public key.
/// `user_id = sha256(public_key)` hex, 64 lowercase characters.
pub fn derive_user_id(public_key: [u8; 32]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_key);
    let digest = hasher.finalize();
    hex::encode(digest)
}

/// Errors returned by [`validate_display_name`]. The error type is
/// unit-only (no context) because the caller already has the
/// offending input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayNameError {
    /// The reason for rejection, as a stable lowercase string. The
    /// Tauri command surface maps this to a closed `AppError`
    /// variant.
    reason: DisplayNameErrorReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DisplayNameErrorReason {
    Empty,
    TooLong,
    LeadingOrTrailingWhitespace,
    ContainsControl,
}

impl DisplayNameError {
    /// The lowercase machine-readable reason. Stable across releases.
    pub fn kind(&self) -> &'static str {
        match self.reason {
            DisplayNameErrorReason::Empty => "empty",
            DisplayNameErrorReason::TooLong => "too_long",
            DisplayNameErrorReason::LeadingOrTrailingWhitespace => "whitespace",
            DisplayNameErrorReason::ContainsControl => "control",
        }
    }
}

impl fmt::Display for DisplayNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            DisplayNameErrorReason::Empty => f.write_str("display name is empty"),
            DisplayNameErrorReason::TooLong => f.write_str("display name is longer than 32 chars"),
            DisplayNameErrorReason::LeadingOrTrailingWhitespace => {
                f.write_str("display name has leading or trailing whitespace")
            }
            DisplayNameErrorReason::ContainsControl => {
                f.write_str("display name contains a control character")
            }
        }
    }
}

impl std::error::Error for DisplayNameError {}

/// Validate a user-supplied display name. The rules (architecture
/// section 10.6):
///
/// - 1-32 characters (after trimming would be a no-op, the
///   rejection runs first).
/// - No leading or trailing whitespace.
/// - No control characters (`< 0x20` or `0x7F..=0x9F`).
///
/// The check is intentionally strict; the display name is shown to
/// other participants.
pub fn validate_display_name(name: &str) -> Result<&str, DisplayNameError> {
    if name.is_empty() {
        return Err(DisplayNameError {
            reason: DisplayNameErrorReason::Empty,
        });
    }
    if name.chars().count() > 32 {
        return Err(DisplayNameError {
            reason: DisplayNameErrorReason::TooLong,
        });
    }
    if name != name.trim() {
        return Err(DisplayNameError {
            reason: DisplayNameErrorReason::LeadingOrTrailingWhitespace,
        });
    }
    for c in name.chars() {
        let cu = c as u32;
        if cu < 0x20 || (0x7F..=0x9F).contains(&cu) {
            return Err(DisplayNameError {
                reason: DisplayNameErrorReason::ContainsControl,
            });
        }
    }
    Ok(name)
}
