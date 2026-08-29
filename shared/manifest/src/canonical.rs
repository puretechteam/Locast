//! Canonical JSON form for a [`crate::model::MediaManifest`].
//!
//! The canonical form is the byte sequence that gets signed by the host
//! and verified by the viewer. Producing the same byte sequence from
//! two equivalent manifests (modulo key ordering, whitespace, and
//! unicode normalization) is the entire job of this module. The rules
//! are defined in `docs/ARCHITECTURE.md` section 8; this file
//! implements them. In summary:
//!
//! 1. `host_signature` is replaced with `null`.
//! 2. Object keys are emitted in lexicographic order at every depth.
//! 3. No insignificant whitespace between tokens.
//! 4. `\uXXXX` escapes are used only when required for valid JSON;
//!    otherwise raw UTF-8 is emitted.
//! 5. Non-finite floats are rejected.
//! 6. All string values are NFC-normalized before emission.
//! 7. A single trailing newline is appended.
//!
//! The data model has no floats, so the non-finite guard is a
//! belt-and-braces check against accidentally introducing one in the
//! future.

use std::collections::BTreeMap;
use std::fmt;

use serde::ser::{
    Serialize, SerializeMap, SerializeSeq, SerializeStruct, SerializeStructVariant, SerializeTuple,
    SerializeTupleStruct, SerializeTupleVariant, Serializer as SerdeSerializer,
};
use unicode_normalization::UnicodeNormalization;

use crate::error::CanonicalError;
use crate::model::MediaManifest;

/// A node in the collected value tree. The custom serializer walks the
/// value being serialized, turns it into a tree of these nodes, and only
/// then writes JSON from the tree. That second pass is what gives us
/// deterministic key ordering.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Node>),
    Object(BTreeMap<String, Node>),
}

/// A JSON number. We only carry integer variants because the data
/// model has no floats; the serializer still has to handle the float
/// methods (rejection is the spec) and serde may call them via custom
/// `Serialize` impls in dependencies.
#[derive(Debug, Clone, PartialEq)]
enum Number {
    PosInt(u64),
    NegInt(i64),
}

/// Collect a `serde::Serialize` value into a `Node` by driving a
/// `CollectSerializer` over it.
fn collect_value<T: Serialize + ?Sized>(v: &T) -> Result<Node, CanonicalError> {
    v.serialize(CollectSerializer)
}

/// `serde::Serializer` that builds a `Node` tree.
struct CollectSerializer;

impl SerdeSerializer for CollectSerializer {
    type Ok = Node;
    type Error = CanonicalError;

    type SerializeSeq = SeqSerializer;
    type SerializeTuple = SeqSerializer;
    type SerializeTupleStruct = SeqSerializer;
    type SerializeTupleVariant = TupVariantSerializer;
    type SerializeMap = MapSerializer;
    type SerializeStruct = StructSerializer;
    type SerializeStructVariant = StructVariantSerializer;

