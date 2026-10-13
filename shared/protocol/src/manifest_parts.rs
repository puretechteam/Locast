//! Chunked transport for manifests that do not fit one signaling frame.
//!
//! A signed manifest lists one SHA-256 per 256 KiB chunk, so it grows with the
//! media it describes: about 67 bytes per chunk, 13.7 MB for a 50 GiB file.
//! The signaling socket caps a frame at 1 MiB, so such a manifest travels as a
//! series of `MANIFEST_PART` envelopes instead of one `MANIFEST_PUBLISH`,
//! `MANIFEST_PUBLISHED` or `MANIFEST_RESPONSE`.
//!
//! The pieces are slices of ONE serialized, signed [`MediaManifest`]. Nothing is
//! signed per piece: the receiver reassembles the exact bytes, checks the
//! length and digest every piece repeats, parses the whole manifest, and hands
//! it to the same verification the single-frame path uses (the host signature,
//! the room binding and the trust anchor). A partial manifest is never
//! returned.
//!
//! Rules the [`PartAssembler`] enforces, so untrusted input cannot grow memory
//! without bound or be half-accepted:
//!
//! - pieces of one assembly arrive in order (the transport is ordered, so a gap
//!   means a lost piece); a gap, a conflicting duplicate or a header that
//!   changes mid-assembly drops the assembly, and the sender starts again with
//!   a fresh `assembly_id`;
//! - an identical duplicate of a piece already taken is ignored;
//! - the declared size, the piece count and each piece are capped, the bytes
//!   held are charged to a shared [`ByteBudget`], the number of assemblies in
//!   flight is capped, and an assembly that stops making progress expires.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use locast_manifest::MediaManifest;
use serde::{Deserialize, Serialize};
use ts_rs::TS;
use uuid::Uuid;

/// The most raw manifest bytes one piece carries (before base64). A piece
/// is ~683 KiB as base64, which fits the default 1 MiB frame with room for the
/// envelope and bearer.
pub const MAX_PART_BYTES: usize = 512 * 1024;

/// The most serialized manifest bytes an assembly may declare: ~490,000 chunks,
/// about 120 GiB of media at 256 KiB per chunk, more than twice the default
/// 50 GiB media quota.
pub const MAX_ASSEMBLED_BYTES: usize = 32 * 1024 * 1024;

/// The most pieces one assembly may declare.
pub const MAX_PARTS: u32 = 1024;

/// A manifest whose serialized form is at most this many bytes still travels
/// as one legacy `MANIFEST_PUBLISH` / `MANIFEST_PUBLISHED` /
/// `MANIFEST_RESPONSE` frame (comfortably inside the default 1 MiB frame cap),
/// so small manifests behave exactly as before and older peers keep working.
pub const SINGLE_FRAME_MANIFEST_BYTES: usize = 640 * 1024;

/// Headroom left in a frame for the envelope, the bearer and base64's
/// expansion when choosing a piece size from a frame cap.
const FRAME_OVERHEAD_BYTES: usize = 4096;

/// What a reassembled manifest is, and so which single-frame message it stands
/// in for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub enum ManifestPartKind {
    /// Host to server: stands in for `MANIFEST_PUBLISH`.
    #[serde(rename = "publish")]
    Publish,
    /// Server to the room: stands in for `MANIFEST_PUBLISHED`.
    #[serde(rename = "published")]
    Published,
    /// Server to one requester: stands in for `MANIFEST_RESPONSE`.
    #[serde(rename = "response")]
    Response,
}

/// `MANIFEST_PART`: one piece of a manifest sent in several frames. Every
/// piece repeats the header, so a receiver can check that all pieces agree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct ManifestPartPayload {
    /// Groups the pieces of one logical manifest; a fresh value per attempt.
    pub assembly_id: Uuid,
    /// Which single-frame message the reassembled manifest stands in for.
    pub kind: ManifestPartKind,
    /// Zero-based position of this piece.
    pub index: u32,
    /// How many pieces the assembly has.
    pub total: u32,
    /// Exact length in bytes of the serialized manifest.
    pub total_bytes: u32,
    /// SHA-256 (lowercase hex) of the serialized manifest.
    pub digest: String,
    /// Server-assigned manifest version (`published` / `response`; `0` for
    /// `publish`).
    pub version: i64,
    /// Server publication time, unix ms (`published` / `response`; `0` for
    /// `publish`).
    pub published_at_ms: i64,
    /// This piece's slice of the serialized manifest, standard base64.
    pub data: String,
}

