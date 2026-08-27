//! Shared protocol crate.
//!
//! P0-T01: placeholder. P0-T07 adds the first `ts-rs` example struct and
//! the TypeScript generator script. P3+ fills in playback, drawing, laser,
//! manifest, and download message types per `docs/ARCHITECTURE.md`
//! section 18 and section 26.4.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use ts_rs::TS;

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
/// - a nested `enum` (`Mood`) to exercise tagged-union marshalling
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
        let rendered = HelloWorld::export_to_string(&ts_rs::Config::default())
            .expect("render HelloWorld bindings");
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