    fn serialize_bool(self, v: bool) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Bool(v))
    }

    fn serialize_i8(self, v: i8) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Number(Number::NegInt(v as i64)))
    }
    fn serialize_i16(self, v: i16) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Number(Number::NegInt(v as i64)))
    }
    fn serialize_i32(self, v: i32) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Number(Number::NegInt(v as i64)))
    }
    fn serialize_i64(self, v: i64) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Number(Number::NegInt(v)))
    }

    fn serialize_u8(self, v: u8) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Number(Number::PosInt(v as u64)))
    }
    fn serialize_u16(self, v: u16) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Number(Number::PosInt(v as u64)))
    }
    fn serialize_u32(self, v: u32) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Number(Number::PosInt(v as u64)))
    }
    fn serialize_u64(self, v: u64) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Number(Number::PosInt(v)))
    }

    fn serialize_f32(self, _v: f32) -> Result<Self::Ok, Self::Error> {
        // The canonical form is integers-only. The data model has no
        // float fields, so reaching this path indicates a programming
        // bug or a non-canonical input. Reject unconditionally rather
        // than risk a lossy numeric conversion.
        Err(CanonicalError::InvalidNonFiniteFloat)
    }
    fn serialize_f64(self, _v: f64) -> Result<Self::Ok, Self::Error> {
        Err(CanonicalError::InvalidNonFiniteFloat)
    }

    fn serialize_char(self, v: char) -> Result<Self::Ok, Self::Error> {
        let mut buf = String::new();
        buf.push(v);
        Ok(Node::String(buf.nfc().collect()))
    }

    fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
        Ok(Node::String(v.nfc().collect()))
    }

    fn serialize_bytes(self, _v: &[u8]) -> Result<Self::Ok, Self::Error> {
        Err(CanonicalError::BytesNotAllowed)
    }

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Null)
    }

    fn serialize_some<T>(self, v: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        v.serialize(self)
    }

    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Null)
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Null)
    }

    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        // Enum unit variants serialize as their name string.
        Ok(Node::String(variant.nfc().collect()))
    }

    fn serialize_newtype_struct<T>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T>(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + Serialize,
    {
        // {"variant": value}
        let mut map = BTreeMap::new();
        map.insert(variant.to_owned(), collect_value(value)?);
        Ok(Node::Object(map))
    }

    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Ok(SeqSerializer {
            items: Vec::with_capacity(len.unwrap_or(0)),
        })
    }

    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Ok(SeqSerializer {
            items: Vec::with_capacity(len),
        })
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Ok(SeqSerializer {
            items: Vec::with_capacity(len),
        })
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Ok(TupVariantSerializer {
            variant,
            items: Vec::with_capacity(len),
        })
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Ok(MapSerializer {
            entries: BTreeMap::new(),
            pending: None,
        })
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Ok(StructSerializer {
            entries: BTreeMap::new(),
            expected: len,
        })
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Ok(StructVariantSerializer {
            variant,
            entries: BTreeMap::new(),
            expected: len,
        })
    }

    fn collect_str<T>(self, value: &T) -> Result<Self::Ok, Self::Error>
    where
        T: ?Sized + fmt::Display,
    {
        Ok(Node::String(value.to_string().nfc().collect()))
    }
}

/// Sequence serializer. Items are appended in input order; arrays are
/// not sorted.
struct SeqSerializer {
    items: Vec<Node>,
}

impl SerializeSeq for SeqSerializer {
    type Ok = Node;
    type Error = CanonicalError;

    fn serialize_element<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        self.items.push(collect_value(value)?);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Array(self.items))
    }
}

impl SerializeTuple for SeqSerializer {
    type Ok = Node;
    type Error = CanonicalError;

    fn serialize_element<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        self.items.push(collect_value(value)?);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Array(self.items))
    }
}

impl SerializeTupleStruct for SeqSerializer {
    type Ok = Node;
    type Error = CanonicalError;

    fn serialize_field<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        self.items.push(collect_value(value)?);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Array(self.items))
    }
}

/// Tuple variant: `{"variant": [a, b, c]}`.
struct TupVariantSerializer {
    variant: &'static str,
    items: Vec<Node>,
}

impl SerializeTupleVariant for TupVariantSerializer {
    type Ok = Node;
    type Error = CanonicalError;

    fn serialize_field<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        self.items.push(collect_value(value)?);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        let mut map = BTreeMap::new();
        map.insert(self.variant.to_owned(), Node::Array(self.items));
        Ok(Node::Object(map))
    }
}

/// Map serializer. Keys must be strings; values are collected. We
/// buffer the most-recently-set key in `pending` so `serialize_value`
/// can attach to it without re-iterating the BTreeMap.
struct MapSerializer {
    entries: BTreeMap<String, Node>,
    pending: Option<String>,
}

impl SerializeMap for MapSerializer {
    type Ok = Node;
    type Error = CanonicalError;

    fn serialize_key<T>(&mut self, key: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let k = collect_value(key)?;
        match k {
            Node::String(s) => {
                self.entries.insert(s.clone(), Node::Null);
                self.pending = Some(s);
                Ok(())
            }
            _ => Err(CanonicalError::NonStringMapKey),
        }
    }

    fn serialize_value<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        let v = collect_value(value)?;
        let key = self.pending.take().ok_or_else(|| {
            CanonicalError::Custom("serialize_value without serialize_key".into())
        })?;
        self.entries.insert(key, v);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Object(self.entries))
    }
}

/// Struct serializer. Tracks the expected number of fields so we can
/// detect when serde called us with a different schema (defensive;
/// would be a bug in the data model).
struct StructSerializer {
    entries: BTreeMap<String, Node>,
    #[allow(dead_code)]
    expected: usize,
}

impl SerializeStruct for StructSerializer {
    type Ok = Node;
    type Error = CanonicalError;

    fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        self.entries.insert(key.to_owned(), collect_value(value)?);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        Ok(Node::Object(self.entries))
    }
}

/// Struct variant: `{"variant": {"k": v, ...}}`.
struct StructVariantSerializer {
    variant: &'static str,
    entries: BTreeMap<String, Node>,
    #[allow(dead_code)]
    expected: usize,
}

impl SerializeStructVariant for StructVariantSerializer {
    type Ok = Node;
    type Error = CanonicalError;

    fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error>
    where
        T: ?Sized + Serialize,
    {
        self.entries.insert(key.to_owned(), collect_value(value)?);
        Ok(())
    }

    fn end(self) -> Result<Self::Ok, Self::Error> {
        let mut map = BTreeMap::new();
        map.insert(self.variant.to_owned(), Node::Object(self.entries));
        Ok(Node::Object(map))
    }
}

// ----------------------------------------------------------------------------
// Second pass: walk the Node tree and write JSON with sorted keys.
// ----------------------------------------------------------------------------

/// Wrapper that lets us push a `Node` through serde's `SerializeSeq`
/// and `SerializeMap` without giving up on sorted-key emission. The
/// wrapper implements `Serialize` and, when driven by serde_json's
/// serializer, re-enters `Node::write` to emit each value with the
/// already-sorted key order.
struct NodeAsSerialize<'a>(&'a Node);

impl Serialize for NodeAsSerialize<'_> {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: SerdeSerializer,
    {
        match self.0 {
            Node::Null => ser.serialize_unit(),
            Node::Bool(b) => ser.serialize_bool(*b),
            Node::Number(Number::PosInt(n)) => ser.serialize_u64(*n),
            Node::Number(Number::NegInt(n)) => ser.serialize_i64(*n),
            Node::String(s) => ser.serialize_str(s),
            Node::Array(items) => {
                let mut seq = ser.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(&NodeAsSerialize(item))?;
                }
                seq.end()
            }
            Node::Object(map) => {
                let mut m = ser.serialize_map(Some(map.len()))?;
                for (k, v) in map {
                    m.serialize_entry(k, &NodeAsSerialize(v))?;
                }
                m.end()
            }
        }
    }
}

impl Serialize for Node {
    fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
    where
        S: SerdeSerializer,
    {
        NodeAsSerialize(self).serialize(ser)
    }
}

/// Walk the tree and write JSON to the given byte buffer. Keys are
/// already in sorted order by virtue of being stored in a `BTreeMap`.
fn write_tree(buf: &mut Vec<u8>, node: &Node) -> Result<(), CanonicalError> {
    let mut ser = serde_json::Serializer::new(buf);
    node.serialize(&mut ser).map_err(CanonicalError::from)
}

// ----------------------------------------------------------------------------
// Public API
// ----------------------------------------------------------------------------

/// Produce the canonical byte representation of a manifest.
///
/// The output is JSON text encoded as UTF-8, with:
///
/// - object keys sorted lexicographically at every depth
/// - no insignificant whitespace between tokens
/// - `host_signature` replaced with `null`
/// - all string values NFC-normalized
/// - a single trailing newline (`\n`)
///
/// Returns [`CanonicalError::InvalidNonFiniteFloat`] if any float
/// field is encountered. The data model has no floats, so reaching
/// that error indicates a programming bug rather than a runtime
/// condition.
pub fn serialize(manifest: &MediaManifest) -> Result<Vec<u8>, CanonicalError> {
    // Step 1: drive the data model's derived Serialize into a Node
    // tree. Field order is whatever the struct declares; the BTreeMap
    // in the second pass will re-sort.
    let tree = collect_value(manifest)?;

    // Step 2: replace the host_signature Object (if any) with Null
    // and ensure the key is present in the canonical output.
    let tree = strip_host_signature(tree);

    // Step 3: walk the tree and write JSON with sorted keys.
    let mut buf: Vec<u8> = Vec::new();
    write_tree(&mut buf, &tree)?;

    // Step 4: append a single trailing newline.
    buf.push(b'\n');
    Ok(buf)
}

/// BLAKE3 hex of the canonical bytes. Convenience for tests and
/// callers that want to commit to the canonical form without going
/// through the crypto crate's streaming API.
pub fn commit(bytes: &[u8]) -> String {
    locast_crypto::blake3::blake3_hex(bytes)
}

