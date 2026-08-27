# Shared protocol crate

Cross-language MessagePack message definitions and data models used by both the Locast client and the Locast server. P0-T01 registers this as a Cargo workspace member; P0-T07 (ts-rs protocol skeleton) adds the first generated TypeScript module. P3+ fills in the playback, drawing, laser, and download message types per `docs/ARCHITECTURE.md` sections 18 and 26.4.
