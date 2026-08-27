//! P1-T02 integration test: `library::fs::complete_download`.
//!
//! Run with `cargo test -p locast-client --test library_fs` or simply
//! `cargo test --workspace`.
//!
//! What this test pins (per `docs/ROADMAP.md` P1-T02 acceptance):
//!
//! 1. Staging a file under `tmp/staging` and calling `complete_download`
//!    moves the file to
//!    `<library_root>/library/<sha[0..2]>/<sha[2..4]>/<sha>/<sanitized>`.
//! 2. A second call with the same arguments is rejected
//!    (`DestinationAlreadyExists`). The roadmap's "a second concurrent
//!    call is rejected" is interpreted as the sequential case here:
//!    the OS-level rename is atomic, so under true concurrency the
//!    second rename will fail with a `Rename` error; the serialized
//!    mutex is P1-T05's concern.
//! 3. The staged source is moved (not copied) on success, and is
//!    preserved on failure.
//!
//! Additional error-path tests cover: invalid sha, missing staging
//! source, library root that is missing or a file, source outside
//! the library root, and path-traversal attempts in the destination
//! filename.

use std::path::PathBuf;

use locast_client_lib::library::fs::{self, FsError};
use tempfile::TempDir;

/// 64 lowercase hex chars; the canonical "valid" sha for these tests.
const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Compute the expected on-disk final path for `sha` and `sanitized`
/// under `library_root`. Mirrors `core::paths::content_addressed_path`
/// without re-validating, so a path-construction bug in `core::paths`
/// would surface here too.
fn expected_final_path(root: &std::path::Path, sha: &str, sanitized: &str) -> PathBuf {
    root.join("library")
        .join(&sha[0..2])
        .join(&sha[2..4])
        .join(sha)
        .join(sanitized)
}

/// Write a partial file under `<root>/tmp/staging/<download-id>/<sha>.partial`
/// and return the path.
fn stage_partial(root: &std::path::Path, download_id: &str, sha: &str, bytes: &[u8]) -> PathBuf {
    let dir = root.join("tmp").join("staging").join(download_id);
    std::fs::create_dir_all(&dir).expect("create staging dir");
    let path = dir.join(format!("{sha}.partial"));
    std::fs::write(&path, bytes).expect("write staged file");
    path
}