/// Why a piece or an assembly was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PartError {
    #[error("manifest piece header is invalid: {0}")]
    BadHeader(&'static str),
    #[error("manifest is too large to send ({bytes} bytes, limit {limit})")]
    TooLarge { bytes: usize, limit: usize },
    #[error("manifest piece {got} arrived where {expected} was expected")]
    OutOfOrder { expected: u32, got: u32 },
    #[error("manifest piece {0} repeats with different content")]
    Conflict(u32),
    #[error("manifest piece {0} is not valid base64 or is too large")]
    BadPiece(u32),
    #[error("manifest pieces do not add up to the declared length")]
    LengthMismatch,
    #[error("manifest pieces do not match their digest")]
    DigestMismatch,
    #[error("reassembled manifest is not a valid manifest: {0}")]
    BadManifest(String),
    #[error("too many manifest bytes are being assembled at once")]
    BudgetExhausted,
    #[error("frame cap {cap} is too small to carry manifest pieces")]
    FrameCapTooSmall { cap: usize },
}

/// A shared cap on the bytes all assemblies together may hold.
#[derive(Debug, Clone)]
pub struct ByteBudget {
    used: Arc<AtomicUsize>,
    cap: usize,
}

impl ByteBudget {
    pub fn new(cap: usize) -> Self {
        Self {
            used: Arc::new(AtomicUsize::new(0)),
            cap,
        }
    }

    /// Bytes currently reserved.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::SeqCst)
    }

    fn reserve(&self, bytes: usize) -> Result<Reservation, PartError> {
        let mut cur = self.used.load(Ordering::SeqCst);
        loop {
            let next = cur
                .checked_add(bytes)
                .filter(|n| *n <= self.cap)
                .ok_or(PartError::BudgetExhausted)?;
            match self
                .used
                .compare_exchange(cur, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => {
                    return Ok(Reservation {
                        used: self.used.clone(),
                        bytes,
                    })
                }
                Err(seen) => cur = seen,
            }
        }
    }
}