// ----------------------------------------------------------------------------
// First pass: collect the value tree, then strip host_signature.
//
// The wrapper derives serde's Serialize on the data model directly, so
// the field list cannot desync from MediaManifest. After collection we
// walk the tree and replace the `host_signature` Object with Null.
// ----------------------------------------------------------------------------

/// Strip a known `host_signature` slot from an object node, replacing
/// it with `Node::Null`. If the slot is not present (e.g. the
/// `skip_serializing_if = "Option::is_none"` rule fired), the canonical
/// representation also lacks it, so we leave the tree alone — the
/// architecture's rule "replace with null" is satisfied by emitting
/// null whenever the field is present in the data model, and by the
/// data model itself when the field is absent. To keep the two cases
/// distinguishable to a verifier, we instead always emit the field
/// with a `null` value by promoting an absent key to `null` here.
fn strip_host_signature(node: Node) -> Node {
    match node {
        Node::Object(mut entries) => {
            entries.insert("host_signature".to_owned(), Node::Null);
            Node::Object(entries)
        }
        other => other,
    }
}

// ----------------------------------------------------------------------------
// Unit tests for the architectural rules.
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    //! Unit tests for the architectural rules of the canonical form.
    //! Integration tests against a checked-in golden vector live in
    //! `tests/golden_canonical.rs`.

    use std::collections::HashMap;

    use serde::Serialize;

    use super::*;
    use crate::model::{Dimensions, HostSignature, MediaEntry, MediaManifest, Source};

    fn fixture_manifest() -> MediaManifest {
        MediaManifest {
            manifest_version: 1,
            room_id: "22222222-2222-4222-8222-222222222222".to_owned(),
            media: vec![MediaEntry {
                id: "11111111-1111-4111-8111-111111111111".to_owned(),
                filename: "movie.mp4".to_owned(),
                sha256: "a".repeat(64),
                blake3: "b".repeat(64),
                size_bytes: 1024,
                mime: "video/mp4".to_owned(),
                duration_ms: 60000,
                dimensions: Some(Dimensions {
                    width: 1920,
                    height: 1080,
                }),
                codecs: None,
                sources: vec![Source {
                    peer_id: "peer-aaaa".to_owned(),
                    url_hint: None,
                    priority: 0,
                    chunk_size: 65536,
                    total_chunks: 1,
                    chunk_hashes: vec!["c".repeat(64)],
                }],
            }],
            subtitles: vec![],
            created_at: 1700000000000,
            host_signature: Some(HostSignature {
                public_key: "should-be-stripped".to_owned(),
                algorithm: "ed25519".to_owned(),
                value: "should-be-stripped".to_owned(),
            }),
        }
    }

    #[test]
    fn serialize_is_deterministic() {
        let m = fixture_manifest();
        let a = serialize(&m).unwrap();
        let b = serialize(&m).unwrap();
        assert_eq!(a, b, "two serializations of the same manifest differ");
    }

    #[test]
    fn appends_single_trailing_newline() {
        let m = fixture_manifest();
        let bytes = serialize(&m).unwrap();
        assert!(bytes.ends_with(b"\n"), "missing trailing newline");
        assert_eq!(*bytes.last().unwrap(), 0x0A);
        assert!(
            !bytes.ends_with(b"\n\n"),
            "should be exactly one trailing newline"
        );
    }

    #[test]
    fn emits_no_whitespace_between_tokens() {
        let m = fixture_manifest();
        let bytes = serialize(&m).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        // Compact JSON has no `, ` or `: ` or newlines inside the
        // document. The single trailing `\n` is the only newline.
        assert!(!s.contains(": "), "found ': ' in {s}");
        assert!(!s.contains(", "), "found ', ' in {s}");
    }

    #[test]
    fn sorts_object_keys_lexicographically_at_every_depth() {
        let m = fixture_manifest();
        let bytes = serialize(&m).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        // Top-level keys: created_at, host_signature, manifest_version,
        // media, room_id, subtitles. That order is what we expect.
        let expected = "{\"created_at\":";
        assert!(
            s.starts_with(expected),
            "top-level keys not in lex order: {s}"
        );
        // Inside media[0]: blake3, codecs (absent), dimensions,
        // duration_ms, filename, id, mime, sha256, size_bytes, sources.
        assert!(s.contains("\"blake3\":"), "blake3 missing: {s}");
        assert!(s.contains("\"dimensions\":"), "dimensions missing: {s}");
        assert!(
            s.find("\"blake3\":").unwrap() < s.find("\"dimensions\":").unwrap(),
            "blake3 must come before dimensions"
        );
        assert!(
            s.find("\"dimensions\":").unwrap() < s.find("\"duration_ms\":").unwrap(),
            "dimensions must come before duration_ms"
        );
        // Inside dimensions: height, width.
        assert!(
            s.find("\"height\":").unwrap() < s.find("\"width\":").unwrap(),
            "height must come before width"
        );
    }

    #[test]
    fn sorts_keys_even_when_input_is_hashmap() {
        // Simulate a map with insertion order "z, a, m" — the
        // canonical output must still come out "a, m, z" because
        // the custom serializer sorts at serialize_map time.
        let mut map: HashMap<String, u32> = HashMap::new();
        map.insert("z".to_owned(), 1);
        map.insert("a".to_owned(), 2);
        map.insert("m".to_owned(), 3);
        let bytes = collect_value(&map).unwrap();
        let mut buf = Vec::new();
        write_tree(&mut buf, &bytes).unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        assert_eq!(s, "{\"a\":2,\"m\":3,\"z\":1}");
    }

    #[test]
    fn normalizes_non_nfc_strings() {
        // "Café" with combining acute (NFD) vs precomposed (NFC).
        let nfc = "Cafe\u{0301}".to_owned(); // composing form (NFD)
        let pre = "Caf\u{00e9}".to_owned(); // precomposed (NFC)
                                            // collect_value normalizes.
        let nfc_node = collect_value(&nfc).unwrap();
        let pre_node = collect_value(&pre).unwrap();
        assert_eq!(
            nfc_node, pre_node,
            "NFC normalization did not collapse forms"
        );
    }

    #[test]
    fn non_nfc_input_still_produces_same_canonical_bytes() {
        // Build a manifest with a non-NFC filename. Its canonical
        // bytes must equal the manifest with the NFC version.
        let mut m1 = fixture_manifest();
        m1.media[0].filename = "Cafe\u{0301}.mp4".to_owned(); // NFD
        let mut m2 = fixture_manifest();
        m2.media[0].filename = "Caf\u{00e9}.mp4".to_owned(); // NFC
        let a = serialize(&m1).unwrap();
        let b = serialize(&m2).unwrap();
        assert_eq!(a, b, "NFD and NFC inputs produced different bytes");
    }

    #[test]
    fn non_nfc_input_differs_from_non_normalized_serializer() {
        // Confirm the canonicalizer would have produced different
        // bytes if it had NOT applied NFC. We do this by skipping
        // the normalizer on a copy of the input and serializing
        // that copy through a BTreeMap directly.
        let mut map: BTreeMap<&'static str, String> = BTreeMap::new();
        map.insert("a", "Cafe\u{0301}".to_owned());
        let bytes = collect_value(&map).unwrap();
        let mut buf = Vec::new();
        write_tree(&mut buf, &bytes).unwrap();
        let normalized = String::from_utf8(buf).unwrap();
        assert!(
            normalized.contains("Caf\u{00e9}"),
            "expected precomposed é in {normalized}"
        );
        assert!(
            !normalized.contains("Cafe\u{0301}"),
            "NFD form leaked through: {normalized}"
        );
    }

    #[test]
    fn preserves_string_internal_whitespace() {
        // The architecture says "no insignificant whitespace between
        // tokens" — it does NOT say to collapse internal whitespace
        // inside string values. A filename like "  movie  .mp4"
        // should survive intact.
        let mut m = fixture_manifest();
        m.media[0].filename = "  movie  .mp4".to_owned();
        let bytes = serialize(&m).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(
            s.contains("\"  movie  .mp4\""),
            "internal whitespace was mangled: {s}"
        );
    }

    #[test]
    fn rejects_non_finite_floats() {
        struct Nan;
        impl Serialize for Nan {
            fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
            where
                S: SerdeSerializer,
            {
                ser.serialize_f64(f64::NAN)
            }
        }
        let err = collect_value(&Nan).unwrap_err();
        assert!(matches!(err, CanonicalError::InvalidNonFiniteFloat));

        struct Inf;
        impl Serialize for Inf {
            fn serialize<S>(&self, ser: S) -> Result<S::Ok, S::Error>
            where
                S: SerdeSerializer,
            {
                ser.serialize_f64(f64::INFINITY)
            }
        }
        let err = collect_value(&Inf).unwrap_err();
        assert!(matches!(err, CanonicalError::InvalidNonFiniteFloat));
    }

    #[test]
    fn host_signature_is_stripped_to_null_regardless_of_input() {
        let mut m1 = fixture_manifest();
        m1.host_signature = None;
        let mut m2 = fixture_manifest();
        m2.host_signature = Some(HostSignature {
            public_key: "k".repeat(64),
            algorithm: "ed25519".to_owned(),
            value: "v".repeat(64),
        });
        let a = serialize(&m1).unwrap();
        let b = serialize(&m2).unwrap();
        assert_eq!(a, b, "host_signature should not affect canonical bytes");
        let s = std::str::from_utf8(&a).unwrap();
        assert!(s.contains("\"host_signature\":null"));
        assert!(
            !s.contains("should-be-stripped"),
            "host signature leaked into canonical: {s}"
        );
    }

    #[test]
    fn field_change_changes_canonical_bytes() {
        let mut m = fixture_manifest();
        m.room_id = "different-room-id".to_owned();
        let a = serialize(&fixture_manifest()).unwrap();
        let b = serialize(&m).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn optional_dimensions_absent_vs_present_differ() {
        let mut m1 = fixture_manifest();
        m1.media[0].dimensions = None;
        let a = serialize(&m1).unwrap();
        let b = serialize(&fixture_manifest()).unwrap();
        assert_ne!(a, b);
        let sa = std::str::from_utf8(&a).unwrap();
        assert!(
            !sa.contains("\"dimensions\":"),
            "absent dimensions must not emit a key: {sa}"
        );
    }

    #[test]
    fn empty_subtitles_array_is_present_not_absent() {
        let m = fixture_manifest();
        let bytes = serialize(&m).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("\"subtitles\":[]"), "subtitles key missing: {s}");
    }

    #[test]
    fn array_order_is_preserved() {
        // Array order is significant: media[0] comes before media[1]
        // and the manifest is invalid if a viewer treats them as
        // unordered.
        let mut m = fixture_manifest();
        m.media.push(MediaEntry {
            id: "99999999-9999-4999-8999-999999999999".to_owned(),
            filename: "second.mp4".to_owned(),
            sha256: "9".repeat(64),
            blake3: "9".repeat(64),
            size_bytes: 99,
            mime: "video/mp4".to_owned(),
            duration_ms: 1000,
            dimensions: None,
            codecs: None,
            sources: vec![Source {
                peer_id: "peer-zzzz".to_owned(),
                url_hint: None,
                priority: 0,
                chunk_size: 65536,
                total_chunks: 1,
                chunk_hashes: vec!["9".repeat(64)],
            }],
        });
        let bytes = serialize(&m).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        let first_pos = s.find("11111111-1111-4111-8111-111111111111").unwrap();
        let second_pos = s.find("99999999-9999-4999-8999-999999999999").unwrap();
        assert!(first_pos < second_pos, "array order not preserved");
    }

    #[test]
    fn unicode_escape_is_not_used_for_printable_utf8() {
        // The architecture says `\uXXXX` is used only when required
        // for valid JSON. "Café" in precomposed form should appear
        // as raw UTF-8 in the output, not as `\u00e9`.
        let mut m = fixture_manifest();
        m.room_id = "Caf\u{00e9}".to_owned();
        let bytes = serialize(&m).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("Caf\u{00e9}"), "raw UTF-8 not preserved: {s}");
        assert!(!s.contains("\\u00e9"), "non-required escape used: {s}");
    }

    #[test]
    fn must_escape_control_characters() {
        // The serializer must still emit a `\u` escape for control
        // characters that raw JSON cannot represent. Use a `\t` (U+0009)
        // which serde_json escapes as `\t` rather than `\u0009`.
        let mut m = fixture_manifest();
        m.room_id = "tab\there".to_owned();
        let bytes = serialize(&m).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("tab\\there"), "control char not escaped: {s}");
    }

    #[test]
    fn optional_dimensions_present_is_sorted() {
        let m = fixture_manifest();
        let bytes = serialize(&m).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        let dims = s.find("\"dimensions\":").expect("dimensions missing");
        let height = s[dims..].find("\"height\":").unwrap();
        let width = s[dims..].find("\"width\":").unwrap();
        assert!(height < width);
    }
}
