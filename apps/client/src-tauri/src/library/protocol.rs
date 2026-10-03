//! `library::protocol` - the `locast://` custom URI scheme.
//!
//! P1-T08 implements the architecture's section-5 protocol. The
//! webview blocks `file://` URLs in `<video src>`, `<img src>`,
//! and `fetch()`; the `locast://` scheme routes those requests
//! through Tauri so we can:
//!
//! 1. Enforce library-root containment (no `..`, no symlink
//!    escape) by resolving the URL to a `media_items` (or
//!    `media_subtitles`) row in SQLite and reading ITS
//!    `relative_path`. The webview never sees the on-disk path.
//! 2. Set `Content-Type` from a static extension map.
//! 3. Support HTTP `Range` (single range, v1) with proper
//!    206 / 416 responses.
//! 4. Set `Content-Length`, `Accept-Ranges: bytes`, and
//!    `Cache-Control: no-store`.
//!
//! # URL shapes
//!
//! - Media: `locast://media/<sha256-hex-prefix[0..16]>/<filename>`
//! - Subtitle: `locast://subtitles/<sub-id>/<filename>`
//! - Sidecar metadata: `locast://meta/<media-id>/locast.json`
//!   (optional; mostly for debugging).
//!
//! The protocol is registered with Tauri via
//! `tauri::Builder::register_asynchronous_uri_scheme_protocol("locast", handler)`.
//! The handler in this module is `pub async fn serve(...)` and
//! returns a `Response<Body>` (or `tauri::http::Response<Vec<u8>>`).
//!
//! See `docs/ARCHITECTURE.md` section 5 and
//! `docs/ROADMAP.md` P1-T08.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::path::PathBuf;
use std::sync::Arc;

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::Mutex;

use crate::core::paths::{validate_library_path, validate_sha, LibraryPathError};
use crate::storage::Storage;

/// Errors raised by the `locast://` protocol. The enum is closed
/// and the variants are the contract between the protocol module
/// and the Tauri command surface (which maps them to
/// `AppError`).
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// The URL did not match any `locast://` shape. Equivalent
    /// to HTTP 400.
    #[error("invalid locast:// URL: {0}")]
    BadUrl(String),

    /// The URL's id (or sha prefix) does not resolve to a row in
    /// the local library. Equivalent to HTTP 404.
    #[error("not found: {0}")]
    NotFound(String),

    /// The `Range` header is malformed or unsatisfiable.
    /// Equivalent to HTTP 416.
    #[error("invalid or unsatisfiable Range: {0}")]
    BadRange(String),

    /// The resolved path lives outside the library root. This is
    /// a defense-in-depth tripwire; the URL handler should not
    /// be able to construct such a path because it always
    /// resolves through the DB.
    #[error("path escapes the library root: {0}")]
    OutOfLibrary(String),

    /// An I/O error while serving a request.
    #[error("locast:// io error: {0}")]
    Io(#[from] std::io::Error),

    /// A storage (SQLite) error. Flattened to a string.
    #[error("storage error: {0}")]
    Storage(String),

    /// A path-construction error. Flattened to a string.
    #[error("path error: {0}")]
    Paths(String),
}