#[derive(Debug)]
struct Reservation {
    used: Arc<AtomicUsize>,
    bytes: usize,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

/// The serialized form a manifest is split from and reassembled into.
pub fn encode_manifest(manifest: &MediaManifest) -> Result<Vec<u8>, PartError> {
    let bytes = serde_json::to_vec(manifest).map_err(|e| PartError::BadManifest(e.to_string()))?;
    if bytes.len() > MAX_ASSEMBLED_BYTES {
        return Err(PartError::TooLarge {
            bytes: bytes.len(),
            limit: MAX_ASSEMBLED_BYTES,
        });
    }
    Ok(bytes)
}

/// The raw piece size to use for a connection whose frame cap is `frame_cap`
/// bytes: [`MAX_PART_BYTES`], or less when the cap is small.
pub fn part_size_for_frame_cap(frame_cap: usize) -> Result<usize, PartError> {
    let room = frame_cap.saturating_sub(FRAME_OVERHEAD_BYTES);
    let size = (room / 4 * 3).min(MAX_PART_BYTES);
    // MAX_ASSEMBLED_BYTES must still fit in MAX_PARTS pieces.
    if size < MAX_ASSEMBLED_BYTES.div_ceil(MAX_PARTS as usize) {
        return Err(PartError::FrameCapTooSmall { cap: frame_cap });
    }
    Ok(size)
}

/// Split serialized manifest bytes into pieces of at most `part_size` raw
/// bytes.
pub fn split_manifest_bytes(
    bytes: &[u8],
    kind: ManifestPartKind,
    version: i64,
    published_at_ms: i64,
    assembly_id: Uuid,
    part_size: usize,
) -> Result<Vec<ManifestPartPayload>, PartError> {
    use locast_crypto::ed25519::to_base64;
    if bytes.is_empty() || part_size == 0 || part_size > MAX_PART_BYTES {
        return Err(PartError::BadHeader("empty manifest or bad piece size"));
    }
    if bytes.len() > MAX_ASSEMBLED_BYTES {
        return Err(PartError::TooLarge {
            bytes: bytes.len(),
            limit: MAX_ASSEMBLED_BYTES,
        });
    }
    let total = bytes.len().div_ceil(part_size);
    if total > MAX_PARTS as usize {
        return Err(PartError::TooLarge {
            bytes: bytes.len(),
            limit: part_size * MAX_PARTS as usize,
        });
    }
    let digest = locast_crypto::sha256::sha256_hex(bytes);
    Ok(bytes
        .chunks(part_size)
        .enumerate()
        .map(|(i, piece)| ManifestPartPayload {
            assembly_id,
            kind,
            index: i as u32,
            total: total as u32,
            total_bytes: bytes.len() as u32,
            digest: digest.clone(),
            version,
            published_at_ms,
            data: to_base64(piece),
        })
        .collect())
}

/// A manifest reassembled from pieces. It has been parsed but NOT verified:
/// the caller runs the host-signature, room and trust-anchor checks.
#[derive(Debug, Clone, PartialEq)]
pub struct AssembledManifest {
    pub room_id: Uuid,
    pub kind: ManifestPartKind,
    pub version: i64,
    pub published_at_ms: i64,
    pub manifest: MediaManifest,
}

#[derive(Debug)]
struct Entry {
    room_id: Uuid,
    assembly_id: Uuid,
    kind: ManifestPartKind,
    total: u32,
    total_bytes: usize,
    digest: String,
    version: i64,
    published_at_ms: i64,
    /// End offset in `buf` of each piece taken so far.
    ends: Vec<usize>,
    buf: Vec<u8>,
    last_activity: Instant,
    _reservation: Reservation,
}

/// Reassembles `MANIFEST_PART` pieces. One per connection.
#[derive(Debug)]
pub struct PartAssembler {
    max_concurrent: usize,
    ttl: Duration,
    budget: ByteBudget,
    entries: Vec<Entry>,
}

impl PartAssembler {
    /// `max_concurrent` assemblies at most, each expiring after `ttl` without
    /// a new piece, all charged to `budget`.
    pub fn new(max_concurrent: usize, ttl: Duration, budget: ByteBudget) -> Self {
        Self {
            max_concurrent: max_concurrent.max(1),
            ttl,
            budget,
            entries: Vec::new(),
        }
    }

    /// Assemblies in flight.
    pub fn in_flight(&self) -> usize {
        self.entries.len()
    }

    /// Drop every assembly in flight.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drop assemblies that made no progress for the ttl. Returns how many.
    pub fn expire(&mut self, now: Instant) -> usize {
        let before = self.entries.len();
        let ttl = self.ttl;
        self.entries
            .retain(|e| now.saturating_duration_since(e.last_activity) < ttl);
        before - self.entries.len()
    }

    /// Take one piece for `room_id`. `Ok(Some(_))` once the last piece
    /// completes a manifest; `Ok(None)` while more are expected. An error
    /// drops the assembly the piece belonged to.
    pub fn push(
        &mut self,
        room_id: Uuid,
        part: &ManifestPartPayload,
        now: Instant,
    ) -> Result<Option<AssembledManifest>, PartError> {
        use locast_crypto::ed25519::from_base64;
        self.expire(now);
        validate_header(part)?;
        // Decode first: the base64 length bound keeps this small, and a bad
        // piece must not allocate an assembly.
        if part.data.len() > MAX_PART_BYTES.div_ceil(3) * 4 {
            self.drop_assembly(room_id, part.assembly_id);
            return Err(PartError::BadPiece(part.index));
        }
        let piece = from_base64(&part.data).map_err(|_| {
            self.drop_assembly(room_id, part.assembly_id);
            PartError::BadPiece(part.index)
        })?;
        if piece.len() > MAX_PART_BYTES {
            self.drop_assembly(room_id, part.assembly_id);
            return Err(PartError::BadPiece(part.index));
        }

        let pos = self
            .entries
            .iter()
            .position(|e| e.room_id == room_id && e.assembly_id == part.assembly_id);
        let pos = match pos {
            Some(p) => p,
            None => {
                if part.index != 0 {
                    // The start of this assembly was lost or expired.
                    return Err(PartError::OutOfOrder {
                        expected: 0,
                        got: part.index,
                    });
                }
                self.begin(room_id, part, now)?
            }
        };

        let result = self.take(pos, part, &piece, now);
        match result {
            Ok(None) => Ok(None),
            Ok(Some(done)) => {
                self.entries.swap_remove(pos);
                Ok(Some(done))
            }
            Err(e) => {
                self.entries.swap_remove(pos);
                Err(e)
            }
        }
    }

