//! Pure path construction for the library on-disk layout.
//!
//! Every function in this module is a pure path builder: no I/O, no
//! environment reads, no clock reads, no allocation beyond the returned
//! `PathBuf`. The architecture (section 6) is the source of truth for the
//! layout. P1-T02 wires this module into `library::fs` for atomic
//! completion; later phase-1 tasks will use it for the on-disk scanner
//! and the disk-quota walk.
//!
//! # Layout (architecture section 6)
//!
//! ```text
//! <library_root>/
//!   library/<sha[0..2]>/<sha[2..4]>/<sha>/<sanitized>   # permanent
//!   tmp/staging/<download-id>/<sha>.partial              # awaiting rename
//!   tmp/incomplete/<download-id>/<download-id>.part.<n>  # in-flight chunks
//! ```
//!
//! All three builders validate the user-supplied components
//! (`sha`, `download_id`, `sanitized_filename`) and refuse to construct
//! a path that contains traversal sequences or path separators. The
//! library-root containment check on the filesystem side is the second
//! line of defense; this module is the first.

use std::path::{Path, PathBuf};

/// Errors returned by the path builders in this module.
///
/// Each variant carries the offending input so `Display` and logs can
/// surface what was rejected. The variants are deliberately distinct:
/// `InvalidSha` is a well-formed caller mistake (sha is a public
/// identifier and the caller has a real reason to want to see what was
/// rejected); `InvalidDownloadId` is the same; `InvalidSanitizedFilename`
/// should not happen in normal operation because the sanitizer already
/// rejects path separators, so seeing it means the caller bypassed
/// `core::library::sanitize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// `sha` was not exactly 64 lowercase hex characters. Carries the
    /// offending value.
    InvalidSha(String),

    /// `download_id` was not a UUID-shaped string (lowercase hex + `-`,
    /// no path separators, no `..`). Carries the offending value.
    InvalidDownloadId(String),

    /// `sanitized_filename` contained a path separator. The sanitizer
    /// already strips these, so seeing one here means the caller
    /// bypassed the sanitizer. Carries the offending value.
    InvalidSanitizedFilename(String),
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::InvalidSha(s) => {
                write!(f, "invalid sha256 {s:?}: expected 64 lowercase hex chars")
            }
            PathError::InvalidDownloadId(s) => {
                write!(
                    f,
                    "invalid download id {s:?}: expected uuid-shaped lowercase hex + '-'"
                )
            }
            PathError::InvalidSanitizedFilename(s) => write!(
                f,
                "invalid sanitized filename {s:?}: must not contain path separators"
            ),
        }
    }
}

impl std::error::Error for PathError {}

/// Length of a SHA-256 hex string.
const SHA256_HEX_LEN: usize = 64;

/// Validate that `sha` is exactly 64 lowercase hex characters. Exposed
/// as a public helper so `library::fs` can validate sha without
/// constructing a throwaway content-addressed path.
pub fn validate_sha(sha: &str) -> Result<(), PathError> {
    if sha.len() != SHA256_HEX_LEN {
        return Err(PathError::InvalidSha(sha.to_string()));
    }
    if !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(PathError::InvalidSha(sha.to_string()));
    }
    if sha.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(PathError::InvalidSha(sha.to_string()));
    }
    Ok(())
}

/// Validate that `download_id` is a UUID-shaped string: only lowercase
/// hex digits and `-`, with no `..` and no path separators. We do not
/// require a specific length or hyphenation pattern; v4 UUIDs are
/// 8-4-4-4-12 hex, but the caller is allowed to pass a trimmed prefix
/// for shorter intermediate ids.
fn check_download_id(id: &str) -> Result<(), PathError> {
    if id.is_empty() {
        return Err(PathError::InvalidDownloadId(id.to_string()));
    }
    if id == "." || id == ".." {
        return Err(PathError::InvalidDownloadId(id.to_string()));
    }
    for b in id.bytes() {
        let ok = b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || b == b'-';
        if !ok {
            return Err(PathError::InvalidDownloadId(id.to_string()));
        }
    }
    Ok(())
}

