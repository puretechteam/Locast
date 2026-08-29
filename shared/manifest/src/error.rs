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