/// MIME type table. The map is conservative: extensions not in
/// the table fall through to `application/octet-stream`. New
/// types are added only after the architecture's content-type
/// rules are updated.
pub fn mime_for_ext(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp4" => "video/mp4",
        "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" => "audio/ogg",
        "opus" => "audio/ogg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        "srt" => "application/x-subrip",
        "vtt" | "webvtt" => "text/vtt",
        "ass" | "ssa" => "text/x-ssa",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

/// The largest slice served for one `Range` request (P1-T10). Tauri's
/// response body is an in-memory `Vec<u8>`, and browsers open media with
/// `Range: bytes=0-` (to end of file), so an uncapped range would read the
/// whole file into memory. The handler answers with a shorter `206` whose
/// `Content-Range` states what was actually sent; the media element then asks
/// for the next slice as playback or seeking needs it.
pub const MAX_RANGE_BYTES: u64 = 8 * 1024 * 1024;

/// Characters left unescaped in a generated path segment: the RFC 3986
/// "unreserved" set.
const SEGMENT_ESCAPE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

/// Percent-encode one URL path segment (a file name) so the generated URL is
/// valid even for names with spaces or non-ASCII characters.
pub fn encode_segment(segment: &str) -> String {
    utf8_percent_encode(segment, SEGMENT_ESCAPE).to_string()
}

/// If `url` is one of the forms a webview can send for this scheme, return
/// the part after the scheme and authority (`media/<sha>/<name>`):
///
/// - `locast://media/...`            (what `media_resolve_url` returns)
/// - `locast://localhost/media/...`  (macOS and Linux webviews)
/// - `http://locast.localhost/media/...` and the `https` form (Windows and
///   Android webviews, which cannot navigate to a custom scheme directly)
fn strip_scheme(url: &str) -> Option<&str> {
    for prefix in [
        "locast://localhost/",
        "http://locast.localhost/",
        "https://locast.localhost/",
        "locast://",
    ] {
        if let Some(rest) = url.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    None
}

/// Percent-decode one segment and refuse anything that could change which
/// file is addressed: empty segments, path separators, NUL, or `..`.
fn decode_segment(raw: &str) -> Result<String, ProtocolError> {
    let decoded = percent_decode_str(raw)
        .decode_utf8()
        .map_err(|_| ProtocolError::BadUrl("segment is not valid UTF-8".into()))?
        .into_owned();
    if decoded.is_empty() {
        return Err(ProtocolError::BadUrl("empty segment".into()));
    }
    if decoded.contains("..") {
        return Err(ProtocolError::BadUrl("traversal sequence".into()));
    }
    if decoded.contains(['/', '\\', '\0']) {
        return Err(ProtocolError::BadUrl("separator in segment".into()));
    }
    Ok(decoded)
}

/// A parsed `locast://` URL. The handler constructs one of these
/// before touching the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocastUrl {
    /// `locast://media/<sha-prefix>/<filename>`
    Media {
        sha_prefix: String,
        filename: String,
    },
    /// `locast://subtitles/<sub-id>/<filename>`
    Subtitle { sub_id: String, filename: String },
    /// `locast://meta/<media-id>/<sidecar-name>` (optional, debug)
    Meta { media_id: String, name: String },
}

impl LocastUrl {
    /// Parse a `locast://` URL. Returns `BadUrl` if the scheme
    /// is not `locast`, the host is not one of the known shapes,
    /// or any segment is empty / contains a `..` traversal.
    pub fn parse(url: &str) -> Result<Self, ProtocolError> {
        let rest = strip_scheme(url)
            .ok_or_else(|| ProtocolError::BadUrl(format!("not a locast:// URL: {url:?}")))?;
        // A media element never adds one, but tolerate a query or fragment.
        let rest = rest.split(['?', '#']).next().unwrap_or("");
        // The remainder looks like `media/<sha-prefix>/<filename>` or
        // `subtitles/<sub-id>/<filename>` or `meta/<media-id>/<name>`.
        // Tauri's URI handler also strips the leading slash before
        // calling us, so accept either form.
        let trimmed = rest.trim_start_matches('/');
        let mut parts = trimmed.splitn(3, '/');
        let host = parts
            .next()
            .ok_or_else(|| ProtocolError::BadUrl("empty host".into()))?;
        let seg1 = parts
            .next()
            .ok_or_else(|| ProtocolError::BadUrl("missing first segment".into()))?;
        let seg2 = parts
            .next()
            .ok_or_else(|| ProtocolError::BadUrl("missing filename segment".into()))?;
        let seg1 = decode_segment(seg1)?;
        let seg2 = decode_segment(seg2)?;
        match host {
            "media" => Ok(LocastUrl::Media {
                sha_prefix: seg1,
                filename: seg2,
            }),
            "subtitles" => Ok(LocastUrl::Subtitle {
                sub_id: seg1,
                filename: seg2,
            }),
            "meta" => Ok(LocastUrl::Meta {
                media_id: seg1,
                name: seg2.to_string(),
            }),
            _ => Err(ProtocolError::BadUrl(format!("unknown host {host:?}"))),
        }
    }
}

/// The body of a `locast://` response. Three shapes:
/// - `Full(Vec<u8>)` - the entire file in memory. Used for small
///   files (subtitles, sidecars) and for the meta endpoint.
/// - `Range { path, start, length }` - a 206 response that
///   streams a window from disk.
/// - `File(PathBuf)` - a 200 response that streams the entire
///   file from disk. Used for media without a Range request.
#[derive(Debug, Clone)]
pub enum ResponseBody {
    Full(Vec<u8>),
    Range {
        path: PathBuf,
        start: u64,
        length: u64,
    },
    File(PathBuf),
}

/// A minimal response shape that mirrors `tauri::http::Response`
/// enough for our needs. We return a tuple of `(status, headers,
/// body)` and let the Tauri adapter assemble the final response
/// in `app.rs`. Keeping the shape small makes the unit tests
/// trivial.
#[derive(Debug)]
pub struct ProtocolResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: ResponseBody,
}

