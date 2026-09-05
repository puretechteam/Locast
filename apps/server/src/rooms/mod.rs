//! Room-lifecycle module. Houses the [`registry`], the
//! per-message [`dispatch`], the [`codes`] generator, the
//! [`error`] enum, and the [`state`] types.
//!
//! P2-T04: server-side room registry, create/join/leave
//! envelopes, optional host migration.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod caps;
pub mod codes;
pub mod dispatch;
pub mod error;
pub mod manifest;
pub mod playback;
pub mod presence;
pub mod registry;
pub mod signal;
pub mod state;
pub mod store;
pub mod validation;

pub use codes::{generate_code, is_valid_code, normalize, ALPHABET, CODE_LEN};
pub use dispatch::{dispatch_room_message, DispatchContext, RoomDispatchOutcome};
pub use error::RoomError;
pub use manifest::{handle_manifest_publish, manifest_published_envelope};
pub use playback::{handle_playback_cmd, PlaybackError};
pub use presence::{handle_position_report, PresenceError};
pub use registry::{
    BroadcastItem, CachedManifest, RoomEvent, RoomHandle, RoomRegistry, RoomRegistryConfig,
};
pub use signal::{
    handle_signal, SendError, SignalError, SignalOutcome, SignalRelay, SIGNAL_MAX_BYTES,
};
pub use state::{ParticipantRecord, PlaybackBookkeeping, RoomLifecycle, RoomState};
use std::sync::Arc;
use std::time::Duration;
pub use store::{DbRoomStore, NoopRoomStore, RoomStore};
use tracing::warn;
pub use validation::validate_display_name;

use crate::time::{Clock, MockClock};

/// Spawn the background task that drives the host-disconnect
/// grace and the stale-participant cleanup. The task runs
/// for the lifetime of the server; the returned
/// `JoinHandle` is intentionally not held (the task is
/// expected to run forever).
///
/// `interval` is the wall-clock gap between ticks. A small
/// interval (e.g. 200-500ms) keeps the grace deadline
/// accurate; a larger one saves CPU at the cost of latency
/// on the migration announcement.
pub fn spawn_room_ticker(
    rooms: Arc<RoomRegistry>,
    store: Arc<dyn store::RoomStore>,
    clock: Arc<dyn Clock>,
    interval: Duration,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let now = clock.now_ms();
            if let Err(e) = run_tick(&rooms, store.as_ref(), now).await {
                warn!(error = %e, "locast-server room ticker iteration failed");
            }
        }
    });
}

async fn run_tick(
    rooms: &RoomRegistry,
    store: &dyn store::RoomStore,
    now: i64,
) -> Result<(), String> {
    // Grace timer: any room whose `host_disconnect_deadline_ms`
    // has elapsed triggers a host election.
    let migrations = rooms.tick_grace(store, now).await;
    if !migrations.is_empty() {
        tracing::debug!(count = migrations.len(), "room grace tick fired");
    }
    // P4-T08: presence-driven DISCONNECTED transition.
    // Runs BEFORE the5-minute stale-cleanup sweep so a
    // participant who times out is broadcast as
    // `Disconnected` immediately and is then removed
    // from in-memory state only after the longer stale
    // window elapses. The two timers share the same tick
    // cadence (500 ms in production) so there is exactly
    // one background task, not two.
    let disconnects = rooms.tick_presence_timeout(now).await;
    if !disconnects.is_empty() {
        tracing::debug!(
            count = disconnects.len(),
            "presence timeout -> DISCONNECTED + broadcast"
        );
    }
    // Stale-participant cleanup.
    let stale = rooms.tick_stale_participants(store, now).await;
    if !stale.is_empty() {
        tracing::debug!(count = stale.len(), "stale participants removed");
    }
    Ok(())
}

/// Test-only variant of [`spawn_room_ticker`] that uses an
/// injected [`Clock`] (production code uses
/// [`SystemClock`]). On every tick, this variant also
/// synchronizes the clock to wall-clock time so the
/// 200ms-grace tests don't have to manually advance the
/// clock. Returns a future that runs the ticker
/// indefinitely; tests spawn it on the test runtime and let
/// the runtime drop it when the test ends.
pub async fn spawn_room_ticker_for_test(
    rooms: Arc<RoomRegistry>,
    store: Arc<dyn store::RoomStore>,
    clock: Arc<MockClock>,
    interval: Duration,
) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        // Sync the mock clock to wall time so the grace
        // and stale-cleanup paths advance in real-time
        // tests.
        let now_wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        clock.set(now_wall);
        let now = clock.now_ms();
        if let Err(e) = run_tick(&rooms, store.as_ref(), now).await {
            warn!(error = %e, "locast-server test room ticker iteration failed");
        }
    }
}
