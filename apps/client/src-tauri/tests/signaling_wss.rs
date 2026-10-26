//! wss:// against real public TLS endpoints, through the client's own build.
//!
//! The signaling WebSocket used to be built without a TLS feature, so every
//! wss:// URL failed before a byte left the machine. These tests prove that
//! the client now completes a TLS handshake with a publicly trusted server
//! and that certificate validation is really on: an expired certificate and
//! a certificate for another host are refused with a certificate error, not
//! with some unrelated failure (DNS, refused connection, timeout).
//!
//! Every test needs the internet, so all of them are `#[ignore]` and neither
//! CI nor `cargo test --workspace` runs them. Run them with:
//!
//! ```text
//! cargo test -p locast-client --test signaling_wss -- --ignored --nocapture
//! ```
//!
//! `LOCAST_WSS_TEST_URL` overrides the echo endpoint (default
//! `wss://echo.websocket.org`). Set `RUST_LOG=debug` to see the client's logs.
//!
//! Bad-certificate endpoints: `expired-rsa-dv.ssl.com` (SSL.com's expired
//! test site), `8-8-8-8.nip.io` (nip.io resolves it to 8.8.8.8, which
//! presents Google's `dns.google` certificate, so the name does not match)
//! and `self-signed.badssl.com`. badssl.com only speaks TLS 1.2, so that
//! last one also proves the client's rustls build has tls12 enabled: without
//! it the handshake fails with an alert before any certificate is checked.
//!
//! Two layers are covered:
//! - The real `SignalingClient` connect loop, pointed at the endpoint. It
//!   cannot complete a Locast handshake with an echo server, but the
//!   `last_error` it records says which step failed: a TLS refusal is recorded
//!   by the connect step as `NetworkUnreachable: IO error: invalid peer
//!   certificate: ...`, while a connection that got through TLS fails later,
//!   in the Locast handshake.
//! - `tokio_tungstenite::connect_async` called directly, to send and receive a
//!   frame and to match on the exact rustls error. It is called from a test
//!   in this crate on purpose: Cargo unifies features per build, so this is
//!   the same tokio-tungstenite / rustls build the client links, TLS backend
//!   and root store included. The client's `ensure_crypto_provider` is
//!   private, so these tests install the same ring provider themselves; an
//!   `Err` from that only means it is already installed, so test order does
//!   not matter.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use locast_client_lib::identity::keystore::{IdentityKeyring, IdentityService, MockKeyring};
use locast_client_lib::net::config::SignalingConfig;
use locast_client_lib::net::signaling::SignalingClient;
use locast_client_lib::storage::Storage;
use locast_protocol::handshake::Platform;
use rustls::CertificateError;
use tokio_tungstenite::tungstenite::error::TlsError;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

const TIMEOUT: Duration = Duration::from_secs(15);

/// A public endpoint whose certificate must be refused, and how.
struct BadCert {
    url: &'static str,
    /// Text the rustls error message must contain.
    message: &'static str,
    /// The certificate error rustls must report.
    matches: fn(&CertificateError) -> bool,
}

const BAD_CERTS: &[BadCert] = &[
    BadCert {
        url: "wss://expired-rsa-dv.ssl.com/",
        message: "certificate expired",
        matches: |e| {
            matches!(
                e,
                CertificateError::Expired | CertificateError::ExpiredContext { .. }
            )
        },
    },
    BadCert {
        url: "wss://8-8-8-8.nip.io/",
        message: "certificate not valid for name",
        matches: |e| {
            matches!(
                e,
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. }
            )
        },
    },
    BadCert {
        url: "wss://self-signed.badssl.com/",
        message: "UnknownIssuer",
        matches: |e| matches!(e, CertificateError::UnknownIssuer),
    },
];

fn echo_url() -> String {
    std::env::var("LOCAST_WSS_TEST_URL").unwrap_or_else(|_| "wss://echo.websocket.org".into())
}

fn init() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    if std::env::var("RUST_LOG").is_ok() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .try_init();
    }
}