    fn drop_assembly(&mut self, room_id: Uuid, assembly_id: Uuid) {
        self.entries
            .retain(|e| !(e.room_id == room_id && e.assembly_id == assembly_id));
    }

    fn begin(
        &mut self,
        room_id: Uuid,
        part: &ManifestPartPayload,
        now: Instant,
    ) -> Result<usize, PartError> {
        // A newer manifest of the same kind in the same room replaces one still
        // being assembled: the older one can only be stale.
        self.entries
            .retain(|e| !(e.room_id == room_id && e.kind == part.kind));
        while self.entries.len() >= self.max_concurrent {
            let oldest = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_activity)
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.entries.swap_remove(oldest);
        }
        let reservation = self.budget.reserve(part.total_bytes as usize)?;
        self.entries.push(Entry {
            room_id,
            assembly_id: part.assembly_id,
            kind: part.kind,
            total: part.total,
            total_bytes: part.total_bytes as usize,
            digest: part.digest.clone(),
            version: part.version,
            published_at_ms: part.published_at_ms,
            ends: Vec::new(),
            buf: Vec::new(),
            last_activity: now,
            _reservation: reservation,
        });
        Ok(self.entries.len() - 1)
    }

    fn take(
        &mut self,
        pos: usize,
        part: &ManifestPartPayload,
        piece: &[u8],
        now: Instant,
    ) -> Result<Option<AssembledManifest>, PartError> {
        let e = &mut self.entries[pos];
        if e.kind != part.kind
            || e.total != part.total
            || e.total_bytes != part.total_bytes as usize
            || e.digest != part.digest
            || e.version != part.version
            || e.published_at_ms != part.published_at_ms
        {
            return Err(PartError::BadHeader("header changed within an assembly"));
        }
        let next = e.ends.len() as u32;
        if part.index < next {
            // A repeat of a piece already taken: fine if identical.
            let start = if part.index == 0 {
                0
            } else {
                e.ends[part.index as usize - 1]
            };
            let end = e.ends[part.index as usize];
            if &e.buf[start..end] == piece {
                return Ok(None);
            }
            return Err(PartError::Conflict(part.index));
        }
        if part.index != next {
            return Err(PartError::OutOfOrder {
                expected: next,
                got: part.index,
            });
        }
        if e.buf.len() + piece.len() > e.total_bytes {
            return Err(PartError::LengthMismatch);
        }
        e.buf.extend_from_slice(piece);
        e.ends.push(e.buf.len());
        e.last_activity = now;
        if part.index + 1 < e.total {
            return Ok(None);
        }
        // Last piece: the whole must be exactly what was declared.
        if e.buf.len() != e.total_bytes {
            return Err(PartError::LengthMismatch);
        }
        if locast_crypto::sha256::sha256_hex(&e.buf) != e.digest {
            return Err(PartError::DigestMismatch);
        }
        let manifest: MediaManifest = serde_json::from_slice(&e.buf)
            .map_err(|err| PartError::BadManifest(err.to_string()))?;
        Ok(Some(AssembledManifest {
            room_id: e.room_id,
            kind: e.kind,
            version: e.version,
            published_at_ms: e.published_at_ms,
            manifest,
        }))
    }
}

