//! P1-T10 tests for the `locast://` handler as the webview actually uses it:
//! the URL forms a webview sends, file names that need percent-encoding, and
//! bounded `Range` responses for seeking.
//!
//! Run with `cargo test -p locast-client --test protocol_playback -j 1`.

use locast_client_lib::library::protocol::{
    encode_segment, resolve_media_url, stream_range, LocastUrl, ProtocolHandler, ProtocolResponse,
    ResponseBody, MAX_RANGE_BYTES,
};
use locast_client_lib::storage::Storage;
use sha2::{Digest, Sha256};
use std::path::Path;
use tempfile::TempDir;

struct Fixture {
    storage: Storage,
    handler: ProtocolHandler,
    root: std::path::PathBuf,
    _dir: TempDir,
}

async fn fixture() -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path().to_path_buf();
    let storage = Storage::open(root.join("index.sqlite"))
        .await
        .expect("storage opens");
    let handler = ProtocolHandler::new(storage.clone(), root.clone());
    Fixture {
        storage,
        handler,
        root,
        _dir: dir,
    }
}

/// Write `bytes` at the content-addressed path under `filename` and insert
/// the matching `media_items` row. Returns `(id, sha256)`.
async fn add_media(f: &Fixture, filename: &str, bytes: &[u8]) -> (String, String) {
    let sha = hex::encode(Sha256::digest(bytes));
    let rel = format!("library/{}/{}/{}/{}", &sha[..2], &sha[2..4], sha, filename);
    let abs = f.root.join(&rel);
    tokio::fs::create_dir_all(abs.parent().expect("parent"))
        .await
        .expect("mkdir");
    tokio::fs::write(&abs, bytes).await.expect("write media");
    let id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO media_items (id, sha256, blake3, size_bytes, filename, relative_path, \
         mime, status, created_at, last_seen_at, provenance) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'video/mp4', 'permanent', 1, 1, '{}')",
    )
    .bind(&id)
    .bind(&sha)
    .bind(blake3::hash(bytes).to_hex().to_string())
    .bind(bytes.len() as i64)
    .bind(filename)
    .bind(&rel)
    .execute(&f.storage.pool())
    .await
    .expect("insert row");
    (id, sha)
}

fn header<'a>(r: &'a ProtocolResponse, name: &str) -> Option<&'a str> {
    r.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The bytes a `206` response would put on the wire, read the way the Tauri
