//! P2-T08: storage-layer tests for the `recent_rooms` table.
//!
//! These tests exercise the SQL idempotency contract that the
//! `/rooms` page relies on:
//!
//! - Insert a new row.
//! - Upsert the same `room_id` and assert no duplicate.
//! - Upsert with `last_ended_ms = Some(now)` and assert the row
//!   is now ended.
//! - Upsert again with `last_ended_ms = None` and assert the row
//!   remains ended (the `COALESCE` in
//!   `storage::rooms::upsert_recent_room` must keep the previous
//!   non-null end timestamp).

#![allow(clippy::needless_return)]

use locast_client_lib::storage::rooms::{
    list_recent_rooms, upsert_recent_room, RecentRoomEntry, RecentRoomRole,
};
use locast_client_lib::storage::Storage;

async fn open_storage() -> (tempfile::TempDir, Storage) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("index.sqlite");
    let storage = Storage::open(&path).await.expect("open storage");
    (dir, storage)
}

fn entry(room_id: &str, last_seen_ms: i64, last_ended_ms: Option<i64>) -> RecentRoomEntry {
    RecentRoomEntry {
        room_id: room_id.to_string(),
        code: "ABC123".to_string(),
        title: "Test Room".to_string(),
        host_user_id: "host-user".to_string(),
        host_display_name: "Host Person".to_string(),
        role: RecentRoomRole::Host,
        last_seen_ms,
        last_ended_ms,
        created_ms: 1_700_000_000_000,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_then_list_returns_the_row() {
    let (_dir, storage) = open_storage().await;

    upsert_recent_room(&storage, &entry("r1", 1_000, None))
        .await
        .expect("insert");

    let rows = list_recent_rooms(&storage, 100).await.expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].room_id, "r1");
    assert_eq!(rows[0].code, "ABC123");
    assert_eq!(rows[0].title, "Test Room");
    assert_eq!(rows[0].host_user_id, "host-user");
    assert_eq!(rows[0].host_display_name, "Host Person");
    assert_eq!(rows[0].role, RecentRoomRole::Host);
    assert_eq!(rows[0].last_seen_ms, 1_000);
    assert_eq!(rows[0].last_ended_ms, None);
    assert_eq!(rows[0].created_ms, 1_700_000_000_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_existing_room_id_updates_in_place() {
    let (_dir, storage) = open_storage().await;

    upsert_recent_room(&storage, &entry("r2", 1_000, None))
        .await
        .expect("first insert");

    let mut updated = entry("r2", 2_000, None);
    updated.host_display_name = "New Host".to_string();
    upsert_recent_room(&storage, &updated)
        .await
        .expect("update");

    let rows = list_recent_rooms(&storage, 100).await.expect("list");
    assert_eq!(rows.len(), 1, "upsert must not duplicate the row");
    assert_eq!(rows[0].host_display_name, "New Host");
    assert_eq!(rows[0].last_seen_ms, 2_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_orders_by_last_seen_desc() {
    let (_dir, storage) = open_storage().await;

    upsert_recent_room(&storage, &entry("a", 100, None))
        .await
        .expect("a");
    upsert_recent_room(&storage, &entry("b", 300, None))
        .await
        .expect("b");
    upsert_recent_room(&storage, &entry("c", 200, None))
        .await
        .expect("c");

    let rows = list_recent_rooms(&storage, 100).await.expect("list");
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].room_id, "b");
    assert_eq!(rows[1].room_id, "c");
    assert_eq!(rows[2].room_id, "a");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_with_ended_then_stale_none_keeps_end_timestamp() {
    let (_dir, storage) = open_storage().await;

    upsert_recent_room(&storage, &entry("r3", 1_000, None))
        .await
        .expect("insert");

    // Room ends.
    let ended = entry("r3", 2_000, Some(2_000));
    upsert_recent_room(&storage, &ended).await.expect("end");

    let rows = list_recent_rooms(&storage, 100).await.expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].last_ended_ms, Some(2_000));

    // Stale `room://state` arrives after end with last_ended_ms = None.
    let stale = entry("r3", 3_000, None);
    upsert_recent_room(&storage, &stale).await.expect("stale");

    let rows = list_recent_rooms(&storage, 100).await.expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].last_ended_ms,
        Some(2_000),
        "stale upsert must not reset last_ended_ms to NULL"
    );
    // last_seen_ms is still updated so the row floats to the top of
    // the list.
    assert_eq!(rows[0].last_seen_ms, 3_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limit_caps_returned_rows() {
    let (_dir, storage) = open_storage().await;

    for i in 0..5 {
        upsert_recent_room(&storage, &entry(&format!("r{i}"), i as i64 + 1, None))
            .await
            .expect("insert");
    }

    let rows = list_recent_rooms(&storage, 3).await.expect("list");
    assert_eq!(rows.len(), 3);
    // newest first
    assert_eq!(rows[0].room_id, "r4");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn role_survives_round_trip() {
    let (_dir, storage) = open_storage().await;

    let mut e = entry("r4", 1_000, None);
    e.role = RecentRoomRole::Guest;
    upsert_recent_room(&storage, &e).await.expect("insert");

    let rows = list_recent_rooms(&storage, 100).await.expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].role, RecentRoomRole::Guest);

    // Re-upsert with the other role; expect the new role.
    let mut e2 = entry("r4", 2_000, None);
    e2.role = RecentRoomRole::Host;
    upsert_recent_room(&storage, &e2).await.expect("update");

    let rows = list_recent_rooms(&storage, 100).await.expect("list");
    assert_eq!(rows[0].role, RecentRoomRole::Host);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn row_survives_storage_restart() {
    // P2-T08 acceptance: "a client that creates a room, restarts,
    // and visits `/rooms` still sees the room." At the SQL layer
    // this is equivalent to: upsert a row, drop the Storage,
    // reopen Storage against the same path (which re-runs the
    // migrator and exercises `0002_recent_rooms.sql`), and assert
    // the row is still there.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("index.sqlite");
    {
        let storage = Storage::open(&path).await.expect("first open");
        upsert_recent_room(
            &storage,
            &entry(
                "survives-restart",
                1_700_000_000_000,
                Some(1_700_000_500_000),
            ),
        )
        .await
        .expect("insert");
    }
    {
        let storage = Storage::open(&path).await.expect("second open");
        let rows = list_recent_rooms(&storage, 100).await.expect("list");
        assert_eq!(rows.len(), 1, "row must survive storage restart");
        assert_eq!(rows[0].room_id, "survives-restart");
        assert_eq!(rows[0].host_display_name, "Host Person");
        assert_eq!(rows[0].role, RecentRoomRole::Host);
        assert_eq!(rows[0].last_ended_ms, Some(1_700_000_500_000));
    }
}
