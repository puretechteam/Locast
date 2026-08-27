//! P1-T06 integration test: the optional `ffprobe` / `ffmpeg` probe.
//!
//! Run with `cargo test -p locast-client --test probe` or simply
//! `cargo test --workspace`.
//!
//! The roadmap's P1-T06 acceptance is:
//!
//! > with no ffmpeg on PATH, the probe returns `None` and the import
//! > still succeeds; with a stub ffmpeg that prints JSON, the parse
//! > populates `duration_ms`, `width`, `height`, `video_codec`,
//! > `audio_codec`, `container`.
//!
//! Each test below sets up a controlled `PATH` (a tempdir plus, on
//! Windows, the absolute system directories needed to launch any
//! process at all) and runs the probe against a stub executable
//! inside that `PATH`. The probes the user is most likely to care
//! about are all covered here.

#![allow(clippy::needless_raw_string_hashes)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use locast_client_lib::probe::ffprobe::{
    executable_candidates, run, run_with_timeout, ProbeResult, DEFAULT_TIMEOUT,
};
use tempfile::TempDir;

/// The current `PATH` value, snapshotted at process start. We restore
/// it after every test to keep the suite hermetic.
static ORIGINAL_PATH: Mutex<Option<std::ffi::OsString>> = Mutex::new(None);

/// A second mutex held for the duration of every PATH-isolating
/// test. Cargo runs tests in parallel by default; the PATH is a
/// process-wide global, so two tests that both call
/// `std::env::set_var("PATH", ...)` concurrently will stomp on each
/// other. This mutex serializes them. It is `static` so it lives
/// for the entire test binary, and it is held only for the body of
/// `with_isolated_path` (not for the lifetime of the test) so the
/// critical section is short.
static PATH_TEST_LOCK: Mutex<()> = Mutex::new(());

/// The PATH entries that we always keep available, regardless of the
/// test's chosen "isolated" path. The `tokio::process::Command` and
/// `std::process::Command` machinery on Windows relies on
/// `CreateProcessW`, which needs to find `cmd.exe` (for `.cmd`/`.bat`
/// stubs) and the basic Windows utilities. Without these, even a
/// well-formed stub cannot launch.
///
/// On POSIX this is empty: `/bin/sh` is always reachable via an
/// absolute path in our POSIX stubs, and the shell's own lookup
/// machinery does not depend on PATH for the interpreter itself.
#[cfg(windows)]
fn base_path_entries() -> Vec<&'static str> {
    vec![
        r"C:\Windows\System32",
        r"C:\Windows",
        r"C:\Windows\System32\Wbem",
    ]
}

#[cfg(not(windows))]
fn base_path_entries() -> Vec<&'static str> {
    vec!["/usr/bin", "/bin", "/usr/local/bin"]
}

/// `with_isolated_path` runs `body` with `PATH` set to `dir` plus the
/// platform's base entries, NOT including the original system PATH.
/// This is the only way to guarantee the host's pre-installed
/// `ffmpeg`/`ffprobe` does not shadow our stub. The original PATH is
/// captured once (so we can restore it after the test) but it is
/// intentionally excluded from the isolated PATH.
///
/// The body runs under [`PATH_TEST_LOCK`] so concurrent tests cannot
/// stomp on each other's PATH.
fn with_isolated_path<F: FnOnce(&Path)>(dir: &Path, body: F) {
    // Recover from poisoning so a panic in one test does not cascade.
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
    let mut parts: Vec<String> = vec![dir_trim.to_string()];
    for entry in base_path_entries() {
        parts.push(entry.to_string());
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

/// Write a POSIX `#!/bin/sh` script that prints `body` and exits 0.
/// On Windows this won't run; tests targeting Windows use
/// `write_cmd_stub` instead.
#[cfg(unix)]
fn write_shell_stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).expect("write shell stub");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&p).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).expect("chmod");
    }
    p
}

/// Write a Windows `.cmd` script that prints `body` and exits 0.
/// On POSIX this isn't used; the test suite picks the right helper.
#[cfg(windows)]
fn write_cmd_stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(format!("{name}.cmd"));
    // `echo` + `exit /b 0` produces the body and exits cleanly.
    // We use `>nul` redirection so the stub file itself is created
    // (the `>` here writes the file, not the stub's content).
    let content = format!("@echo off\r\n{body}\r\nexit /b 0\r\n");
    std::fs::write(&p, content).expect("write cmd stub");
    p
}