/// The protocol handler. Holds a `Storage` clone (cheap) and a
/// `library_root` for the containment check. Both are wrapped in
/// `Arc` so the handler can be installed as a Tauri URI scheme
/// handler and called from any thread.
///
/// The handler is `async` (Tauri's `register_asynchronous_uri_scheme_protocol`
/// expects a `Future<Output = Response<Body>>`); we resolve the
/// DB row on the calling task and then `spawn_blocking` for the
/// file read so we do not block the Tauri main runtime.
#[derive(Clone)]
pub struct ProtocolHandler {
    inner: Arc<ProtocolHandlerInner>,
}

struct ProtocolHandlerInner {
    storage: Storage,
    library_root: PathBuf,
    /// Concurrency guard for the per-(media-id) range read. The
    /// Tauri URI handler may be invoked concurrently for the
    /// same media_id (e.g. the webview seeks and re-fetches);
    /// the guard serializes the open/seek/read sequence so two
    /// concurrent reads do not race on a single `tokio::fs::File`
    /// handle. (We open a fresh `File` per request; the guard is
    /// a cheap way to avoid contention on the storage pool and
    /// to bound in-flight requests at one per media_id.)
    per_media_lock: Mutex<()>,
}

impl ProtocolHandler {
    /// Construct a new handler. The library root is the parent
    /// of the SQLite file (matches the convention in
    /// `commands::import::resolve_library_root`).
    pub fn new(storage: Storage, library_root: PathBuf) -> Self {
        Self {
            inner: Arc::new(ProtocolHandlerInner {
                storage,
                library_root,
                per_media_lock: Mutex::new(()),
            }),
        }
    }

