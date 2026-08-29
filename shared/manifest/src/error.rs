//! Error types for the manifest canonical form.

use serde::ser;
use thiserror::Error;

/// Errors that can occur while producing the canonical form of a
/// [`crate::model::MediaManifest`].
#[derive(Debug, Error)]
pub enum CanonicalError {
    /// The custom serializer was asked to emit a non-finite float
    /// (`NaN`, `+Infinity`, or `-Infinity`). The canonical form is
    /// integers-only and rejects any such value.
    #[error("non-finite float values are not allowed in the canonical form")]
    InvalidNonFiniteFloat,

    /// A map key was not representable as a string. The custom
    /// serializer requires string keys so it can sort them.
    #[error("map keys must be strings; got non-string key")]
    NonStringMapKey,

    /// A byte sequence could not be emitted as a string. The canonical
    /// form is JSON text and only string values are permitted.
    #[error("byte strings are not allowed in the canonical form")]
    BytesNotAllowed,

    /// `serde_json` rejected the data while the custom serializer was
    /// writing the final output.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// `serde` raised an error from one of the `serialize_*` calls
    /// while the custom serializer was collecting the value tree.
    #[error("custom serialization error: {0}")]
    Custom(String),
}

impl ser::Error for CanonicalError {
    fn custom<T>(msg: T) -> Self
    where
        T: std::fmt::Display,
    {
        CanonicalError::Custom(msg.to_string())
    }
}

/// Result alias for [`sign_manifest`]. Sign-side failures are
/// exclusively canonicalization failures because the underlying
/// Ed25519 `sign` is infallible.
pub type SigningResult<T> = Result<T, CanonicalError>;

/// Reason a `host_signature.public_key` was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvalidPublicKeyReason {
    /// The base64 string could not be decoded.
    #[error("base64 decode failed")]
    Base64Decode,
    /// The decoded blob had the wrong length.
    #[error("expected 32 bytes, got {0}")]
    WrongLength(usize),
    /// The bytes were a valid 32-byte blob but dalek rejected them
    /// as an invalid curve point (the all-zero identity element is
    /// the most common offender; not all non-curve 32-byte strings
    /// are caught here because dalek's own constructor also does
    /// this check).
    #[error("ed25519 verify rejected the public key")]
    RejectedByVerifier,
}

/// Reason a `host_signature.value` was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvalidSignatureEncodingReason {
    /// The base64 string could not be decoded.
    #[error("base64 decode failed")]
    Base64Decode,
    /// The decoded blob had the wrong length.
    #[error("expected 64 bytes, got {0}")]
    WrongLength(usize),
}

/// Errors raised by [`verify_manifest`](crate::signing::verify_manifest).
///
/// The variants are ordered roughly from "the caller forgot
/// something" to "the bytes are bad" so that an operator triaging a
/// failure can read the `Debug` output top-to-bottom and know
/// whether to nag the host, fix the manifest, or block the
/// download.
#[derive(Debug, Error)]
pub enum VerifyError {
    /// The manifest has no `host_signature` block. A viewer that
    /// receives this manifest cannot prove provenance and must
    /// refuse to download any media.
    #[error("manifest has no host_signature")]
    MissingSignature,

    /// The `host_signature.algorithm` field is not a known value.
    /// Only `"ed25519"` is supported in v1.
    #[error("unsupported signature algorithm: {0}")]
    UnsupportedAlgorithm(String),

    /// The `host_signature.public_key` field is not a valid 32-byte
    /// Ed25519 verifying key. The reason is structured so the
    /// viewer can log or surface a precise reason.
    #[error("invalid host_signature.public_key: {0}")]
    InvalidPublicKey(#[source] InvalidPublicKeyReason),

    /// The `host_signature.value` field is not a valid 64-byte
    /// Ed25519 signature blob. The reason is structured.
    #[error("invalid host_signature.value: {0}")]
    InvalidSignatureEncoding(#[source] InvalidSignatureEncodingReason),

    /// Canonicalization, public-key parsing, and signature parsing
    /// all succeeded, but the signature does not verify over the
    /// canonical bytes. This is the variant that proves either
    /// tampering or a host-identity mismatch.
    #[error("signature does not verify over canonical bytes")]
    SignatureMismatch,

    /// The manifest's canonical form could not be produced. The
    /// underlying [`CanonicalError`] is preserved for diagnostics.
    #[error("canonicalization failed: {0}")]
    CanonicalizationFailed(#[from] CanonicalError),
}
