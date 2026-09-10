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
//! # Path Validation (P8-T01)
//!
//! The `validate_library_path` function is the single entry point for
//! path validation per architecture section 21.7. It consolidates all
//! traversal and malicious filename checks. It is the single entry
//! point for validating a relative path against the library root.

use std::path::{Path, PathBuf};
#[allow(unused_imports)]
use unicode_normalization::UnicodeNormalization;

/// Errors returned by the path builders in this module.
///
/// Each variant carries the offending input so `Display` and logs can
/// surface what was rejected. The variants are deliberately distinct:
/// `InvalidSha` is a well-formed caller mistake (sha is a public
/// identifier and the caller has a real reason to want to see what was
/// rejected); `InvalidDownloadId` is the same; `InvalidSanitizedFilename`
/// should not happen in normal operation because the sanitizer already
/// rejects path separators, so seeing it here means the caller bypassed
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
    /// already strips these, so seeing it here means the caller bypassed
    /// `core::library::sanitize`. Carries the offending value.
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

/// Errors returned by the library path validator.
///
/// This validator consolidates all path traversal and malicious
/// filename checks per architecture section 21.7. It is the single
/// entry point for validating a relative path against the library
/// root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryPathError {
    /// The relative path is empty.
    Empty,
    /// The path contains an absolute component (starts with `/` or `\`,
    /// or on Windows a drive letter prefix).
    AbsolutePath,
    /// The path contains a `..` segment (traversal attempt).
    ParentTraversal,
    /// The path contains a backslash separator (only forward slashes
    /// are allowed per architecture section 21.7).
    BackslashSeparator,
    /// A path segment is a Windows reserved name (CON, PRN, AUX,
    /// NUL, COM1-9, LPT1-9) regardless of extension.
    ReservedName(String),
    /// A path segment contains a NUL byte.
    NulByte,
    /// A path segment exceeds 255 bytes (max filename length).
    SegmentTooLong(String),
    /// The total path exceeds 4096 bytes.
    PathTooLong,
    /// The path contains non-ASCII characters (only ASCII allowed).
    NonAscii(String),
    /// The path segment contains control characters (0x00-0x1F, 0x7F).
    ControlCharacter(String),
    /// Unicode is not in NFC normalization form.
    NotNfc(String),
    /// The resolved absolute path escapes the library root (symlink
    /// or junction pointing outside).
    EscapesLibraryRoot,
    /// The path is not a regular file (directory, symlink, etc.).
    NotAFile,
    /// I/O error during canonicalization or metadata check.
    IoError(String),
    /// The file was not found.
    NotFound,
}

impl std::fmt::Display for LibraryPathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LibraryPathError::Empty => write!(f, "path is empty"),
            LibraryPathError::AbsolutePath => write!(f, "path is absolute"),
            LibraryPathError::ParentTraversal => write!(f, "path contains parent traversal (..)"),
            LibraryPathError::BackslashSeparator => write!(f, "path contains backslash separator"),
            LibraryPathError::ReservedName(s) => write!(f, "path contains reserved name: {s}"),
            LibraryPathError::NulByte => write!(f, "path contains NUL byte"),
            LibraryPathError::SegmentTooLong(s) => write!(f, "path segment too long: {s}"),
            LibraryPathError::PathTooLong => write!(f, "total path too long (>4096 bytes)"),
            LibraryPathError::NonAscii(s) => write!(f, "path contains non-ASCII: {s}"),
            LibraryPathError::ControlCharacter(s) => {
                write!(f, "path contains control character: {s}")
            }
            LibraryPathError::NotNfc(s) => write!(f, "path not NFC normalized: {s}"),
            LibraryPathError::EscapesLibraryRoot => write!(f, "resolved path escapes library root"),
            LibraryPathError::NotAFile => write!(f, "path is not a regular file"),
            LibraryPathError::IoError(s) => write!(f, "I/O error: {s}"),
            LibraryPathError::NotFound => write!(f, "file not found"),
        }
    }
}

impl std::error::Error for LibraryPathError {}

