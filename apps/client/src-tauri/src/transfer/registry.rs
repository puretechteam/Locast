//! P3-T13: TransferRegistry -- owns the CancellationTokens for
//! in-flight transfers, keyed by download_id. The download_open
//! command registers each spawned transfer; room_leave / app
//! shutdown can cancel_all to ensure no leaked tasks.
//!
//! P3-T13 (review fix A#4/D#19): the registry no longer takes
//! ownership of the orchestrator's `JoinHandle`. The caller keeps
//! the handle locally, awaits it inside the spawned task, and
//! calls `unregister(id)` after the orchestrator returns. This
//! removes the leak the prior `register(JoinHandle<()>, ...)`
//! shape caused when an orchestrator completed normally: the
//! join handle stayed parked in the registry until the next
//! `register`/`cancel`/`cancel_all` on the same id.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct RegistryInner {
    tokens: HashMap<String, CancellationToken>,
}

#[derive(Clone, Default)]
pub struct TransferRegistry(Arc<Mutex<RegistryInner>>);

impl TransferRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the cancellation token for a fresh transfer. If
    /// an entry already exists for `download_id`, the prior
    /// token is cancelled (the previous transfer is told to
    /// abort) and replaced.
    pub async fn register(&self, download_id: String, cancel: CancellationToken) {
        let mut g = self.0.lock().await;
        if let Some(old_t) = g.tokens.remove(&download_id) {
            old_t.cancel();
        }
        g.tokens.insert(download_id, cancel);
    }

    /// Remove the entry for `download_id`. Idempotent.
    pub async fn unregister(&self, download_id: &str) {
        let mut g = self.0.lock().await;
        g.tokens.remove(download_id);
    }

    /// Cancel the transfer (if any) for this download_id and
    /// return `true` if a transfer was cancelled.
    pub async fn cancel(&self, download_id: &str) -> bool {
        let mut g = self.0.lock().await;
        if let Some(t) = g.tokens.remove(download_id) {
            t.cancel();
            true
        } else {
            false
        }
    }

    /// Cancel every registered transfer. Used on room_leave /
    /// shutdown.
    pub async fn cancel_all(&self) {
        let mut g = self.0.lock().await;
        for (_, t) in g.tokens.drain() {
            t.cancel();
        }
    }
}
