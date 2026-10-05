//! The Settings page's server address: validation, persistence and the
//! saved / active / next-launch view, against a real SQLite file.

use locast_client_lib::commands::settings::{read_server_settings, write_server_url};
use locast_client_lib::net::config::{DEFAULT_URL, ENV_URL, SETTING_URL_KEY};
use locast_client_lib::storage::{settings, Storage};
use tempfile::TempDir;

const ACTIVE: &str = "ws://127.0.0.1:8787/ws";

/// `next_url` follows the env var when it is set, which would make the
/// expectations below depend on the machine running the tests.
fn env_override_set() -> bool {
    std::env::var(ENV_URL).is_ok_and(|s| !s.trim().is_empty())
}

async fn open() -> (TempDir, Storage) {
    let tmp = TempDir::new().expect("tempdir");
    let storage = Storage::open(&tmp.path().join("index.sqlite"))
        .await
        .expect("open storage");
    (tmp, storage)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_configured_on_a_fresh_install() {
    let (_tmp, storage) = open().await;
    let s = read_server_settings(&storage, ACTIVE).await.expect("read");
    assert_eq!(s.configured_url, None);
    assert_eq!(s.active_url, ACTIVE);
    if !env_override_set() {
        assert_eq!(s.next_url, DEFAULT_URL);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_address_is_saved_normalized_and_survives_a_reopen() {
    let (tmp, storage) = open().await;
    let saved = write_server_url(
        &storage,
        ACTIVE,
        Some("  wss://locast.example.com/ws ".into()),
    )
    .await
    .expect("save");
    assert_eq!(
        saved.configured_url.as_deref(),
        Some("wss://locast.example.com/ws")
    );
    assert_eq!(saved.active_url, ACTIVE, "the running client is unchanged");
    if !env_override_set() {
        assert_eq!(
            saved.next_url, "wss://locast.example.com/ws",
            "the next launch uses the saved address"
        );
    }

    drop(storage);
    let reopened = Storage::open(&tmp.path().join("index.sqlite"))
        .await
        .expect("reopen");
    let again = read_server_settings(&reopened, ACTIVE).await.expect("read");
    assert_eq!(
        again.configured_url.as_deref(),
        Some("wss://locast.example.com/ws")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_address_names_the_reason_and_writes_nothing() {
    let (_tmp, storage) = open().await;
    write_server_url(&storage, ACTIVE, Some("wss://good.example.com/ws".into()))
        .await
        .expect("save");

    for (bad, reason) in [
        ("ws://remote.example.com/ws", "only allowed for localhost"),
        ("http://remote.example.com/ws", "must start with wss://"),
        (
            "wss://user:pw@remote.example.com/ws",
            "user name or password",
        ),
        ("wss://remote.example.com", "needs a path"),
        ("nonsense", "not a valid URL"),
    ] {
        let err = write_server_url(&storage, ACTIVE, Some(bad.into()))
            .await
            .expect_err(bad);
        let text = format!("{err:?}");
        assert!(
            text.contains(reason),
            "{bad:?} should be refused because of {reason:?}, got {text}"
        );
        let s = read_server_settings(&storage, ACTIVE).await.expect("read");
        assert_eq!(
            s.configured_url.as_deref(),
            Some("wss://good.example.com/ws"),
            "{bad:?} must leave the saved address untouched"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn none_or_a_blank_address_clears_the_setting() {
    let (_tmp, storage) = open().await;
    for clear in [None, Some(String::new()), Some("   ".to_string())] {
        write_server_url(&storage, ACTIVE, Some("wss://good.example.com/ws".into()))
            .await
            .expect("save");
        let cleared = write_server_url(&storage, ACTIVE, clear.clone())
            .await
            .expect("clear");
        assert_eq!(cleared.configured_url, None, "{clear:?}");
        if !env_override_set() {
            assert_eq!(cleared.next_url, DEFAULT_URL, "{clear:?}");
        }
    }
}

/// The app is running on a saved address and the user clears it: the next
/// launch uses the default, so `next_url` differs from `active_url` and the
/// page must say a restart is needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clearing_while_running_on_a_saved_address_needs_a_restart() {
    if env_override_set() {
        return;
    }
    let (_tmp, storage) = open().await;
    let running_on = "wss://good.example.com/ws";
    let cleared = write_server_url(&storage, running_on, None)
        .await
        .expect("clear");
    assert_eq!(cleared.active_url, running_on);
    assert_eq!(cleared.next_url, DEFAULT_URL);
    assert_ne!(cleared.next_url, cleared.active_url);
}

/// A value edited straight into the database is not trusted: the page
/// shows what is stored, but the next launch falls back to the default.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tampered_stored_value_cannot_become_the_next_address() {
    if env_override_set() {
        return;
    }
    let (_tmp, storage) = open().await;
    settings::set_json(&storage, SETTING_URL_KEY, &"ws://evil.example.com/ws")
        .await
        .expect("write raw");
    let s = read_server_settings(&storage, ACTIVE).await.expect("read");
    assert_eq!(
        s.configured_url.as_deref(),
        Some("ws://evil.example.com/ws")
    );
    assert_eq!(
        s.next_url, DEFAULT_URL,
        "an address that fails validation must never be connected to"
    );
}

/// A damaged row (not a JSON string) must not break the page: it reads as
/// "not configured", and saving overwrites it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_damaged_stored_value_reads_as_not_configured_and_can_be_replaced() {
    let (_tmp, storage) = open().await;
    settings::set_raw(&storage, SETTING_URL_KEY, "{not json")
        .await
        .expect("write raw");
    let s = read_server_settings(&storage, ACTIVE).await.expect("read");
    assert_eq!(s.configured_url, None);

    let fixed = write_server_url(&storage, ACTIVE, Some("wss://good.example.com/ws".into()))
        .await
        .expect("replace");
    assert_eq!(
        fixed.configured_url.as_deref(),
        Some("wss://good.example.com/ws")
    );
}
