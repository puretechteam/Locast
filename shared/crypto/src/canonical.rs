//! Canonical MessagePack encoder.
//!
//! v1 protocol messages are field-name-tagged structs (e.g.
//! `HelloPayload`, `WelcomePayload`). `rmp-serde` serializes struct
//! fields in declaration order, which is deterministic for a given
//! Rust source file, so the canonical encoder is not required for
//! the current message set. The canonical encoder is reserved for
//! future map-typed payloads (e.g. capability maps, room metadata)
//! where key ordering is part of the signed scope (§18.9).
//!
//! When a full canonical encoder is needed, the recommended
//! approach is a `serde::ser::Serializer` wrapper around
//! `rmp_serde::Serializer` that on `serialize_map` collects all
//! key/value pairs into a `BTreeMap` and writes them in sorted
//! order. That implementation is non-trivial and is not
//! required for P2-T02; this stub forwards to `rmp-serde` so
//! callers can already say `to_canonical_msgpack(&value)`.
//!
//! See `docs/ARCHITECTURE.md` section 18.9 for the canonical
//! signing rules.

#![forbid(unsafe_code)]

use serde::Serialize;

/// Encode `value` as canonical MessagePack. For the v1 protocol
/// (field-name-tagged structs), this is identical to plain
/// `rmp_serde::to_vec`; the function exists so that call sites
/// that will eventually need canonical encoding can be written
/// against a stable name.
///
/// Returns the encoded byte vector or an `rmp_serde::encode::Error`.
pub fn to_canonical_msgpack<T: Serialize>(value: &T) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Sample {
        a: u32,
        b: String,
        c: bool,
    }

    #[test]
    fn encodes_to_non_empty_bytes() {
        let s = Sample {
            a: 42,
            b: "hello".to_string(),
            c: true,
        };
        let bytes = to_canonical_msgpack(&s).expect("encode");
        assert!(!bytes.is_empty());
    }

    /// The encoded form is the same as `rmp_serde::to_vec` for
    /// struct types, which is the v1 contract.
    #[test]
    fn matches_rmp_serde_for_struct() {
        let s = Sample {
            a: 1,
            b: "b".to_string(),
            c: false,
        };
        let via_canonical = to_canonical_msgpack(&s).expect("encode canonical");
        let via_rmp = rmp_serde::to_vec(&s).expect("encode rmp");
        assert_eq!(via_canonical, via_rmp);
    }
}