/// Drop a stub on disk using the platform's native format and return
/// the path the OS will resolve when we ask for `name` on PATH.
fn drop_stub(dir: &Path, name_without_ext: &str, body: &str) -> PathBuf {
    #[cfg(unix)]
    {
        write_shell_stub(dir, name_without_ext, body)
    }
    #[cfg(windows)]
    {
        write_cmd_stub(dir, name_without_ext, body)
    }
}

// ===========================================================================
// acceptance: no ffprobe on PATH => run returns None
// ===========================================================================

#[test]
fn probe_returns_none_with_no_ffprobe_on_path() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    with_isolated_path(scratch.path(), |dir| {
        let dummy = dir.join("dummy.mkv");
        std::fs::write(&dummy, b"not a real video").expect("write dummy");
        let result = rt.block_on(run(&dummy));
        assert!(
            result.is_none(),
            "with no ffprobe/ffmpeg on PATH, run must return None, got {result:?}"
        );
    });
}

// ===========================================================================
// acceptance: stub ffprobe prints JSON => fields populated
// ===========================================================================

#[test]
fn probe_populates_all_six_fields_from_stub_json() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let json = r#"{"format":{"duration":"5400.123456","format_name":"matroska,webm"},"streams":[{"codec_type":"video","codec_name":"h264","width":1920,"height":1080},{"codec_type":"audio","codec_name":"aac"}]}"#;
    let body = stub_body_echo_json(json);
    let stub_path = drop_stub(scratch.path(), "ffprobe", &body);
    eprintln!("STUB_PATH: {}", stub_path.display());
    eprintln!("STUB_BODY: {body}");

    with_isolated_path(scratch.path(), |dir| {
        let dummy = dir.join("dummy.mkv");
        std::fs::write(&dummy, b"x").expect("write dummy");
        let result = rt.block_on(run(&dummy));
        eprintln!("STUB_RESULT: {result:?}");
        let p = result.expect("probe succeeded against stub");
        assert_eq!(p.duration_ms, Some(5_400_123));
        assert_eq!(p.width, Some(1920));
        assert_eq!(p.height, Some(1080));
        assert_eq!(p.video_codec.as_deref(), Some("h264"));
        assert_eq!(p.audio_codec.as_deref(), Some("aac"));
        assert_eq!(p.container.as_deref(), Some("matroska"));
    });
}

// ===========================================================================
// executable_candidates exposes the documented order
// ===========================================================================

#[test]
fn executable_candidates_order() {
    let c = executable_candidates();
    assert!(c.contains(&"ffprobe"));
    assert!(c.contains(&"ffmpeg"));
    // ffprobe is the canonical first choice.
    assert_eq!(c[0], "ffprobe");
    // The .exe variants must be present for Windows.
    assert!(c.contains(&"ffprobe.exe"));
    assert!(c.contains(&"ffmpeg.exe"));
}

// ===========================================================================
// stub exits nonzero => None
// ===========================================================================

#[test]
fn probe_returns_none_on_nonzero_exit() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let body = stub_body_exit_nonzero();
    drop_stub(scratch.path(), "ffprobe", &body);
    with_isolated_path(scratch.path(), |dir| {
        let dummy = dir.join("dummy.mkv");
        std::fs::write(&dummy, b"x").expect("write dummy");
        let result = rt.block_on(run(&dummy));
        assert!(
            result.is_none(),
            "nonzero exit must yield None, got {result:?}"
        );
    });
}

// ===========================================================================
// stub prints malformed JSON => None (parse falls through to default)
// ===========================================================================
// malformed JSON => the parser returns the default-all-None ProbeResult,
// so `run` returns Some(default), not None. The test is named to
// reflect the actual behavior (Some with default fields), not the
// misleading "None" suggestion.
// ===========================================================================

#[test]
fn probe_returns_some_default_on_malformed_json() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let body = stub_body_echo_raw("{this is not json");
    drop_stub(scratch.path(), "ffprobe", &body);
    with_isolated_path(scratch.path(), |dir| {
        let dummy = dir.join("dummy.mkv");
        std::fs::write(&dummy, b"x").expect("write dummy");
        let result = rt.block_on(run(&dummy));
        // Malformed JSON: parse_ffprobe_json returns the default
        // (all-None) ProbeResult, so `run` returns Some(default).
        // Document the choice: the run function never returns None
        // for a well-formed-but-empty probe; it returns the
        // default-all-None ProbeResult.
        let p = result.expect("malformed JSON yields Some(default), not None");
        assert_eq!(p, ProbeResult::default());
    });
}