fn validate_header(part: &ManifestPartPayload) -> Result<(), PartError> {
    if part.total == 0 || part.total > MAX_PARTS {
        return Err(PartError::BadHeader("piece count out of range"));
    }
    if part.index >= part.total {
        return Err(PartError::BadHeader("piece index out of range"));
    }
    let bytes = part.total_bytes as usize;
    if bytes == 0 || bytes > MAX_ASSEMBLED_BYTES {
        return Err(PartError::TooLarge {
            bytes,
            limit: MAX_ASSEMBLED_BYTES,
        });
    }
    if bytes > part.total as usize * MAX_PART_BYTES || (part.total as usize) > bytes {
        return Err(PartError::BadHeader("piece count does not fit the length"));
    }
    if part.digest.len() != 64
        || !part
            .digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(PartError::BadHeader("digest is not 64 lowercase hex"));
    }
    if part.kind == ManifestPartKind::Publish && (part.version != 0 || part.published_at_ms != 0) {
        return Err(PartError::BadHeader("a publish carries no version"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use locast_manifest::{MediaEntry, Source};

    fn manifest(chunks: u32) -> MediaManifest {
        MediaManifest {
            manifest_version: 1,
            room_id: Uuid::nil().to_string(),
            media: vec![MediaEntry {
                id: "m".into(),
                filename: "film.mkv".into(),
                sha256: "ab".repeat(32),
                blake3: "cd".repeat(32),
                size_bytes: u64::from(chunks) * 262_144,
                mime: "video/x-matroska".into(),
                duration_ms: 10_800_000,
                dimensions: None,
                codecs: None,
                sources: vec![Source {
                    peer_id: "p".into(),
                    url_hint: None,
                    priority: 0,
                    chunk_size: 262_144,
                    total_chunks: chunks,
                    chunk_hashes: (0..chunks).map(|i| format!("{i:064x}")).collect(),
                }],
            }],
            subtitles: vec![],
            created_at: 1,
            host_signature: None,
        }
    }

    fn parts_of(m: &MediaManifest, size: usize) -> Vec<ManifestPartPayload> {
        let bytes = encode_manifest(m).unwrap();
        split_manifest_bytes(
            &bytes,
            ManifestPartKind::Published,
            3,
            99,
            Uuid::new_v4(),
            size,
        )
        .unwrap()
    }

    fn asm() -> PartAssembler {
        PartAssembler::new(2, Duration::from_secs(30), ByteBudget::new(64 << 20))
    }

    #[test]
    fn pieces_reassemble_to_the_same_manifest() {
        let m = manifest(20_000);
        let parts = parts_of(&m, MAX_PART_BYTES);
        assert!(parts.len() > 2);
        let mut a = asm();
        let room = Uuid::new_v4();
        let now = Instant::now();
        let mut done = None;
        for p in &parts {
            done = a.push(room, p, now).unwrap();
        }
        let done = done.expect("last piece completes");
        assert_eq!(done.manifest, m);
        assert_eq!(done.version, 3);
        assert_eq!(done.published_at_ms, 99);
        assert_eq!(done.room_id, room);
        assert_eq!(a.in_flight(), 0);
    }

    #[test]
    fn the_largest_supported_manifest_fits_the_piece_limits() {
        // 50 GiB default quota at 256 KiB chunks.
        let m = manifest(204_800);
        let bytes = encode_manifest(&m).unwrap();
        assert!(bytes.len() < MAX_ASSEMBLED_BYTES / 2, "{}", bytes.len());
        let parts = parts_of(&m, MAX_PART_BYTES);
        assert!(parts.len() <= MAX_PARTS as usize);
        // Every piece is well inside a 1 MiB frame.
        assert!(parts.iter().all(|p| p.data.len() < 800_000));
    }

    #[test]
    fn a_missing_piece_never_completes() {
        let parts = parts_of(&manifest(20_000), 100_000);
        let mut a = asm();
        let room = Uuid::new_v4();
        let now = Instant::now();
        a.push(room, &parts[0], now).unwrap();
        // Skip piece 1.
        let err = a.push(room, &parts[2], now).unwrap_err();
        assert!(matches!(
            err,
            PartError::OutOfOrder {
                expected: 1,
                got: 2
            }
        ));
        assert_eq!(a.in_flight(), 0, "the broken assembly is dropped");
    }

    #[test]
    fn an_identical_duplicate_is_ignored_and_a_conflicting_one_is_not() {
        let parts = parts_of(&manifest(20_000), 100_000);
        let room = Uuid::new_v4();
        let now = Instant::now();
        let mut a = asm();
        a.push(room, &parts[0], now).unwrap();
        a.push(room, &parts[1], now).unwrap();
        assert!(a.push(room, &parts[0], now).unwrap().is_none());
        assert_eq!(a.in_flight(), 1);
        let mut tampered = parts[1].clone();
        tampered.data = parts[0].data.clone();
        let err = a.push(room, &tampered, now).unwrap_err();
        assert_eq!(err, PartError::Conflict(1));
        assert_eq!(a.in_flight(), 0);
    }

    #[test]
    fn a_tampered_piece_fails_the_digest() {
        let mut parts = parts_of(&manifest(20_000), 100_000);
        let last = parts.len() - 1;
        // Swap two same-length slices' worth of content: replace piece 1 with a
        // different valid base64 of the same length.
        let mut raw = locast_crypto::ed25519::from_base64(&parts[1].data).unwrap();
        raw[10] ^= 1;
        parts[1].data = locast_crypto::ed25519::to_base64(&raw);
        let mut a = asm();
        let room = Uuid::new_v4();
        let now = Instant::now();
        for p in &parts[..last] {
            a.push(room, p, now).unwrap();
        }
        let err = a.push(room, &parts[last], now).unwrap_err();
        assert_eq!(err, PartError::DigestMismatch);
    }

    #[test]
    fn a_header_that_changes_mid_assembly_is_refused() {
        let parts = parts_of(&manifest(20_000), 100_000);
        let room = Uuid::new_v4();
        let now = Instant::now();
        let mut a = asm();
        a.push(room, &parts[0], now).unwrap();
        let mut other = parts[1].clone();
        other.version = 4;
        assert!(matches!(
            a.push(room, &other, now),
            Err(PartError::BadHeader(_))
        ));
        assert_eq!(a.in_flight(), 0);
    }

    #[test]
    fn oversized_and_abusive_headers_are_refused_before_any_allocation() {
        let mut a = asm();
        let room = Uuid::new_v4();
        let now = Instant::now();
        let base = parts_of(&manifest(1_000), 100_000).remove(0);
        for mutate in [
            |p: &mut ManifestPartPayload| p.total_bytes = (MAX_ASSEMBLED_BYTES + 1) as u32,
            |p: &mut ManifestPartPayload| p.total_bytes = 0,
            |p: &mut ManifestPartPayload| p.total = MAX_PARTS + 1,
            |p: &mut ManifestPartPayload| p.total = 0,
            |p: &mut ManifestPartPayload| p.index = p.total,
            |p: &mut ManifestPartPayload| p.digest = "zz".into(),
            |p: &mut ManifestPartPayload| p.data = "A".repeat(MAX_PART_BYTES * 2),
            |p: &mut ManifestPartPayload| p.data = "!!!not base64".into(),
            |p: &mut ManifestPartPayload| p.data = String::new(),
        ] {
            let mut p = base.clone();
            mutate(&mut p);
            assert!(a.push(room, &p, now).is_err());
            assert_eq!(a.in_flight(), 0);
        }
        assert_eq!(a.budget.used(), 0);
    }

    #[test]
    fn a_piece_that_overruns_the_declared_length_is_refused() {
        let parts = parts_of(&manifest(1_000), 100_000);
        let mut p0 = parts[0].clone();
        p0.total_bytes = 10; // declares 10 bytes, piece is 100_000
        let mut a = asm();
        assert!(a.push(Uuid::new_v4(), &p0, Instant::now()).is_err());
        assert_eq!(a.in_flight(), 0);
    }

    #[test]
    fn a_piece_for_an_assembly_that_never_started_is_refused() {
        let parts = parts_of(&manifest(20_000), 100_000);
        let mut a = asm();
        let err = a
            .push(Uuid::new_v4(), &parts[1], Instant::now())
            .unwrap_err();
        assert!(matches!(
            err,
            PartError::OutOfOrder {
                expected: 0,
                got: 1
            }
        ));
        assert_eq!(a.in_flight(), 0);
    }

    #[test]
    fn abandoned_assemblies_expire_and_release_their_budget() {
        let parts = parts_of(&manifest(20_000), 100_000);
        let mut a = asm();
        let t0 = Instant::now();
        a.push(Uuid::new_v4(), &parts[0], t0).unwrap();
        assert_eq!(a.in_flight(), 1);
        assert!(a.budget.used() > 0);
        assert_eq!(a.expire(t0 + Duration::from_secs(29)), 0);
        assert_eq!(a.expire(t0 + Duration::from_secs(31)), 1);
        assert_eq!(a.in_flight(), 0);
        assert_eq!(a.budget.used(), 0);
    }

    #[test]
    fn concurrent_assemblies_are_capped_and_the_budget_is_shared() {
        let parts = parts_of(&manifest(20_000), 100_000);
        let one = parts[0].total_bytes as usize;
        // Budget for exactly two assemblies.
        let budget = ByteBudget::new(one * 2);
        let mut a = PartAssembler::new(8, Duration::from_secs(30), budget.clone());
        let now = Instant::now();
        let mk = |id: Uuid| {
            let mut p = parts[0].clone();
            p.assembly_id = id;
            p
        };
        a.push(Uuid::new_v4(), &mk(Uuid::new_v4()), now).unwrap();
        a.push(Uuid::new_v4(), &mk(Uuid::new_v4()), now).unwrap();
        let err = a
            .push(Uuid::new_v4(), &mk(Uuid::new_v4()), now)
            .unwrap_err();
        assert_eq!(err, PartError::BudgetExhausted);
        assert_eq!(a.in_flight(), 2);
        drop(a);
        assert_eq!(budget.used(), 0, "dropping the assembler frees its bytes");

        // A cap of 1 evicts the older assembly instead.
        let mut a = PartAssembler::new(1, Duration::from_secs(30), ByteBudget::new(one * 4));
        a.push(Uuid::new_v4(), &mk(Uuid::new_v4()), now).unwrap();
        a.push(Uuid::new_v4(), &mk(Uuid::new_v4()), now).unwrap();
        assert_eq!(a.in_flight(), 1);
    }

    #[test]
    fn a_newer_manifest_replaces_a_stale_assembly_in_the_same_room() {
        let parts = parts_of(&manifest(20_000), 100_000);
        let room = Uuid::new_v4();
        let now = Instant::now();
        let mut a = asm();
        a.push(room, &parts[0], now).unwrap();
        let mut newer = parts[0].clone();
        newer.assembly_id = Uuid::new_v4();
        newer.version = 4;
        a.push(room, &newer, now).unwrap();
        assert_eq!(a.in_flight(), 1);
        // The old assembly's next piece finds nothing to continue.
        assert!(a.push(room, &parts[1], now).is_err());
    }

    #[test]
    fn assemblies_in_different_rooms_do_not_mix() {
        let parts = parts_of(&manifest(20_000), 100_000);
        let now = Instant::now();
        let mut a = asm();
        a.push(Uuid::new_v4(), &parts[0], now).unwrap();
        // Same assembly id, different room: not a continuation.
        assert!(a.push(Uuid::new_v4(), &parts[1], now).is_err());
    }

    #[test]
    fn a_small_frame_cap_yields_smaller_pieces_and_a_tiny_one_is_refused() {
        assert_eq!(part_size_for_frame_cap(1 << 20).unwrap(), MAX_PART_BYTES);
        assert_eq!(part_size_for_frame_cap(8 << 20).unwrap(), MAX_PART_BYTES);
        let small = part_size_for_frame_cap(128 * 1024).unwrap();
        assert!(small < MAX_PART_BYTES && small * 4 / 3 + 4096 <= 128 * 1024);
        assert!(matches!(
            part_size_for_frame_cap(16 * 1024),
            Err(PartError::FrameCapTooSmall { .. })
        ));
    }

    #[test]
    fn garbage_that_is_not_a_manifest_is_refused_at_the_end() {
        let bytes = vec![b'x'; 1000];
        let parts = split_manifest_bytes(
            &bytes,
            ManifestPartKind::Response,
            1,
            1,
            Uuid::new_v4(),
            400,
        )
        .unwrap();
        let mut a = asm();
        let room = Uuid::new_v4();
        let now = Instant::now();
        let mut last = Ok(None);
        for p in &parts {
            last = a.push(room, p, now);
        }
        assert!(matches!(last, Err(PartError::BadManifest(_))));
    }
}
