//! P1-T06 integration test: `import_one` flows through the probe.
//!
//! Run with `cargo test -p locast-client --test probe_import` or
//! `cargo test --workspace`.
//!
//! Pins that the `import_one` orchestrator wires the probe's six
//! optional fields into the `media_items` row when a stub `ffprobe`
//! is on `PATH`. Also pins the regression case: when no `ffprobe` is
//! on `PATH`, the import still succeeds and the six columns are
//! `NULL` (the import must not fail just because the probe is
//! missing).

#![allow(clippy::needless_raw_string_hashes)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use locast_client_lib::commands::import::import_one;
use locast_client_lib::core::quota::QuotaAccountant;
use locast_client_lib::storage::Storage;
use sqlx::Row;
use tempfile::TempDir;

/// Captures the original PATH once so we can restore it between tests.
static ORIGINAL_PATH: Mutex<Option<std::ffi::OsString>> = Mutex::new(None);

/// Serializes tests that mutate the process-wide PATH. See the
/// matching mutex in `probe.rs` for the rationale.
static PATH_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Same isolation strategy as `probe.rs::with_isolated_path`: PATH
/// becomes the tempdir plus the platform's base entries (System32 on
/// Windows, /bin on POSIX) but NOT the original system PATH. This is
/// the only way to keep the host's pre-installed `ffmpeg` from
/// shadowing our test stub.
fn with_isolated_path<F: FnOnce(&Path)>(dir: &Path, body: F) {
    // Use `lock().unwrap_or_else(|e| e.into_inner())` so a panic in
    // a previous test (which would poison the mutex) does not
    // cascade into a panic in this test. We still serialize.
    let _lock = PATH_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut guard = ORIGINAL_PATH
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if guard.is_none() {
        *guard = Some(std::env::var_os("PATH").unwrap_or_default());
    }
    drop(guard);

    let dir_str = dir.to_string_lossy().into_owned();
    let dir_trim = dir_str
        .trim_end_matches(std::path::MAIN_SEPARATOR)
        .trim_end_matches('/');
    const PATH_SEP: char = if cfg!(windows) { ';' } else { ':' };
    #[cfg(windows)]
    let base: &[&str] = &[
        r"C:\Windows\System32",
        r"C:\Windows",
        r"C:\Windows\System32\Wbem",
    ];
    #[cfg(not(windows))]
    let base: &[&str] = &["/usr/bin", "/bin", "/usr/local/bin"];
    let mut parts: Vec<String> = vec![dir_trim.to_string()];
    for entry in base {
        parts.push((*entry).to_string());
    }
    let new_path = parts.join(&PATH_SEP.to_string());

    let previous = std::env::var_os("PATH");
    std::env::set_var("PATH", &new_path);

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(dir)));

    match previous {
        Some(v) => std::env::set_var("PATH", v),
        None => std::env::remove_var("PATH"),
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}

fn open_storage(dir: &TempDir) -> Storage {
    let db = dir.path().join("index.sqlite");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    rt.block_on(Storage::open(&db)).expect("storage opens")
}

fn open_accountant(storage: &Storage) -> QuotaAccountant {
    QuotaAccountant::new(storage.clone())
}

fn write_source(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).expect("write source file");
    p
}

fn make_library_root() -> PathBuf {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path().join("library");
    std::fs::create_dir_all(&root).expect("create library root");
    ROOT_HOLDERS.lock().expect("root holders").push(dir);
    root
}

static ROOT_HOLDERS: Mutex<Vec<TempDir>> = Mutex::new(Vec::new());

#[cfg(unix)]
fn drop_ffprobe_stub(dir: &Path, body: &str) -> PathBuf {
    let p = dir.join("ffprobe");
    std::fs::write(&p, body).expect("write stub");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&p).expect("metadata").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&p, perms).expect("chmod");
    p
}

#[cfg(windows)]
fn drop_ffprobe_stub(dir: &Path, body: &str) -> PathBuf {
    let p = dir.join("ffprobe.cmd");
    std::fs::write(&p, body).expect("write stub");
    p
}

#[cfg(unix)]
fn stub_body_echo_json(json: &str) -> String {
    format!("#!/bin/sh\ncat <<'LOCAST_EOF'\n{json}\nLOCAST_EOF\n")
}

#[cfg(windows)]
fn stub_body_echo_json(json: &str) -> String {
    format!("echo {json}")
}

// ===========================================================================
// acceptance: with a stub ffprobe printing all six fields, the
// import_one call populates the media_items row with the same six
// fields.
// ===========================================================================