    /// Handle a single `locast://` request. Returns
    /// `Err(ProtocolError)` for malformed URLs and DB lookups;
    /// returns `Ok(ProtocolResponse)` for every reachable file
    /// (including 404/416 in-band).
    pub async fn handle(
        &self,
        url: &str,
        method: &str,
        range_header: Option<&str>,
    ) -> Result<ProtocolResponse, ProtocolError> {
        // Some Tauri versions hand us the URL without the
        // `locast://` scheme; tolerate that by prepending it.
        let url = if strip_scheme(url).is_some() {
            url.to_string()
        } else {
            format!("locast://{url}")
        };
        let parsed = LocastUrl::parse(&url)?;
        if !method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD") {
            return Ok(error_response(405, "method not allowed"));
        }
        let _g = self.inner.per_media_lock.lock().await;
        match parsed {
            LocastUrl::Media {
                sha_prefix,
                filename,
            } => self.serve_media(&sha_prefix, &filename, range_header).await,
            LocastUrl::Subtitle { sub_id, filename } => {
                self.serve_subtitle(&sub_id, &filename, range_header).await
            }
            LocastUrl::Meta { media_id, name } => self.serve_meta(&media_id, &name).await,
        }
    }

    async fn serve_media(
        &self,
        sha_prefix: &str,
        filename: &str,
        range_header: Option<&str>,
    ) -> Result<ProtocolResponse, ProtocolError> {
        if sha_prefix.len() != 16 || !sha_prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ProtocolError::BadUrl(format!(
                "media sha prefix must be 16 hex chars: {sha_prefix:?}"
            )));
        }
        // Look up the row by sha256 prefix and filename. The
        // architecture pins the URL to a sha prefix (not a
        // media_id UUID) for short, opaque URLs.
        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT id, sha256, relative_path FROM media_items \
             WHERE substr(sha256, 1, 16) = ?1 AND filename = ?2",
        )
        .bind(sha_prefix)
        .bind(filename)
        .fetch_optional(&self.inner.storage.pool())
        .await
        .map_err(|e| ProtocolError::Storage(e.to_string()))?;
        let (_id, sha, rel_path) = row.ok_or_else(|| {
            ProtocolError::NotFound(format!("no media_items row for {sha_prefix}/{filename}"))
        })?;
        self.serve_path(&rel_path, &sha, filename, range_header)
            .await
    }

    async fn serve_subtitle(
        &self,
        sub_id: &str,
        filename: &str,
        range_header: Option<&str>,
    ) -> Result<ProtocolResponse, ProtocolError> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT media_id, relative_path FROM media_subtitles WHERE id = ?1 AND filename = ?2",
        )
        .bind(sub_id)
        .bind(filename)
        .fetch_optional(&self.inner.storage.pool())
        .await
        .map_err(|e| ProtocolError::Storage(e.to_string()))?;
        let (media_id, rel_path) = row.ok_or_else(|| {
            ProtocolError::NotFound(format!("no media_subtitles row for {sub_id}/{filename}"))
        })?;
        // The subtitle's `relative_path` is relative to the
        // media's content-addressed directory. Look up the
        // media's sha to resolve the absolute path.
        let sha: Option<(String,)> = sqlx::query_as("SELECT sha256 FROM media_items WHERE id = ?1")
            .bind(&media_id)
            .fetch_optional(&self.inner.storage.pool())
            .await
            .map_err(|e| ProtocolError::Storage(e.to_string()))?;
        let sha = sha
            .map(|(s,)| s)
            .ok_or_else(|| ProtocolError::NotFound(format!("media {media_id} not found")))?;
        self.serve_path(&rel_path, &sha, filename, range_header)
            .await
    }

    async fn serve_meta(
        &self,
        media_id: &str,
        name: &str,
    ) -> Result<ProtocolResponse, ProtocolError> {
        if name != "locast.json" {
            return Err(ProtocolError::BadUrl(format!("unknown meta name {name:?}")));
        }
        let row: Option<(String, String, i64)> =
            sqlx::query_as("SELECT sha256, filename, size_bytes FROM media_items WHERE id = ?1")
                .bind(media_id)
                .fetch_optional(&self.inner.storage.pool())
                .await
                .map_err(|e| ProtocolError::Storage(e.to_string()))?;
        let (sha, filename, size_bytes) = row.ok_or_else(|| {
            ProtocolError::NotFound(format!("no media_items row for id {media_id}"))
        })?;
        // Synthesize a sidecar JSON from the DB row. We do NOT
        // touch the filesystem; the sidecar file is for
        // recovery only and the DB is authoritative.
        let json = serde_json::json!({
            "schema": 1,
            "media_id": media_id,
            "sha256": sha,
            "size_bytes": size_bytes,
            "filename": filename,
        });
        let bytes = serde_json::to_vec(&json).map_err(|e| ProtocolError::Storage(e.to_string()))?;
        Ok(ProtocolResponse {
            status: 200,
            headers: vec![
                ("Content-Type".into(), mime_for_ext("json").into()),
                ("Content-Length".into(), bytes.len().to_string()),
                ("Cache-Control".into(), "no-store".into()),
            ],
            body: ResponseBody::Full(bytes),
        })
    }

    /// The shared "resolve a relative_path under the library
    /// root and serve the file" logic. Used by media and
    /// subtitle URLs.
    async fn serve_path(
        &self,
        rel_path: &str,
        sha: &str,
        filename: &str,
        range_header: Option<&str>,
    ) -> Result<ProtocolResponse, ProtocolError> {
        // P8-T01: Use consolidated path validator (section 21.7)
        // This consolidates all traversal and malicious filename checks.
        // Only validation rejects are 403. A DB row whose file is
        // missing on disk stays 404, I/O failures stay `Io`, and a
        // non-regular file stays 400, matching the pre-P8-T01
        // behavior (architecture section 5, custom protocol, step 3).
        let canonical = validate_library_path(&self.inner.library_root, rel_path)
            .await
            .map_err(|e| match e {
                LibraryPathError::NotFound => {
                    ProtocolError::NotFound(format!("file not on disk: {rel_path}"))
                }
                LibraryPathError::IoError(m) => ProtocolError::Io(std::io::Error::other(m)),
                LibraryPathError::NotAFile => {
                    ProtocolError::BadUrl(format!("not a regular file: {rel_path}"))
                }
                other => ProtocolError::OutOfLibrary(other.to_string()),
            })?;

        // Validate sha as defense in depth
        validate_sha(sha).map_err(|e| ProtocolError::Paths(e.to_string()))?;

        let meta = tokio::fs::metadata(&canonical)
            .await
            .map_err(ProtocolError::Io)?;
        if !meta.is_file() {
            return Err(ProtocolError::BadUrl(format!(
                "not a regular file: {}",
                canonical.display()
            )));
        }
        let total_size = meta.len();
        let mime = mime_for_ext(
            std::path::Path::new(filename)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or(""),
        );
        let base_headers: Vec<(String, String)> = vec![
            ("Content-Type".into(), mime.to_string()),
            ("Accept-Ranges".into(), "bytes".into()),
            ("Cache-Control".into(), "no-store".into()),
        ];
        // Parse the Range header. v1 only supports a single
        // range (`bytes=START-END` or `bytes=START-` or
        // `bytes=-SUFFIX`). Multi-range is rejected with 416.
        let range = match range_header {
            None => None,
            Some(s) => match parse_single_range(s, total_size) {
                Ok(r) => Some(r),
                Err(e) => {
                    return Ok(ProtocolResponse {
                        status: 416,
                        headers: {
                            let mut h = base_headers;
                            h.push(("Content-Range".into(), format!("bytes */{total_size}")));
                            h
                        },
                        body: ResponseBody::Full(e.into_bytes()),
                    });
                }
            },
        };
        match range {
            None => Ok(ProtocolResponse {
                status: 200,
                headers: {
                    let mut h = base_headers;
                    h.push(("Content-Length".into(), total_size.to_string()));
                    h
                },
                body: ResponseBody::File(canonical),
            }),
            Some((start, end_inclusive)) => {
                // Never serve more than MAX_RANGE_BYTES per response; see the
                // constant's docs. A short 206 is valid and self-describing.
                let end_inclusive = end_inclusive.min(start.saturating_add(MAX_RANGE_BYTES - 1));
                let length = end_inclusive - start + 1;
                Ok(ProtocolResponse {
                    status: 206,
                    headers: {
                        let mut h = base_headers;
                        h.push(("Content-Length".into(), length.to_string()));
                        h.push((
                            "Content-Range".into(),
                            format!("bytes {start}-{end_inclusive}/{total_size}"),
                        ));
                        h
                    },
                    body: ResponseBody::Range {
                        path: canonical,
                        start,
                        length,
                    },
                })
            }
        }
    }
}