/// Validate a relative library path and return the canonical absolute path.
///
/// This is the single entry point for path validation per architecture
/// section 21.7. It performs all traversal and malicious filename checks:
/// - Empty path rejected
/// - Absolute paths rejected (including drive letters, UNC)
/// - Parent traversal (`..`) rejected
/// - Backslash separators rejected
/// - Windows reserved names rejected (CON, PRN, AUX, NUL, COM1-9, LPT1-9)
/// - NUL bytes rejected
/// - Segment length > 255 bytes rejected
/// - Total path > 4096 bytes rejected
/// - Non-ASCII characters rejected
/// - Control characters (0x00-0x1F, 0x7F) rejected
/// - Non-NFC Unicode rejected
/// - Symlink/junction escaping library root rejected
/// - Non-regular files (directories, symlinks) rejected
///
/// On success, returns the canonical absolute path of the file.
/// The caller must still verify the file exists and is a regular file.
///
/// This function is async because it needs to canonicalize paths
/// and check filesystem metadata (symlink resolution).
pub async fn validate_library_path(
    library_root: &Path,
    rel_path: &str,
) -> Result<PathBuf, LibraryPathError> {
    // 1. Empty path
    if rel_path.is_empty() {
        return Err(LibraryPathError::Empty);
    }

    // 2. Total path length (4096 bytes max per architecture section 21.7)
    if rel_path.len() > 4096 {
        return Err(LibraryPathError::PathTooLong);
    }

    // 2. Absolute path check (including drive letters, UNC paths)
    if rel_path.starts_with('/')
        || rel_path.starts_with('\\')
        || rel_path.len() >= 2 && rel_path.as_bytes()[1] == b':'
    {
        return Err(LibraryPathError::AbsolutePath);
    }

    // 3. Backslash separator check (only forward slashes allowed)
    if rel_path.contains('\\') {
        return Err(LibraryPathError::BackslashSeparator);
    }

    // 4. NUL byte check
    if rel_path.contains('\0') {
        return Err(LibraryPathError::NulByte);
    }

    // 5. Control characters (0x00-0x1F, 0x7F)
    if rel_path.bytes().any(|b| b <= 0x1F || b == 0x7F) {
        return Err(LibraryPathError::ControlCharacter(rel_path.to_string()));
    }

    // 5. Non-ASCII check
    if !rel_path.is_ascii() {
        return Err(LibraryPathError::NonAscii(rel_path.to_string()));
    }

    // 6. NFC normalization check
    if rel_path
        != unicode_normalization::UnicodeNormalization::nfc(rel_path.chars()).collect::<String>()
    {
        return Err(LibraryPathError::NotNfc(rel_path.to_string()));
    }

    // 7. Split into segments and validate each
    let segments: Vec<&str> = rel_path.split('/').collect();
    if segments.is_empty() {
        return Err(LibraryPathError::Empty);
    }

    for seg in &segments {
        // Empty segment (double slash, leading/trailing slash)
        if seg.is_empty() {
            return Err(LibraryPathError::AbsolutePath);
        }

        // Segment length check (255 bytes max per filename)
        if seg.len() > 255 {
            return Err(LibraryPathError::SegmentTooLong(seg.to_string()));
        }

        // Traversal check
        if *seg == "." || *seg == ".." {
            return Err(LibraryPathError::ParentTraversal);
        }

        // Windows reserved names (case-insensitive)
        let upper = seg.to_ascii_uppercase();
        let reserved = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        // Check with and without extension
        let base = upper.split('.').next().unwrap_or("");
        if reserved.contains(&base) {
            return Err(LibraryPathError::ReservedName(seg.to_string()));
        }

        // Control characters in segment
        if seg.bytes().any(|b| b <= 0x1F || b == 0x7F) {
            return Err(LibraryPathError::ControlCharacter(seg.to_string()));
        }

        // Non-ASCII in segment
        if !seg.is_ascii() {
            return Err(LibraryPathError::NonAscii(seg.to_string()));
        }
    }

    // Total path length (after reconstruction with separators)
    let total_len =
        segments.iter().map(|s| s.len()).sum::<usize>() + segments.len().saturating_sub(1);
    if total_len > 4096 {
        return Err(LibraryPathError::PathTooLong);
    }

    // Reconstruct the path for filesystem operations
    let mut abs = library_root.to_path_buf();
    for seg in &segments {
        abs.push(seg);
    }

    // Canonicalize library root and target path
    let canonical_root = tokio::fs::canonicalize(library_root)
        .await
        .map_err(|e| LibraryPathError::IoError(e.to_string()))?;

    let canonical = match tokio::fs::canonicalize(&abs).await {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(LibraryPathError::NotFound);
        }
        Err(e) => {
            return Err(LibraryPathError::IoError(e.to_string()));
        }
    };

    // Library-root containment check (defeats symlink/junction escape)
    if !canonical.starts_with(&canonical_root) {
        return Err(LibraryPathError::EscapesLibraryRoot);
    }

    // Verify it's a regular file (not directory, symlink, etc.)
    let meta = tokio::fs::metadata(&abs)
        .await
        .map_err(|e| LibraryPathError::IoError(e.to_string()))?;
    if !meta.is_file() {
        return Err(LibraryPathError::NotAFile);
    }

    Ok(canonical)
}

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
/// `\`, no NUL. Public so callers that already hold a sanitized filename
/// (e.g. `library::dedup::exists_at_canonical_path`) can re-validate it
/// without re-running the full sanitizer.
pub fn check_sanitized(name: &str) -> Result<(), PathError> {
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
