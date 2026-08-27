# Shared manifest crate

The Locast media manifest data model, validation, and signing/verification. P0-T01 registers this as a Cargo workspace member; P3+ adds the full `MediaManifest`, `MediaEntry`, and `SubtitleEntry` types plus the host-side signer and viewer-side verifier per `docs/ARCHITECTURE.md` section 8 and section 26.4.
