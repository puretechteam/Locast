//! Shared protocol crate.
//!
//! Defines the wire-format types that the Rust client, the Rust
//! server, and the TypeScript webview all agree on. The contents
//! mirror the tables in `docs/ARCHITECTURE.md` section 18 (Network
//! Protocol Design) and section 20 (Server Architecture).
//!
//! P0-T01: placeholder. P0-T07 added the first `ts-rs` example struct
//! and the TypeScript generator script. P2-T02 adds the envelope
//! scaffolding and the five handshake payload structs. P3+ fills
//! in playback, drawing, laser, manifest, and download message
//! types per the architecture.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub mod envelope;
pub mod handshake;
pub mod room;

/// Library version string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Placeholder for the wire-format namespace prefix used by all envelopes
/// (see `docs/ARCHITECTURE.md` section 18.2). Returned as `&'static str` so
/// it can be referenced from `const` contexts and from generated bindings.
pub const WIRE_NAMESPACE: &str = "locast.v1";

/// Example payload used to smoke-test the `ts-rs` generator in P0-T07.
///
/// This is deliberately a toy struct, not a real protocol envelope. The
/// shape exercises the bits the generator needs to round-trip:
///
/// - a `String` (`id`) to exercise string marshalling
/// - an integer (`counter`) to exercise numeric marshalling
/// - an `Option<String>` (`note`) to exercise nullable fields
/// - a nested `enum` (`mood`) to exercise tagged-union marshalling
///
/// `mood` is marked `#[ts(inline)]` so the `Mood` union is rendered inline
/// in `HelloWorld` and the generated `ts/index.ts` stays self-contained
/// (no `import { Mood } from "../Mood"` line). Real envelope types in P3+
/// will be free to either inline or import; this is a smoke test of the
/// generator, not a fixed design decision.
///
/// Real envelope types land in P3+ per `docs/ARCHITECTURE.md` section 18.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "ts/index.ts")]
pub struct HelloWorld {
    pub id: String,
    pub counter: u32,
    pub note: Option<String>,
    #[ts(inline)]
    pub mood: Mood,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub enum Mood {
    Happy,
    Neutral,
    Sad,
}

#[cfg(test)]
mod tests {
    use super::{HelloWorld, Mood};
    use ts_rs::TS;