/// Parse a single-range `Range` header. Returns
/// `(start, end_inclusive)` on success. The end is inclusive
/// (so the response is `Content-Length: end - start + 1`).
///
/// v1 supports only `bytes=START-END` and `bytes=START-` (open
/// range, end is EOF) and `bytes=-SUFFIX` (last N bytes).
/// Multi-range (`bytes=0-10,20-30`) is rejected.
pub fn parse_single_range(header: &str, total_size: u64) -> Result<(u64, u64), String> {
    let s = header.trim();
    if !s.starts_with("bytes=") {
        return Err(format!("unsupported range unit in {s:?}"));
    }
    let spec = &s["bytes=".len()..];
    if spec.contains(',') {
        return Err("multi-range is not supported in v1".to_string());
    }
    if let Some((start_s, end_s)) = spec.split_once('-') {
        if start_s.is_empty() {
            // Suffix form: `-N` means the last N bytes.
            let n: u64 = end_s
                .parse()
                .map_err(|e| format!("invalid suffix length: {e}"))?;
            if n == 0 {
                return Err("zero-length suffix range".into());
            }
            if n > total_size {
                return Err("suffix range larger than file".into());
            }
            let start = total_size - n;
            return Ok((start, total_size - 1));
        }
        let start: u64 = start_s
            .parse()
            .map_err(|e| format!("invalid range start: {e}"))?;
        let end: u64 = if end_s.is_empty() {
            total_size.saturating_sub(1)
        } else {
            end_s
                .parse()
                .map_err(|e| format!("invalid range end: {e}"))?
        };
        if start > end {
            return Err(format!("start {start} > end {end}"));
        }
        if start >= total_size {
            return Err(format!("start {start} >= size {total_size}"));
        }
        Ok((start, end.min(total_size - 1)))
    } else {
        Err(format!("malformed range: {s:?}"))
    }
}

