//! `path_validator` - the P8-T01 cargo-fuzz target.
//!
//! Feeds arbitrary bytes (lossily decoded to a `&str`, the type
//! the production caller `library::protocol` passes) through
//! `locast_client_lib::core::paths::validate_library_path`
//! against a fixed, real library root. The root holds one regular
//! file at `library/ab/cd/Movie.mkv` so mutation can reach the
//! canonicalize + containment branch, not only the lexical checks.
//!
//! Properties checked:
//! - no input panics or hangs (`Err` and `Ok` are both fine);
//! - every `Ok` path is a regular file under the canonical root;
//! - no `Ok` input contains `..`, `\`, NUL, or a leading `/`.
#![no_main]

use std::path::PathBuf;
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use locast_client_lib::core::paths::validate_library_path;

struct Fixture {
    rt: tokio::runtime::Runtime,
    root: PathBuf,
    canonical_root: PathBuf,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = std::env::temp_dir().join("locast-fuzz-path-validator");
        let nested = root.join("library").join("ab").join("cd");
        std::fs::create_dir_all(&nested).expect("create fuzz library root");
        std::fs::write(nested.join("Movie.mkv"), b"fuzz").expect("write fuzz fixture file");
        let canonical_root = std::fs::canonicalize(&root).expect("canonicalize fuzz root");
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("build tokio runtime");
        Fixture {
            rt,
            root,
            canonical_root,
        }
    })
}

fuzz_target!(|data: &[u8]| {
    let fx = fixture();
    let rel = String::from_utf8_lossy(data);
    if let Ok(p) = fx.rt.block_on(validate_library_path(&fx.root, &rel)) {
        assert!(p.starts_with(&fx.canonical_root), "escaped root: {rel:?}");
        assert!(p.is_file(), "accepted non-file: {rel:?}");
        assert!(
            !rel.split('/').any(|s| s == ".."),
            "accepted traversal: {rel:?}"
        );
        assert!(
            !rel.contains('\\') && !rel.contains('\0') && !rel.starts_with('/'),
            "accepted forbidden form: {rel:?}"
        );
    }
});