/// adapter reads them.
async fn range_bytes(path: &Path, start: u64, length: u64) -> Vec<u8> {
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    stream_range(path, start, length, &mut cursor)
        .await
        .expect("stream_range");
    cursor.into_inner()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_encodes_awkward_file_names_and_the_handler_serves_them() {
    let f = fixture().await;
    // ASCII, because the library path validator (P8-T01) rejects non-ASCII
    // paths; names that need percent-encoding are still common.
    let name = "Movie Night #1 (final) & more.mp4";
    let bytes: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8).collect();
    let (id, _) = add_media(&f, name, &bytes).await;

    let url = resolve_media_url(&f.storage, &id).await.expect("resolve");
    assert!(url.starts_with("locast://media/"), "{url}");
    assert!(
        !url.contains(' ') && !url.contains('#'),
        "not encoded: {url}"
    );
    assert!(url.ends_with(&encode_segment(name)), "{url}");

    let resp = f.handler.handle(&url, "GET", None).await.expect("serve");
    assert_eq!(resp.status, 200);
    assert_eq!(header(&resp, "Content-Type"), Some("video/mp4"));
    assert_eq!(header(&resp, "Content-Length"), Some("2048"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_webview_url_form_reaches_the_same_media() {
    let f = fixture().await;
    let bytes = vec![7u8; 4096];
    let (id, sha) = add_media(&f, "Clip One.mp4", &bytes).await;
    let tail = format!("media/{}/Clip%20One.mp4", &sha[..16]);
    assert!(resolve_media_url(&f.storage, &id)
        .await
        .expect("resolve")
        .ends_with(&tail));

    for url in [
        format!("locast://{tail}"),
        format!("locast://localhost/{tail}"),
        format!("http://locast.localhost/{tail}"),
        format!("https://locast.localhost/{tail}"),
        format!("http://locast.localhost/{tail}?t=1#frag"),
    ] {
        let resp = f
            .handler
            .handle(&url, "GET", Some("bytes=0-99"))
            .await
            .unwrap_or_else(|e| panic!("{url}: {e:?}"));
        assert_eq!(resp.status, 206, "{url}");
        assert_eq!(
            header(&resp, "Content-Range"),
            Some("bytes 0-99/4096"),
            "{url}"
        );
    }
}

#[test]
fn encoded_separators_and_traversal_are_refused() {
    let sha = "0123456789abcdef";
    for bad in [
        format!("locast://media/{sha}/..%2F..%2Fsecret.mp4"),
        format!("locast://media/{sha}/%2e%2e"),
        format!("locast://media/{sha}/a%2Fb.mp4"),
        format!("locast://media/{sha}/a%5Cb.mp4"),
        format!("locast://media/{sha}/a%00b.mp4"),
        format!("locast://media/{sha}/%FF.mp4"),
        format!("locast://media/{sha}/"),
        format!("http://locast.localhost/media/{sha}/%2E%2E"),
        "http://example.com/media/0123456789abcdef/a.mp4".to_string(),
    ] {
        assert!(LocastUrl::parse(&bad).is_err(), "must refuse {bad}");
    }
    // Non-ASCII is decoded correctly at the URL layer.
    assert_eq!(
        LocastUrl::parse(&format!("locast://media/{sha}/Am%C3%A9lie.mkv")).expect("valid"),
        LocastUrl::Media {
            sha_prefix: sha.to_string(),
            filename: "Am\u{e9}lie.mkv".to_string()
        }
    );
    assert_eq!(
        LocastUrl::parse(&format!("locast://media/{sha}/a%20b.mp4")).expect("valid"),
        LocastUrl::Media {
            sha_prefix: sha.to_string(),
            filename: "a b.mp4".to_string()
        }
    );
}

/// 20 MiB of position-dependent bytes: every offset holds `offset % 251`, so a
/// slice read from the wrong place is detectable.
fn patterned(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_ended_range_is_capped_and_seeking_reads_only_the_requested_window() {
    let f = fixture().await;
    let total: u64 = 20 * 1024 * 1024;
    let bytes = patterned(total as usize);
    let (id, sha) = add_media(&f, "Big.mp4", &bytes).await;
    let url = resolve_media_url(&f.storage, &id).await.expect("resolve");
    let on_disk = f.root.join(format!(
        "library/{}/{}/{}/Big.mp4",
        &sha[..2],
        &sha[2..4],
        sha
    ));

    // The browser's first request: `Range: bytes=0-` (to end of file).
    let first = f
        .handler
        .handle(&url, "GET", Some("bytes=0-"))
        .await
        .expect("first range");
    assert_eq!(first.status, 206);
    let cap = MAX_RANGE_BYTES;
    assert_eq!(
        header(&first, "Content-Length"),
        Some(cap.to_string().as_str())
    );
    assert_eq!(
        header(&first, "Content-Range"),
        Some(format!("bytes 0-{}/{total}", cap - 1).as_str())
    );
    match first.body {
        ResponseBody::Range { start, length, .. } => {
            assert_eq!((start, length), (0, cap), "served window is bounded");
        }
        _ => panic!("expected a Range body"),
    }

    // A seek to the middle: open-ended again, but starting at 10 MiB.
    let mid = 10 * 1024 * 1024u64 + 123;
    let seek = f
        .handler
        .handle(&url, "GET", Some(&format!("bytes={mid}-")))
        .await
        .expect("seek range");
    assert_eq!(seek.status, 206);
    let (s, l) = match seek.body {
        ResponseBody::Range { start, length, .. } => (start, length),
        _ => panic!("expected a Range body"),
    };
    assert_eq!(s, mid, "range starts at the seek target, not at 0");
    assert_eq!(l, cap);
    assert_eq!(
        header(&seek, "Content-Range"),
        Some(format!("bytes {mid}-{}/{total}", mid + cap - 1).as_str())
    );
    // The window really comes from that offset of the file.
    let got = range_bytes(&on_disk, s, 4096).await;
    assert_eq!(got, bytes[s as usize..s as usize + 4096]);

    // A seek near the end returns just the tail (shorter than the cap).
    let tail = f
        .handler
        .handle(&url, "GET", Some(&format!("bytes={}-", total - 1000)))
        .await
        .expect("tail range");
    assert_eq!(header(&tail, "Content-Length"), Some("1000"));
    assert_eq!(
        header(&tail, "Content-Range"),
        Some(format!("bytes {}-{}/{total}", total - 1000, total - 1).as_str())
    );

    // An explicit small range is untouched; a suffix range is untouched.
    let small = f
        .handler
        .handle(&url, "GET", Some("bytes=5-14"))
        .await
        .expect("small range");
    assert_eq!(
        header(&small, "Content-Range"),
        Some(format!("bytes 5-14/{total}").as_str())
    );
    let suffix = f
        .handler
        .handle(&url, "GET", Some("bytes=-500"))
        .await
        .expect("suffix range");
    assert_eq!(header(&suffix, "Content-Length"), Some("500"));

    // Past the end is still 416.
    let past = f
        .handler
        .handle(&url, "GET", Some(&format!("bytes={total}-")))
        .await
        .expect("past end");
    assert_eq!(past.status, 416);
}