fn error_response(status: u16, msg: &str) -> ProtocolResponse {
    ProtocolResponse {
        status,
        headers: vec![("Content-Type".into(), "text/plain; charset=utf-8".into())],
        body: ResponseBody::Full(msg.as_bytes().to_vec()),
    }
}

/// Resolve a `media_id` to a `locast://` URL. Used by the
/// `media_resolve_url` Tauri command (see `commands::protocol`).
pub async fn resolve_media_url(storage: &Storage, media_id: &str) -> Result<String, ProtocolError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT substr(sha256, 1, 16), filename FROM media_items WHERE id = ?1")
            .bind(media_id)
            .fetch_optional(&storage.pool())
            .await
            .map_err(|e| ProtocolError::Storage(e.to_string()))?;
    let (sha_prefix, filename) =
        row.ok_or_else(|| ProtocolError::NotFound(format!("media_id {media_id} not found")))?;
    Ok(format!(
        "locast://media/{sha_prefix}/{}",
        encode_segment(&filename)
    ))
}

/// Resolve a `subtitle_id` to a `locast://` URL.
pub async fn resolve_subtitle_url(
    storage: &Storage,
    subtitle_id: &str,
) -> Result<String, ProtocolError> {
    let row: Option<(String, String)> =
        sqlx::query_as("SELECT media_id, filename FROM media_subtitles WHERE id = ?1")
            .bind(subtitle_id)
            .fetch_optional(&storage.pool())
            .await
            .map_err(|e| ProtocolError::Storage(e.to_string()))?;
    let (_media_id, filename) =
        row.ok_or_else(|| ProtocolError::NotFound(format!("subtitle_id {subtitle_id} not found")))?;
    Ok(format!(
        "locast://subtitles/{subtitle_id}/{}",
        encode_segment(&filename)
    ))
}