/// Validate that a sanitized filename is a single segment: no `/`, no
/// `\`, no NUL.
fn check_sanitized(name: &str) -> Result<(), PathError> {
    if name.is_empty() {
        return Err(PathError::InvalidSanitizedFilename(name.to_string()));
    }
    for b in name.bytes() {
        if b == b'/' || b == b'\\' || b == 0 {
            return Err(PathError::InvalidSanitizedFilename(name.to_string()));
        }
    }
    Ok(())
}

/// Final content-addressed path for a completed media file.
///
/// Layout: `<library_root>/library/<sha[0..2]>/<sha[2..4]>/<sha>/<sanitized_filename>`.
///
/// Returns `Err(PathError::InvalidSha(...))` if `sha` is not 64 lowercase
/// hex characters, or `Err(PathError::InvalidSanitizedFilename(...))`
/// if `sanitized_filename` contains a path separator. The function
/// never touches the filesystem.
pub fn content_addressed_path(
    library_root: &Path,
    sha: &str,
    sanitized_filename: &str,
) -> Result<PathBuf, PathError> {
    validate_sha(sha)?;
    check_sanitized(sanitized_filename)?;

    let mut p = library_root.to_path_buf();
    p.push("library");
    // sha is exactly 64 ASCII bytes; slicing on byte indices is safe
    // (no UTF-8 boundary can fall inside an ASCII byte).
    p.push(&sha[0..2]);
    p.push(&sha[2..4]);
    p.push(sha);
    p.push(sanitized_filename);
    Ok(p)
}

/// Staging partial path: `<library_root>/tmp/staging/<download_id>/<sha>.partial`.
///
/// Returns `Err` on invalid `sha` or `download_id`. Pure.
///
/// This builder is not called by `library::fs::complete_download` itself
/// (which is the only P1-T02 consumer); it is provided so the downloader
/// that will land in a later phase (P2 download scheduler, P3 chunk
/// scheduler) can construct staging paths without re-implementing the
/// validation rules.
pub fn staging_partial_path(
    library_root: &Path,
    download_id: &str,
    sha: &str,
) -> Result<PathBuf, PathError> {
    validate_sha(sha)?;
    check_download_id(download_id)?;

    let mut p = library_root.to_path_buf();
    p.push("tmp");
    p.push("staging");
    p.push(download_id);
    p.push(format!("{sha}.partial"));
    Ok(p)
}

/// In-flight chunk path: `<library_root>/tmp/incomplete/<download_id>/<download_id>.part.<chunk_index>`.
///
/// Returns `Err` on invalid `download_id`. The chunk index is a
/// non-negative `u32`; any value is accepted. Pure.
pub fn incomplete_chunk_path(
    library_root: &Path,
    download_id: &str,
    chunk_index: u32,
) -> Result<PathBuf, PathError> {
    check_download_id(download_id)?;

    let mut p = library_root.to_path_buf();
    p.push("tmp");
    p.push("incomplete");
    p.push(download_id);
    p.push(format!("{download_id}.part.{chunk_index}"));
    Ok(p)
}