    /// Regenerate `shared/protocol/ts/index.ts` from the `HelloWorld`
    /// example struct and assert that the rendered output is byte-for-byte
    /// identical to the file already on disk.
    ///
    /// Mirrors the P0-T06 `gen_bindings` pattern: on a mismatch the test
    /// overwrites the checked-in file and panics with a clear instruction
    /// to re-run. CI then runs this same test and `git diff --exit-code`
    /// against the regenerated file to fail the build on any drift.
    ///
    /// Marked `#[ignore]` so it does not run on every `cargo test`
    /// invocation; the workspace `cargo test` step in CI does not depend
    /// on this. Invoke manually via:
    ///
    /// ```text
    /// cargo test -p locast-protocol -- --ignored
    /// ```
    #[test]
    #[ignore]
    fn ts_export() {
        // We render each public type via `export_to_string` and
        // compare the concatenation to the on-disk
        // `ts/index.ts`. The `TS` trait is not object-safe so we
        // call each type's method directly.
        //
        // `serde_json::Value` is exported too so the
        // `serde_json/JsonValue.ts` helper is generated; without
        // it the Envelope payload field's import statement would
        // point to a non-existent file. We capture the helper
        // content out of band and append it to the rendered
        // output below the index.ts content.
        let cfg = ts_rs::Config::default();
        let mut rendered = String::new();
        rendered.push_str(&HelloWorld::export_to_string(&cfg).expect("render HelloWorld bindings"));
        rendered.push('\n');
        rendered.push_str(&Mood::export_to_string(&cfg).expect("render Mood bindings"));
        rendered.push('\n');
        rendered.push_str(
            &crate::envelope::Envelope::export_to_string(&cfg).expect("render Envelope bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::envelope::MessageKind::export_to_string(&cfg)
                .expect("render MessageKind bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::envelope::Sender::export_to_string(&cfg).expect("render Sender bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::handshake::HelloPayload::export_to_string(&cfg)
                .expect("render HelloPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::handshake::WelcomePayload::export_to_string(&cfg)
                .expect("render WelcomePayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::handshake::WelcomeConfig::export_to_string(&cfg)
                .expect("render WelcomeConfig bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::handshake::WelcomeRate::export_to_string(&cfg)
                .expect("render WelcomeRate bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::handshake::ChallengePayload::export_to_string(&cfg)
                .expect("render ChallengePayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::handshake::AuthPayload::export_to_string(&cfg)
                .expect("render AuthPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::handshake::AuthOkPayload::export_to_string(&cfg)
                .expect("render AuthOkPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::handshake::AuthBearer::export_to_string(&cfg)
                .expect("render AuthBearer bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::handshake::AuthFailPayload::export_to_string(&cfg)
                .expect("render AuthFailPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::handshake::AuthFailReason::export_to_string(&cfg)
                .expect("render AuthFailReason bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::handshake::Platform::export_to_string(&cfg).expect("render Platform bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::RoomCreatePayload::export_to_string(&cfg)
                .expect("render RoomCreatePayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::RoomCreatedPayload::export_to_string(&cfg)
                .expect("render RoomCreatedPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::RoomJoinRequestPayload::export_to_string(&cfg)
                .expect("render RoomJoinRequestPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::RoomJoinedPayload::export_to_string(&cfg)
                .expect("render RoomJoinedPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::RoomLeavePayload::export_to_string(&cfg)
                .expect("render RoomLeavePayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::RoomStatePayload::export_to_string(&cfg)
                .expect("render RoomStatePayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::ParticipantSelf::export_to_string(&cfg)
                .expect("render ParticipantSelf bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::RoomSummary::export_to_string(&cfg).expect("render RoomSummary bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::Participant::export_to_string(&cfg).expect("render Participant bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::ParticipantStatus::export_to_string(&cfg)
                .expect("render ParticipantStatus bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::ParticipantJoinedPayload::export_to_string(&cfg)
                .expect("render ParticipantJoinedPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::ParticipantLeftPayload::export_to_string(&cfg)
                .expect("render ParticipantLeftPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::HostDisconnectedPayload::export_to_string(&cfg)
                .expect("render HostDisconnectedPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::HostReconnectedPayload::export_to_string(&cfg)
                .expect("render HostReconnectedPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::HostMigratedPayload::export_to_string(&cfg)
                .expect("render HostMigratedPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::RoomClosedPayload::export_to_string(&cfg)
                .expect("render RoomClosedPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::RoomErrorPayload::export_to_string(&cfg)
                .expect("render RoomErrorPayload bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::RoomErrorCode::export_to_string(&cfg)
                .expect("render RoomErrorCode bindings"),
        );
        rendered.push('\n');
        rendered.push_str(
            &crate::room::PresencePayload::export_to_string(&cfg)
                .expect("render PresencePayload bindings"),
        );
        rendered.push('\n');

        let path = std::path::Path::new("ts/index.ts");

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create ts/ directory");
        }

        let existing = std::fs::read_to_string(path).ok();
        match existing {
            Some(existing) if existing == rendered => {
                println!("OK: ts/index.ts is already up to date");
            }
            _ => {
                std::fs::write(path, &rendered).expect("write ts/index.ts");
                panic!(
                    "ts/index.ts was out of date and has been overwritten; \
                     run `cargo test -p locast-protocol -- --ignored` (or \
                     `pnpm -F @locast/protocol gen:protocol`) to regenerate, \
                     then commit the result."
                );
            }
        }
    }

    /// Plain (non-ignored) compile-time check that the example struct
    /// round-trips through serde. P0-T07 only needs the `ts-rs` pipeline
    /// to work, but pinning the serde shape here keeps the example
    /// honest as P3+ adds real envelope types on top of it.
    #[test]
    fn hello_world_serde_roundtrip() {
        let original = HelloWorld {
            id: "abc-123".to_string(),
            counter: 42,
            note: Some("hi".to_string()),
            mood: Mood::Happy,
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let decoded: HelloWorld = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, original);
    }
}