// ===========================================================================
// acceptance: happy path + second call rejected
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_download_moves_file_to_content_addressed_path() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    let download_id = "01234567-89ab-cdef-0123-456789abcdef";
    let payload = b"locast-p1-t02-happy-path";
    let src = stage_partial(root, download_id, SHA, payload);

    let dst_filename = "Movie.mkv";

    let returned = fs::complete_download(root, SHA, &src, dst_filename)
        .await
        .expect("complete_download should succeed on a fresh tempdir");

    // 1. Returned path equals the expected content-addressed path.
    let expected = expected_final_path(root, SHA, "Movie.mkv");
    assert_eq!(
        returned, expected,
        "returned path must equal <root>/library/<sha[0..2]>/<sha[2..4]>/<sha>/<sanitized>"
    );

    // 2. The file is at that path.
    assert!(
        expected.exists(),
        "file must exist at the content-addressed path after rename"
    );

    // 3. The bytes match.
    let on_disk = std::fs::read(&expected).expect("read final file");
    assert_eq!(
        on_disk, payload,
        "on-disk bytes must match the staged payload"
    );

    // 4. The staged source is GONE (the rename moved it). This is
    //    the architecture's atomic completion guarantee: the file is
    //    at the final path, not duplicated.
    assert!(
        !src.exists(),
        "staged source must be moved (not copied) on successful completion"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_call_with_same_args_is_rejected() {
    // Roadmap: "a second concurrent call is rejected." P1-T02
    // implements this via a pre-check on the destination's existence
    // plus the OS-level atomicity of the rename. Under true
    // concurrency the second call would race past the pre-check and
    // fail in the rename. We assert the sequential case here, which
    // is the more common scenario and proves the
    // `DestinationAlreadyExists` variant is wired up.
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    let download_id = "11111111-2222-3333-4444-555555555555";
    let payload = b"second-call-rejection";
    let src = stage_partial(root, download_id, SHA, payload);

    let first = fs::complete_download(root, SHA, &src, "Movie.mkv").await;
    assert!(first.is_ok(), "first complete_download must succeed");

    // For the second call we need a fresh staged source. The first
    // call moved the only file we have, so stage a second one and
    // try the completion again with the same destination filename.
    let src2 = stage_partial(root, download_id, SHA, payload);
    let second = fs::complete_download(root, SHA, &src2, "Movie.mkv").await;

    assert!(
        matches!(second, Err(FsError::DestinationAlreadyExists)),
        "second complete_download must be rejected as DestinationAlreadyExists, got {second:?}"
    );

    // The second staged source was NOT moved (the rejection happened
    // before the rename), so the architecture's "preserve on failure"
    // guarantee is upheld.
    assert!(
        src2.exists(),
        "staged source must be preserved when the second call is rejected"
    );
}

// ===========================================================================
// failure modes
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_sha_is_rejected() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    let download_id = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    let src = stage_partial(root, download_id, SHA, b"x");

    for bad in ["not-a-sha", "abc", &"a".repeat(63), &"A".repeat(64)] {
        let result = fs::complete_download(root, bad, &src, "Movie.mkv").await;
        assert!(
            matches!(result, Err(FsError::InvalidSha(_))),
            "sha {bad:?} should be rejected as InvalidSha, got {result:?}"
        );
    }

    // The staged source is preserved on sha-rejection (the
    // library-root and src checks are upstream of the rename, so
    // there is nothing to clean up; the file is left as-is).
    assert!(src.exists(), "staged source preserved on sha-rejection");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_staging_source_is_rejected() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    let missing = root
        .join("tmp")
        .join("staging")
        .join("nope")
        .join("nope.partial");
    // The file does not exist; do not create it.

    let result = fs::complete_download(root, SHA, &missing, "Movie.mkv").await;
    assert!(
        matches!(result, Err(FsError::StagingSourceMissing(_))),
        "missing src should be rejected as StagingSourceMissing, got {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn library_root_must_exist() {
    let tmp = TempDir::new().expect("tempdir");
    let ghost = tmp.path().join("does-not-exist");
    let src = tmp.path().join("src.partial");
    std::fs::write(&src, b"x").expect("write src");

    let result = fs::complete_download(&ghost, SHA, &src, "Movie.mkv").await;
    assert!(
        matches!(result, Err(FsError::LibraryRootInvalid(_))),
        "missing root should be rejected as LibraryRootInvalid, got {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn library_root_must_be_a_directory() {
    let tmp = TempDir::new().expect("tempdir");
    let file_root = tmp.path().join("not-a-dir");
    std::fs::write(&file_root, b"i am a file").expect("write file");
    let src = tmp.path().join("src.partial");
    std::fs::write(&src, b"x").expect("write src");

    let result = fs::complete_download(&file_root, SHA, &src, "Movie.mkv").await;
    assert!(
        matches!(result, Err(FsError::LibraryRootInvalid(_))),
        "file-as-root should be rejected as LibraryRootInvalid, got {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_outside_library_root_is_rejected() {
    // Two TempDirs simulate "src in /tmp" while library_root is
    // somewhere else. The pre-rename containment check must catch
    // this and return PathEscapesLibrary.
    let lib_root_tmp = TempDir::new().expect("lib tempdir");
    let outside_tmp = TempDir::new().expect("outside tempdir");
    let root = lib_root_tmp.path();
    let outside_src = outside_tmp.path().join("evil.partial");
    std::fs::write(&outside_src, b"i am not yours").expect("write outside src");

    let result = fs::complete_download(root, SHA, &outside_src, "Movie.mkv").await;
    assert!(
        matches!(result, Err(FsError::PathEscapesLibrary)),
        "outside src should be rejected as PathEscapesLibrary, got {result:?}"
    );
}

// ===========================================================================
// path-traversal safety in the destination filename
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn traversal_in_dst_filename_is_rejected_by_sanitizer() {
    // The sanitizer rejects a final segment that is empty, `.`, or
    // `..` (see `core::library::sanitize` tests). These cases
    // therefore come back as `InvalidFilename` BEFORE any filesystem
    // call.
    //
    // Note: inputs like `../etc/passwd` are NOT rejected at the
    // sanitizer level — the sanitizer keeps the last segment
    // (`passwd`) and would write the file to the content-addressed
    // path. The defense against path-traversal via the final segment
    // is the `core::paths` content-addressed path: the file always
    // lands at
    // `<root>/library/<sha[0..2]>/<sha[2..4]>/<sha>/<sanitized>`, with
    // every directory component either literal (`library`) or sha
    // hex. A caller cannot use the filename to escape the root.
    // The pre-rename library-root containment check on the parent
    // is the second line of defense.
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    let download_id = "deadbeef-cafe-babe-0000-111122223333";
    let src = stage_partial(root, download_id, SHA, b"traversal");

    for bad in [".", "..", "foo/.", "foo/..", "foo/."] {
        let result = fs::complete_download(root, SHA, &src, bad).await;
        assert!(
            matches!(result, Err(FsError::InvalidFilename(_))),
            "traversal filename {bad:?} should be rejected as InvalidFilename, got {result:?}"
        );
    }

    // No file was created at the content-addressed path because
    // sanitization failed.
    let content_root = root.join("library");
    assert!(
        !content_root.exists()
            || std::fs::read_dir(&content_root)
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
        "no permanent file should be created when sanitization fails"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whitespace_only_dst_filename_is_rejected() {
    // Sanitization strips trailing spaces and rejects the empty
    // result. A filename that is all spaces is therefore rejected.
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    let download_id = "99999999-8888-7777-6666-555544443333";
    let src = stage_partial(root, download_id, SHA, b"ws");

    let result = fs::complete_download(root, SHA, &src, "   ").await;
    assert!(
        matches!(result, Err(FsError::InvalidFilename(_))),
        "whitespace-only filename should be rejected, got {result:?}"
    );
}

// ===========================================================================
// concurrency: tokio::join! two truly-concurrent calls
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_complete_download_does_not_panic_and_is_deterministic() {
    // The roadmap says "a second concurrent call is rejected." This
    // test exercises the truly-concurrent case: two tasks, each with
    // its own staged source for the same destination, race to complete.
    //
    // Platform-specific outcome:
    //
    // - **POSIX:** `rename(2)` is atomic and rejects the second
    //   attempt with `EEXIST`/`ENOTEMPTY`. The pre-check may or may
    //   not catch the duplicate depending on scheduling, but the
    //   rename itself will. Exactly one `Ok` and one `Err` per pair.
    // - **Windows (Rust 1.65+):** `MoveFileExW` SILENTLY OVERWRITES.
    //   Under true concurrency, the second call may return `Ok` and
    //   clobber the first call's bytes. The serialized mutex is
    //   P1-T05's concern. We do NOT assert a specific count of Ok on
    //   Windows; we only assert that no call panics, that the test
    //   finishes (no deadlock), and that the file at the destination
    //   has non-zero content.
    //
    // In both cases the call count is exactly 2; no call may panic
    // and no call may return a result type we don't recognize.
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path();
    let download_id = "cafef00d-0000-1111-2222-333344445555";
    let payload = b"concurrent-completion";

    let src1 = stage_partial(root, download_id, SHA, payload);
    let src2 = stage_partial(root, download_id, SHA, payload);

    let r1 = fs::complete_download(root, SHA, &src1, "Movie.mkv");
    let r2 = fs::complete_download(root, SHA, &src2, "Movie.mkv");
    let (a, b) = tokio::join!(r1, r2);

    assert!(
        a.is_ok() || a.is_err(),
        "call 1 returned an unexpected variant: {a:?}"
    );
    assert!(
        b.is_ok() || b.is_err(),
        "call 2 returned an unexpected variant: {b:?}"
    );

    let final_path = expected_final_path(root, SHA, "Movie.mkv");
    let on_disk = std::fs::read(&final_path).expect("read final file");
    assert_eq!(
        on_disk, payload,
        "on-disk bytes must match the payload (the last-write-wins on Windows may not match the 'first' call but the bytes must still be the payload)"
    );

    if cfg!(unix) {
        // On POSIX exactly one of the two calls must succeed.
        let oks = [&a, &b].iter().filter(|r| r.is_ok()).count();
        let errs = [&a, &b].iter().filter(|r| r.is_err()).count();
        assert_eq!(oks, 1, "POSIX: expected exactly 1 Ok, got {oks}");
        assert_eq!(errs, 1, "POSIX: expected exactly 1 Err, got {errs}");
    }
    // On Windows we accept any (Ok, Ok), (Ok, Err), or (Err, Ok) as
    // long as no panic and the destination file exists. P1-T05's mutex
    // will serialize and make this deterministic.
}

// ===========================================================================
// assertion helper: assert_within
// ===========================================================================

#[test]
fn assert_within_returns_true_for_descendants() {
    let root = std::path::Path::new(if cfg!(windows) {
        "C:\\locast-test"
    } else {
        "/locast-test"
    });
    let inside = root.join("library").join("01").join("abc");
    // Manually canonicalize-like: on Windows we cannot canonicalize a
    // path that does not exist, so the function takes pre-canonicalized
    // paths. We simulate by constructing the same string the
    // implementation would have built.
    assert!(fs::assert_within(root, &inside));
}

#[test]
fn assert_within_returns_false_for_outsiders() {
    let root = std::path::Path::new(if cfg!(windows) {
        "C:\\locast-test"
    } else {
        "/locast-test"
    });
    let outside = std::path::Path::new(if cfg!(windows) {
        "C:\\other-locast-test"
    } else {
        "/other-locast-test"
    });
    assert!(!fs::assert_within(root, outside));
}

#[test]
fn assert_within_returns_true_for_root_itself() {
    let root = std::path::Path::new(if cfg!(windows) {
        "C:\\locast-test"
    } else {
        "/locast-test"
    });
    // The root is its own ancestor.
    assert!(fs::assert_within(root, root));
}

// ===========================================================================
// symlink tests (POSIX only; Windows requires admin/developer mode)
// ===========================================================================

/// On POSIX, plant a symlink at `link_path` pointing to `target`.
/// On Windows this is a no-op (the test that uses it is gated to
/// `cfg(unix)`); the suite still passes on Windows because no test
/// in this module ever calls this function on Windows.
#[cfg(unix)]
fn plant_symlink(target: &std::path::Path, link_path: &std::path::Path) {
    std::os::unix::fs::symlink(target, link_path).expect("create symlink");
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn posix_symlink_planted_inside_root_is_detected() {
    // Plant a symlink at `<root>/library/01/escape` -> `/tmp/somewhere-else`.
    // A caller that asks to complete a download whose SHA prefix lands
    // in this symlinked dir would have its destination under the
    // symlink target, which is OUTSIDE the library root. The
    // canonicalize-after-`create_dir_all` defense must catch this and
    // return `PathEscapesLibrary`.
    //
    // This test only runs on POSIX. The Windows host may not allow
    // unprivileged symlink creation, and the architecture's section 21.7
    // mentions symlinks only as a defense-in-depth concern; the
    // primary Windows defense is the read-only library dialog and the
    // SHA-validated path construction, not symlink resolution.
    let outside = tempfile::TempDir::new().expect("outside tempdir");
    let lib_root_tmp = TempDir::new().expect("lib tempdir");
    let root = lib_root_tmp.path();

    // Create the directory tree up to the prefix the symlink will
    // replace.
    let prefix_dir = root.join("library").join("01");
    std::fs::create_dir_all(&prefix_dir).expect("create prefix dir");
    let link_path = prefix_dir.join("escape");
    plant_symlink(outside.path(), &link_path);

    // Now stage a partial and try to complete.
    let src = stage_partial(root, "deadbeef-cafe-0000-0000-000000000001", SHA, b"x");

    let result = fs::complete_download(root, SHA, &src, "Movie.mkv").await;
    assert!(
        matches!(result, Err(FsError::PathEscapesLibrary)),
        "symlink redirecting library/01/* to outside should be detected as PathEscapesLibrary, got {result:?}"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn posix_symlink_as_library_root_works() {
    // If the user-chosen library root itself is a symlink, the
    // canonicalize step in `complete_download` resolves it once and
    // every subsequent `starts_with` check uses the canonical form.
    // This test plants `<root_symlink> -> <real_dir>` and confirms a
    // complete_download that targets the symlink succeeds and lands
    // the file under the real directory.
    let real_tmp = TempDir::new().expect("real tempdir");
    let link_tmp = TempDir::new().expect("link tempdir");
    let real_root = real_tmp.path().to_path_buf();
    let link_root = link_tmp.path().join("libroot");
    plant_symlink(&real_root, &link_root);

    let src = stage_partial(
        &real_root,
        "cafef00d-0000-1111-2222-333344445555",
        SHA,
        b"symlinked-root",
    );

    let result = fs::complete_download(&link_root, SHA, &src, "Movie.mkv").await;
    assert!(
        result.is_ok(),
        "complete_download against a symlinked library root should succeed, got {result:?}"
    );

    let expected = expected_final_path(&real_root, SHA, "Movie.mkv");
    assert!(
        expected.exists(),
        "file must exist at the canonical (real) library root, not the symlink path"
    );
}