/// Derive the library root from a `Storage` path (the
/// `<root>/index.sqlite` file). The library root is the
/// parent directory of the storage file, per the layout
/// in the module-level comment (`<library_root>/library/...`).
///
/// This is the canonical helper used by every command
/// that needs to read or write content-addressed media
/// (e.g. `media_import`, `library_scan`, `quota_get`,
/// `manifest_publish`).
///
/// Returns `None` if the storage path has no parent
/// (which should be impossible in practice — the storage
/// file always has a parent directory).
pub fn library_root_for(storage_path: &Path) -> Option<PathBuf> {
    storage_path.parent().map(|p| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/library")
    }

    fn valid_sha() -> &'static str {
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    }

    fn other_sha() -> &'static str {
        "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"
    }

    // ----- sha validation

    #[test]
    fn content_addressed_path_accepts_valid_sha() {
        let p = content_addressed_path(&root(), valid_sha(), "Movie.mkv").unwrap();
        assert_eq!(
            p,
            PathBuf::from("/library/library/01/23/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/Movie.mkv")
        );
    }

    #[test]
    fn content_addressed_path_rejects_short_sha() {
        assert_eq!(
            content_addressed_path(&root(), "abc", "Movie.mkv"),
            Err(PathError::InvalidSha("abc".to_string()))
        );
    }

    #[test]
    fn content_addressed_path_rejects_non_hex_sha() {
        let s = "z".repeat(64);
        assert_eq!(
            content_addressed_path(&root(), &s, "Movie.mkv"),
            Err(PathError::InvalidSha(s.clone()))
        );
    }

    #[test]
    fn content_addressed_path_rejects_uppercase_sha() {
        let s = "A".repeat(64);
        assert_eq!(
            content_addressed_path(&root(), &s, "Movie.mkv"),
            Err(PathError::InvalidSha(s.clone()))
        );
    }

    #[test]
    fn content_addressed_path_rejects_64_with_one_uppercase() {
        let mut s = "a".repeat(63);
        s.push('A');
        assert_eq!(
            content_addressed_path(&root(), &s, "Movie.mkv"),
            Err(PathError::InvalidSha(s.clone()))
        );
    }

    #[test]
    fn content_addressed_path_rejects_empty_sha() {
        assert_eq!(
            content_addressed_path(&root(), "", "Movie.mkv"),
            Err(PathError::InvalidSha("".to_string()))
        );
    }

    // ----- sanitized filename validation

    #[test]
    fn content_addressed_path_rejects_slash_in_sanitized() {
        assert_eq!(
            content_addressed_path(&root(), valid_sha(), "foo/bar"),
            Err(PathError::InvalidSanitizedFilename("foo/bar".to_string()))
        );
    }

    #[test]
    fn content_addressed_path_rejects_backslash_in_sanitized() {
        assert_eq!(
            content_addressed_path(&root(), valid_sha(), "foo\\bar"),
            Err(PathError::InvalidSanitizedFilename("foo\\bar".to_string()))
        );
    }

    #[test]
    fn content_addressed_path_rejects_empty_sanitized() {
        assert_eq!(
            content_addressed_path(&root(), valid_sha(), ""),
            Err(PathError::InvalidSanitizedFilename("".to_string()))
        );
    }

    #[test]
    fn content_addressed_path_accepts_sanitized_with_dot() {
        let p = content_addressed_path(&root(), valid_sha(), "Movie.s01e02.mkv").unwrap();
        assert!(p.ends_with("Movie.s01e02.mkv"));
    }

    // ----- download id validation

    #[test]
    fn staging_partial_path_accepts_uuid() {
        let id = "01234567-89ab-cdef-0123-456789abcdef";
        let p = staging_partial_path(&root(), id, valid_sha()).unwrap();
        assert_eq!(
            p,
            PathBuf::from(format!(
                "/library/tmp/staging/{id}/{sha}.partial",
                sha = valid_sha()
            ))
        );
    }

    #[test]
    fn staging_partial_path_rejects_empty_download_id() {
        assert_eq!(
            staging_partial_path(&root(), "", valid_sha()),
            Err(PathError::InvalidDownloadId("".to_string()))
        );
    }

    #[test]
    fn staging_partial_path_rejects_dot_download_id() {
        assert_eq!(
            staging_partial_path(&root(), ".", valid_sha()),
            Err(PathError::InvalidDownloadId(".".to_string()))
        );
    }

    #[test]
    fn staging_partial_path_rejects_dotdot_download_id() {
        assert_eq!(
            staging_partial_path(&root(), "..", valid_sha()),
            Err(PathError::InvalidDownloadId("..".to_string()))
        );
    }

    #[test]
    fn staging_partial_path_rejects_uppercase_download_id() {
        // We require lowercase hex; uppercase is not allowed.
        assert_eq!(
            staging_partial_path(&root(), "ABCDEF", valid_sha()),
            Err(PathError::InvalidDownloadId("ABCDEF".to_string()))
        );
    }

    #[test]
    fn staging_partial_path_rejects_path_separator_in_id() {
        assert_eq!(
            staging_partial_path(&root(), "foo/bar", valid_sha()),
            Err(PathError::InvalidDownloadId("foo/bar".to_string()))
        );
        assert_eq!(
            staging_partial_path(&root(), "foo\\bar", valid_sha()),
            Err(PathError::InvalidDownloadId("foo\\bar".to_string()))
        );
    }

    // ----- validate_sha exposed helper

    #[test]
    fn validate_sha_accepts_valid() {
        assert!(validate_sha(valid_sha()).is_ok());
    }

    #[test]
    fn validate_sha_rejects_each_invalid_kind() {
        for bad in ["", "abc", &"a".repeat(63), &"A".repeat(64), &"z".repeat(64)] {
            assert_eq!(
                validate_sha(bad),
                Err(PathError::InvalidSha(bad.to_string()))
            );
        }
    }

    #[test]
    fn incomplete_chunk_path_uses_index() {
        let id = "01234567-89ab-cdef-0123-456789abcdef";
        let p = incomplete_chunk_path(&root(), id, 7).unwrap();
        assert_eq!(
            p,
            PathBuf::from(format!("/library/tmp/incomplete/{id}/{id}.part.7"))
        );
    }

    #[test]
    fn incomplete_chunk_path_zero_index() {
        let id = "01234567-89ab-cdef-0123-456789abcdef";
        let p = incomplete_chunk_path(&root(), id, 0).unwrap();
        // `Path::ends_with` is a component comparison, and on
        // Windows a leading `.` is treated as a hidden-file prefix
        // rather than a normal component, so compare the final
        // component via the OsStr instead.
        let last = p.file_name().expect("file_name");
        assert_eq!(last.to_string_lossy(), format!("{id}.part.0"));
    }

    // ----- display + error traits

    #[test]
    fn path_error_display() {
        assert_eq!(
            format!("{}", PathError::InvalidSha("ABC".to_string())),
            "invalid sha256 \"ABC\": expected 64 lowercase hex chars"
        );
        assert_eq!(
            format!("{}", PathError::InvalidDownloadId("XYZ".to_string())),
            "invalid download id \"XYZ\": expected uuid-shaped lowercase hex + '-'"
        );
        assert_eq!(
            format!("{}", PathError::InvalidSanitizedFilename("a/b".to_string())),
            "invalid sanitized filename \"a/b\": must not contain path separators"
        );
    }

    #[test]
    fn path_error_is_std_error() {
        let e = PathError::InvalidSha("abc".to_string());
        let _src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
    }

    // ----- pure: no I/O side effects when the root doesn't exist

    #[test]
    fn builders_do_not_touch_filesystem() {
        // A clearly non-existent root path; if the builders touch the
        // filesystem, this would fail on Windows due to the missing
        // drive. The builders must not.
        let missing = PathBuf::from("Z:\\definitely\\not\\a\\real\\path");
        let _ = content_addressed_path(&missing, valid_sha(), "Movie.mkv").unwrap();
        let _ = staging_partial_path(&missing, "deadbeef", valid_sha()).unwrap();
        let _ = incomplete_chunk_path(&missing, "deadbeef", 0).unwrap();
    }

    #[test]
    fn library_root_for_returns_parent_of_storage() {
        // A typical layout: <app_data>/<library>/index.sqlite
        // -> library_root = <app_data>/<library>.
        let storage = PathBuf::from("/appdata/mylib/index.sqlite");
        assert_eq!(
            library_root_for(&storage),
            Some(PathBuf::from("/appdata/mylib"))
        );
    }

    #[test]
    fn library_root_for_returns_empty_for_root_relative_storage() {
        // A relative path like "index.sqlite" has a
        // parent that is the empty string "" (the current
        // directory). The helper returns `Some("")` in
        // that case; callers should treat that as "the
        // current directory".
        let storage = PathBuf::from("index.sqlite");
        assert_eq!(library_root_for(&storage), Some(PathBuf::from("")));
    }

    #[test]
    fn different_shas_use_different_prefix_dirs() {
        let a = content_addressed_path(&root(), valid_sha(), "Movie.mkv").unwrap();
        let b = content_addressed_path(&root(), other_sha(), "Movie.mkv").unwrap();
        assert_ne!(a, b);
        // First two components under library/ differ.
        let comps_a: Vec<_> = a.iter().collect();
        let comps_b: Vec<_> = b.iter().collect();
        assert_ne!(comps_a[comps_a.len() - 4], comps_b[comps_b.len() - 4]);
        assert_ne!(comps_a[comps_a.len() - 3], comps_b[comps_b.len() - 3]);
    }
}