/// Stream a range of `path` into `out`. This is the helper the
/// Tauri adapter uses for the 206 path. It opens a fresh
/// `tokio::fs::File`, seeks to `start`, and copies `length` bytes
/// into `out` using a 1 MiB buffer.
pub async fn stream_range(
    path: &std::path::Path,
    start: u64,
    length: u64,
    mut out: impl tokio::io::AsyncWrite + Unpin,
) -> Result<(), std::io::Error> {
    let mut f = tokio::fs::File::open(path).await?;
    f.seek(std::io::SeekFrom::Start(start)).await?;
    let mut remaining = length;
    let mut buf = vec![0u8; 1024 * 1024];
    while remaining > 0 {
        let to_read = buf.len().min(remaining as usize);
        let n = f.read(&mut buf[..to_read]).await?;
        if n == 0 {
            break;
        }
        tokio::io::AsyncWriteExt::write_all(&mut out, &buf[..n]).await?;
        remaining -= n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_map_is_stable() {
        assert_eq!(mime_for_ext("mp4"), "video/mp4");
        assert_eq!(mime_for_ext("MP4"), "video/mp4");
        assert_eq!(mime_for_ext("mkv"), "video/x-matroska");
        assert_eq!(mime_for_ext("webm"), "video/webm");
        assert_eq!(mime_for_ext("srt"), "application/x-subrip");
        assert_eq!(mime_for_ext("vtt"), "text/vtt");
        assert_eq!(mime_for_ext("ass"), "text/x-ssa");
        assert_eq!(mime_for_ext("ssa"), "text/x-ssa");
        assert_eq!(mime_for_ext("unknown"), "application/octet-stream");
    }

    #[test]
    fn url_parse_media() {
        let u = LocastUrl::parse("locast://media/0123456789abcdef/Movie.mkv").unwrap();
        assert_eq!(
            u,
            LocastUrl::Media {
                sha_prefix: "0123456789abcdef".into(),
                filename: "Movie.mkv".into(),
            }
        );
    }

    #[test]
    fn url_parse_subtitle() {
        let u = LocastUrl::parse("locast://subtitles/sub-uuid/Movie.en.srt").unwrap();
        assert_eq!(
            u,
            LocastUrl::Subtitle {
                sub_id: "sub-uuid".into(),
                filename: "Movie.en.srt".into(),
            }
        );
    }

    #[test]
    fn url_parse_meta() {
        let u = LocastUrl::parse("locast://meta/media-uuid/locast.json").unwrap();
        assert_eq!(
            u,
            LocastUrl::Meta {
                media_id: "media-uuid".into(),
                name: "locast.json".into(),
            }
        );
    }

    #[test]
    fn url_parse_rejects_traversal() {
        assert!(LocastUrl::parse("locast://media/0123456789abcdef/..").is_err());
        assert!(LocastUrl::parse("locast://media/../foo").is_err());
    }

    #[test]
    fn url_parse_rejects_unknown_host() {
        assert!(LocastUrl::parse("locast://wat/foo/bar").is_err());
    }

    #[test]
    fn url_parse_rejects_wrong_scheme() {
        assert!(LocastUrl::parse("file:///etc/passwd").is_err());
    }

    #[test]
    fn range_start_end() {
        assert_eq!(parse_single_range("bytes=0-1023", 4096).unwrap(), (0, 1023));
    }

    #[test]
    fn range_open_end() {
        assert_eq!(parse_single_range("bytes=100-", 4096).unwrap(), (100, 4095));
    }

    #[test]
    fn range_suffix() {
        assert_eq!(
            parse_single_range("bytes=-100", 4096).unwrap(),
            (3996, 4095)
        );
    }

    #[test]
    fn range_multi_is_rejected() {
        assert!(parse_single_range("bytes=0-10,20-30", 4096).is_err());
    }

    #[test]
    fn range_unsatisfiable_start_past_eof() {
        assert!(parse_single_range("bytes=10000-20000", 4096).is_err());
    }

    #[test]
    fn range_malformed() {
        assert!(parse_single_range("garbage", 4096).is_err());
        assert!(parse_single_range("bytes=abc-def", 4096).is_err());
    }
}