#[test]
fn import_one_populates_probe_fields_when_ffprobe_present() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let json = r#"{"format":{"duration":"5400.123456","format_name":"matroska,webm"},"streams":[{"codec_type":"video","codec_name":"h264","width":1920,"height":1080},{"codec_type":"audio","codec_name":"aac"}]}"#;
    let stub = drop_ffprobe_stub(scratch.path(), &stub_body_echo_json(json));
    eprintln!("PROBE_IMPORT_STUB: {}", stub.display());

    with_isolated_path(scratch.path(), |_dir| {
        let lib_root = make_library_root();
        let storage_holder = TempDir::new().expect("storage tempdir");
        let storage = open_storage(&storage_holder);
        let accountant = open_accountant(&storage);

        let src_dir = TempDir::new().expect("src tempdir");
        let bytes: Vec<u8> = (0u32..2048).map(|i| (i & 0xFF) as u8).collect();
        let src = write_source(src_dir.path(), "Probeable.mkv", &bytes);

        let imported = rt
            .block_on(import_one(
                &accountant,
                &lib_root,
                &storage,
                &src,
                "Probeable.mkv",
            ))
            .expect("import succeeds");

        // Query the row and assert the six optional columns are
        // populated. We do this directly via sqlx so the test is
        // orthogonal to any future change in `ImportedMedia`.
        let row = rt.block_on(async {
            sqlx::query(
                "SELECT duration_ms, width, height, video_codec, audio_codec, container \
                 FROM media_items WHERE id = ?1",
            )
            .bind(&imported.id)
            .fetch_one(&storage.pool())
            .await
            .expect("row present")
        });

        let duration_ms: Option<i64> = row.get("duration_ms");
        let width: Option<i32> = row.get("width");
        let height: Option<i32> = row.get("height");
        let video_codec: Option<String> = row.get("video_codec");
        let audio_codec: Option<String> = row.get("audio_codec");
        let container: Option<String> = row.get("container");

        eprintln!("ROW duration_ms={duration_ms:?} width={width:?} height={height:?} video_codec={video_codec:?} audio_codec={audio_codec:?} container={container:?}");

        assert_eq!(duration_ms, Some(5_400_123));
        assert_eq!(width, Some(1920));
        assert_eq!(height, Some(1080));
        assert_eq!(video_codec.as_deref(), Some("h264"));
        assert_eq!(audio_codec.as_deref(), Some("aac"));
        assert_eq!(container.as_deref(), Some("matroska"));
    });
}

// ===========================================================================
// regression: with no ffprobe on PATH, the import still succeeds
// and the six optional columns are NULL.
// ===========================================================================

#[test]
fn import_one_succeeds_with_no_ffprobe_on_path() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    with_isolated_path(scratch.path(), |_dir| {
        let lib_root = make_library_root();
        let storage_holder = TempDir::new().expect("storage tempdir");
        let storage = open_storage(&storage_holder);
        let accountant = open_accountant(&storage);

        let src_dir = TempDir::new().expect("src tempdir");
        let bytes = b"no probe, no problem".to_vec();
        let src = write_source(src_dir.path(), "NoProbe.mkv", &bytes);

        let imported = rt
            .block_on(import_one(
                &accountant,
                &lib_root,
                &storage,
                &src,
                "NoProbe.mkv",
            ))
            .expect("import succeeds even without ffprobe");

        let row = rt.block_on(async {
            sqlx::query(
                "SELECT duration_ms, width, height, video_codec, audio_codec, container \
                 FROM media_items WHERE id = ?1",
            )
            .bind(&imported.id)
            .fetch_one(&storage.pool())
            .await
            .expect("row present")
        });

        let duration_ms: Option<i64> = row.get("duration_ms");
        let width: Option<i32> = row.get("width");
        let height: Option<i32> = row.get("height");
        let video_codec: Option<String> = row.get("video_codec");
        let audio_codec: Option<String> = row.get("audio_codec");
        let container: Option<String> = row.get("container");

        assert_eq!(duration_ms, None, "no probe => duration_ms NULL");
        assert_eq!(width, None, "no probe => width NULL");
        assert_eq!(height, None, "no probe => height NULL");
        assert_eq!(video_codec, None, "no probe => video_codec NULL");
        assert_eq!(audio_codec, None, "no probe => audio_codec NULL");
        assert_eq!(container, None, "no probe => container NULL");
    });
}

// ===========================================================================
// regression: a stub that exits nonzero is treated like "no probe" -
// the import still succeeds and the columns stay NULL.
// ===========================================================================

#[test]
fn import_one_succeeds_when_stub_exits_nonzero() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let body = if cfg!(windows) {
        "exit /b 1".to_string()
    } else {
        "#!/bin/sh\nexit 1\n".to_string()
    };
    drop_ffprobe_stub(scratch.path(), &body);

    with_isolated_path(scratch.path(), |_dir| {
        let lib_root = make_library_root();
        let storage_holder = TempDir::new().expect("storage tempdir");
        let storage = open_storage(&storage_holder);
        let accountant = open_accountant(&storage);

        let src_dir = TempDir::new().expect("src tempdir");
        let bytes = b"nonzero exit is fine".to_vec();
        let src = write_source(src_dir.path(), "Nonzero.mkv", &bytes);

        let imported = rt
            .block_on(import_one(
                &accountant,
                &lib_root,
                &storage,
                &src,
                "Nonzero.mkv",
            ))
            .expect("nonzero ffprobe must not block the import");

        let row = rt.block_on(async {
            sqlx::query(
                "SELECT duration_ms, width, height, video_codec, audio_codec, container \
                 FROM media_items WHERE id = ?1",
            )
            .bind(&imported.id)
            .fetch_one(&storage.pool())
            .await
            .expect("row present")
        });
        let duration_ms: Option<i64> = row.get("duration_ms");
        assert_eq!(duration_ms, None, "nonzero probe => all-NULL row");
    });
}