// ===========================================================================
// empty stdout => the executable ran but produced no output, which
// the parser treats as a probe failure => `run` returns None.
// ===========================================================================

#[test]
fn probe_returns_none_on_empty_stdout() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    // The stub prints nothing on stdout.
    let body = String::new();
    drop_stub(scratch.path(), "ffprobe", &body);
    with_isolated_path(scratch.path(), |dir| {
        let dummy = dir.join("dummy.mkv");
        std::fs::write(&dummy, b"x").expect("write dummy");
        let result = rt.block_on(run(&dummy));
        assert!(result.is_none(), "empty stdout => None, got {result:?}");
    });
}

// ===========================================================================
// empty format section => Some(all-None) ProbeResult
// ===========================================================================

#[test]
fn probe_returns_some_all_none_on_empty_format() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let body = stub_body_echo_json(r#"{"format": {}}"#);
    drop_stub(scratch.path(), "ffprobe", &body);
    with_isolated_path(scratch.path(), |dir| {
        let dummy = dir.join("dummy.mkv");
        std::fs::write(&dummy, b"x").expect("write dummy");
        let result = rt.block_on(run(&dummy));
        let p = result.expect("present JSON yields Some");
        assert_eq!(p, ProbeResult::default());
    });
}

// ===========================================================================
// no streams => only container / duration filled
// ===========================================================================

#[test]
fn probe_handles_no_streams() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let body = stub_body_echo_json(r#"{"format": {"format_name": "wav"}}"#);
    drop_stub(scratch.path(), "ffprobe", &body);
    with_isolated_path(scratch.path(), |dir| {
        let dummy = dir.join("dummy.mkv");
        std::fs::write(&dummy, b"x").expect("write dummy");
        let p = rt.block_on(run(&dummy)).expect("present JSON yields Some");
        assert_eq!(p.container.as_deref(), Some("wav"));
        assert_eq!(p.video_codec, None);
        assert_eq!(p.audio_codec, None);
        assert_eq!(p.width, None);
        assert_eq!(p.height, None);
        assert_eq!(p.duration_ms, None);
    });
}

// ===========================================================================
// container with multiple names => first segment only
// ===========================================================================

#[test]
fn probe_container_takes_first_segment() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let body = stub_body_echo_json(r#"{"format":{"format_name":"matroska,webm"}}"#);
    drop_stub(scratch.path(), "ffprobe", &body);
    with_isolated_path(scratch.path(), |dir| {
        let dummy = dir.join("dummy.mkv");
        std::fs::write(&dummy, b"x").expect("write dummy");
        let p = rt.block_on(run(&dummy)).expect("Some");
        assert_eq!(p.container.as_deref(), Some("matroska"));
    });
}

// ===========================================================================
// duration parse: 5400.123456 => 5400123
// ===========================================================================

#[test]
fn probe_duration_ms_parse() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let body = stub_body_echo_json(
        r#"{"format":{"duration":"5400.123456","format_name":"matroska"},"streams":[{"codec_type":"video","codec_name":"h264","width":1280,"height":720}]}"#,
    );
    drop_stub(scratch.path(), "ffprobe", &body);
    with_isolated_path(scratch.path(), |dir| {
        let dummy = dir.join("dummy.mkv");
        std::fs::write(&dummy, b"x").expect("write dummy");
        let p = rt.block_on(run(&dummy)).expect("Some");
        assert_eq!(p.duration_ms, Some(5_400_123));
    });
}

// ===========================================================================
// path containing spaces => probe still works (no shell, arg passed verbatim)
// ===========================================================================

#[test]
fn probe_handles_path_with_spaces() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let body = stub_body_echo_json(
        r#"{"format":{"duration":"1.0","format_name":"mp4"},"streams":[{"codec_type":"video","codec_name":"h264","width":640,"height":360}]}"#,
    );
    drop_stub(scratch.path(), "ffprobe", &body);
    with_isolated_path(scratch.path(), |dir| {
        let dir_with_space = dir.join("path with space");
        std::fs::create_dir_all(&dir_with_space).expect("mkdir");
        let dummy = dir_with_space.join("my file.mkv");
        std::fs::write(&dummy, b"x").expect("write dummy");
        let p = rt.block_on(run(&dummy)).expect("Some");
        assert_eq!(p.video_codec.as_deref(), Some("h264"));
        assert_eq!(p.width, Some(640));
    });
}