/// Points a real `SignalingClient` at `url`, lets it make one connect
/// attempt, and returns the `last_error` that attempt recorded.
async fn first_signaling_error(url: &str) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let storage = Storage::open(&dir.path().join("index.sqlite"))
        .await
        .expect("storage");
    let keyring: Arc<dyn IdentityKeyring> = Arc::new(MockKeyring::new());
    let identity = Arc::new(IdentityService::with_keyring(keyring, storage));
    identity.get_or_create("tester").await.expect("identity");
    let signaling = SignalingClient::new(
        SignalingConfig::new_for_test(url, TIMEOUT, 1024 * 1024, Platform::Linux),
        identity,
    );
    signaling.start().await.expect("start");
    // Connect and the Locast handshake each get TIMEOUT; leave room for both.
    let deadline = tokio::time::Instant::now() + TIMEOUT * 2 + Duration::from_secs(5);
    let error = loop {
        if let Some(e) = signaling.snapshot().await.last_error {
            break e;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no connect outcome from {url}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    signaling.shutdown().await;
    error
}

/// The rustls error behind a failed connect, if TLS is what failed.
/// tokio-rustls reports handshake failures as an `io::Error` wrapping the
/// rustls error; `Error::Tls` is checked too in case that ever changes.
fn rustls_error(err: &WsError) -> Option<&rustls::Error> {
    match err {
        WsError::Tls(TlsError::Rustls(e)) => Some(e),
        WsError::Io(io) => io.get_ref().and_then(|e| e.downcast_ref::<rustls::Error>()),
        _ => None,
    }
}

#[tokio::test]
#[ignore = "needs internet: real public TLS endpoints"]
async fn signaling_client_gets_through_tls_to_a_public_server() {
    init();
    let url = echo_url();
    let error = first_signaling_error(&url).await;
    println!("{url}: {error}");
    // An echo server is not a Locast server, so the attempt must fail, but
    // only after the connect step: TLS and the WebSocket upgrade succeeded.
    assert!(
        !error.starts_with("NetworkUnreachable") && !error.contains("connect timeout"),
        "the connect step to {url} failed: {error}"
    );
}

#[tokio::test]
#[ignore = "needs internet: real public TLS endpoints"]
async fn signaling_client_refuses_bad_certificates() {
    init();
    for bad in BAD_CERTS {
        let error = first_signaling_error(bad.url).await;
        println!("{}: {error}", bad.url);
        assert!(
            error.starts_with("NetworkUnreachable: IO error: invalid peer certificate:"),
            "{} was not refused on its certificate (DNS, timeout or other failure?): {error}",
            bad.url
        );
        assert!(
            error.contains(bad.message),
            "{} was refused for the wrong certificate reason: {error}",
            bad.url
        );
    }
}

#[tokio::test]
#[ignore = "needs internet: real public TLS endpoints"]
async fn wss_echo_round_trips_a_text_frame() {
    init();
    let url = echo_url();
    let (mut ws, resp) =
        match tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(&url)).await {
            Err(_) => panic!("connect to {url} timed out"),
            Ok(Err(e)) => panic!("connect to {url} failed: {e:?}"),
            Ok(Ok(pair)) => pair,
        };
    println!("{url}: upgrade status {}", resp.status());

    let payload = format!("locast wss probe {}", uuid::Uuid::new_v4());
    ws.send(Message::Text(payload.clone()))
        .await
        .expect("send text frame");

    // The echo server may greet first; read until our payload comes back.
    let echoed = tokio::time::timeout(TIMEOUT, async {
        while let Some(msg) = ws.next().await {
            match msg.expect("read frame") {
                Message::Text(text) if text == payload => return true,
                other => println!("{url}: skipping frame {other:?}"),
            }
        }
        false
    })
    .await
    .expect("echo within the timeout");
    assert!(echoed, "{url} closed before echoing the payload");
    let _ = ws.close(None).await;
}

#[tokio::test]
#[ignore = "needs internet: real public TLS endpoints"]
async fn wss_connect_rejects_bad_certificates_with_a_certificate_error() {
    init();
    for bad in BAD_CERTS {
        let url = bad.url;
        let err = match tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(url)).await {
            Err(_) => panic!("connect to {url} timed out; that is not a TLS refusal"),
            Ok(Ok(_)) => panic!("{url} was accepted: certificate validation is off"),
            Ok(Err(e)) => e,
        };
        match rustls_error(&err) {
            Some(rustls::Error::InvalidCertificate(cert)) => {
                println!("{url}: refused, invalid certificate: {cert:?}");
                assert!(
                    (bad.matches)(cert),
                    "{url} was refused for the wrong certificate reason: {cert:?}"
                );
            }
            Some(other) => panic!("{url} failed in TLS, but not on its certificate: {other:?}"),
            None => panic!("{url} failed, but not in TLS: {err:?}"),
        }
    }
}