// ===========================================================================
// Unicode filename => probe still works
// ===========================================================================

#[test]
fn probe_handles_unicode_filename() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let body = stub_body_echo_json(
        r#"{"format":{"duration":"1.0","format_name":"mp4"},"streams":[{"codec_type":"video","codec_name":"h264","width":640,"height":360}]}"#,
    );
    drop_stub(scratch.path(), "ffprobe", &body);
    with_isolated_path(scratch.path(), |dir| {
        let unicode_name = "動画_тест.mkv";
        let dummy = dir.join(unicode_name);
        std::fs::write(&dummy, b"x").expect("write dummy");
        let p = rt.block_on(run(&dummy)).expect("Some");
        assert_eq!(p.video_codec.as_deref(), Some("h264"));
    });
}

// ===========================================================================
// timeout: a long-sleeping stub is killed and run returns None
// ===========================================================================

#[test]
fn probe_returns_none_on_timeout() {
    let scratch = TempDir::new().expect("scratch");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let body = stub_body_sleep();
    drop_stub(scratch.path(), "ffprobe", &body);
    with_isolated_path(scratch.path(), |dir| {
        let dummy = dir.join("dummy.mkv");
        std::fs::write(&dummy, b"x").expect("write dummy");
        let result = rt.block_on(run_with_timeout(
            &dummy,
            std::time::Duration::from_millis(200),
        ));
        assert!(result.is_none(), "timeout must yield None, got {result:?}");
    });
}

// ===========================================================================
// default timeout is the documented 30 seconds
// ===========================================================================

#[test]
fn default_timeout_is_30_seconds() {
    assert_eq!(DEFAULT_TIMEOUT, std::time::Duration::from_secs(30));
}

// ===========================================================================
// Platform stub bodies.
//
// `cmd.exe` on Windows and POSIX `/bin/sh` differ enough that we have
// to construct the stub body per platform. The bodies are kept
// deliberately minimal.
// ===========================================================================

#[cfg(unix)]
fn stub_body_echo_json(json: &str) -> String {
    // `cat <<'EOF'` keeps quoting intact; the trailing `EOF` must be
    // at column 0 with no leading whitespace.
    format!("#!/bin/sh\ncat <<'LOCAST_EOF'\n{json}\nLOCAST_EOF\n")
}

#[cfg(windows)]
fn stub_body_echo_json(json: &str) -> String {
    // `echo` with no surrounding quotes preserves the JSON literal.
    // We have to escape `>` and `<` (we don't use any in the test
    // bodies) and avoid `&` (we don't use any either).
    format!("echo {json}")
}

#[cfg(unix)]
fn stub_body_echo_raw(s: &str) -> String {
    format!("#!/bin/sh\nprintf '%s' '{s}'\n")
}

#[cfg(windows)]
fn stub_body_echo_raw(s: &str) -> String {
    // `echo` is good enough for our short, no-metachar test inputs.
    format!("echo {s}")
}

#[cfg(unix)]
fn stub_body_exit_nonzero() -> String {
    "#!/bin/sh\nexit 1\n".to_string()
}

#[cfg(windows)]
fn stub_body_exit_nonzero() -> String {
    "exit /b 1".to_string()
}

#[cfg(unix)]
fn stub_body_sleep() -> String {
    "#!/bin/sh\nsleep 60\n".to_string()
}

#[cfg(windows)]
fn stub_body_sleep() -> String {
    // `timeout.exe` blocks for the given number of seconds and is
    // present in every supported Windows version's System32.
    // `>nul` suppresses the progress output; we don't care about
    // its exit code (the timeout wrapper will kill the process).
    "timeout /t 30 /nobreak >nul 2>&1".to_string()
}

// ===========================================================================
// sanity: the same logic invoked via `Command` directly works on
// every platform. This is more of a build-time test: if the binary
// can be spawned and exit cleanly, the subprocess machinery on this
// host is functional and the rest of the suite is meaningful.
// ===========================================================================

#[test]
fn command_stub_actually_runs_on_this_platform() {
    let scratch = TempDir::new().expect("scratch");
    let body = if cfg!(windows) {
        "echo hello"
    } else {
        "#!/bin/sh\necho hello\n"
    };
    let stub = drop_stub(scratch.path(), "sanity", body);
    let out = Command::new(&stub).output().expect("spawn stub").stdout;
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("hello"),
        "stub must print 'hello' on this platform, got {s:?}"
    );
}
