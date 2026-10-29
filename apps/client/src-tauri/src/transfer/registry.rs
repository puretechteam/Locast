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
//!
//! P7-T05: adds `RegisterGuard` for RAII-based unregister on
//! any exit path including panic.
//!
//! One active transfer per download id: `try_register` checks and
//! inserts under one lock, so two overlapping `download_open` calls
//! cannot both start an orchestrator (the second one used to cancel
//! the first, which then marked the shared row Cancelled while the
//! survivor kept transferring). Each entry carries a generation and a
//! guard only removes the entry it created, so an old guard can never
//! erase a newer transfer's entry. The lock is a std Mutex (never held
//! across an await) so `Drop` releases the entry synchronously.
//!
//! `cancel` / `cancel_all` only cancel; the entry stays until the
//! cancelled task's guard drops, so a reopen (leave and rejoin) cannot
//! start a second transfer on the same row while the first one is
//! still winding down and touching that row and its scratch files.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio_util::sync::CancellationToken;

struct Entry {
    token: CancellationToken,
    generation: u64,
    /// False while the claim holds `try_register`'s placeholder token
    /// (the orchestrator is still being set up and may never start);
    /// true once `adopt_token` handed over the real one.
    adopted: bool,
}

/// Outcome of [`TransferRegistry::try_register`].
pub enum Claim {
    /// The id was free; the caller owns it until the guard drops.
    Claimed(RegisterGuard),
    /// A transfer is running for this id.
    Running,
    /// Another caller is still setting up a transfer for this id, or
    /// a cancelled one is still winding down. It may or may not end
    /// up running, so the caller should ask again later.
    Busy,
}

#[derive(Default)]
struct RegistryInner {
    tokens: HashMap<String, Entry>,
    next_generation: u64,
}

#[derive(Clone, Default)]
pub struct TransferRegistry(Arc<Mutex<RegistryInner>>);

/// RAII guard that unregisters a download from the registry on drop.
/// Guarantees cleanup even if the registering task panics. It only
/// removes the entry it created (matched by generation).
pub struct RegisterGuard {
    registry: TransferRegistry,
    download_id: String,
    generation: u64,
}

impl RegisterGuard {
    /// Swap the cancellation token of the entry this guard owns.
    /// `download_open` claims the id before the orchestrator (and its
    /// token) exists, then hands the real token over here. Returns
    /// `false` when the entry is gone or no longer this guard's (a
    /// `cancel` / `cancel_all` ran in between); `token` is cancelled
    /// in that case so the caller does not start the transfer.
    pub fn adopt_token(&self, token: CancellationToken) -> bool {
        let mut g = self.registry.lock();
        match g.tokens.get_mut(&self.download_id) {
            Some(e) if e.generation == self.generation && !e.token.is_cancelled() => {
                e.token = token;
                e.adopted = true;
                true
            }
            _ => {
                token.cancel();
                false
            }
        }
    }
}

impl Drop for RegisterGuard {
    fn drop(&mut self) {
        // Unregister on any exit path (normal return, panic, early
        // return), but only if the entry is still ours.
        let mut g = self.registry.lock();
        if g.tokens
            .get(&self.download_id)
            .is_some_and(|e| e.generation == self.generation)
        {
            g.tokens.remove(&self.download_id);
        }
    }
}

impl TransferRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, RegistryInner> {
        // A panic while holding this lock cannot leave the map
        // half-updated (every critical section is a single map
        // operation), so a poisoned lock is still safe to use.
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn insert(
        g: &mut RegistryInner,
        download_id: &str,
        cancel: CancellationToken,
        adopted: bool,
    ) -> u64 {
        g.next_generation += 1;
        let generation = g.next_generation;
        g.tokens.insert(
            download_id.to_string(),
            Entry {
                token: cancel,
                generation,
                adopted,
            },
        );
        generation
    }

    /// Claim `download_id` for a fresh transfer. When an entry
    /// already exists it is left untouched and the caller learns
    /// whether that transfer is running or not (yet / any more).
    /// The check and the insert happen under one lock. `cancel` is a
    /// placeholder until [`RegisterGuard::adopt_token`].
    pub async fn try_register(&self, download_id: String, cancel: CancellationToken) -> Claim {
        let mut g = self.lock();
        if let Some(e) = g.tokens.get(&download_id) {
            return if e.adopted && !e.token.is_cancelled() {
                Claim::Running
            } else {
                Claim::Busy
            };
        }
        let generation = Self::insert(&mut g, &download_id, cancel, false);
        Claim::Claimed(RegisterGuard {
            registry: self.clone(),
            download_id,
            generation,
        })
    }

    /// Register the cancellation token for a fresh transfer. If
    /// an entry already exists for `download_id`, the prior
    /// token is cancelled (the previous transfer is told to
    /// abort) and replaced. Returns a `RegisterGuard` that
    /// unregisters on drop (RAII pattern for panic safety).
    /// `download_open` uses `try_register`; this replacing form is
    /// kept for callers that mean to supersede a transfer.
    pub async fn register(&self, download_id: String, cancel: CancellationToken) -> RegisterGuard {
        let mut g = self.lock();
        if let Some(old) = g.tokens.remove(&download_id) {
            old.token.cancel();
        }
        let generation = Self::insert(&mut g, &download_id, cancel, true);
        RegisterGuard {
            registry: self.clone(),
            download_id,
            generation,
        }
    }

    /// True when a transfer is registered (running) for
    /// `download_id`.
    pub async fn is_active(&self, download_id: &str) -> bool {
        self.lock().tokens.contains_key(download_id)
    }

    /// Remove the entry for `download_id`. Idempotent.
    pub async fn unregister(&self, download_id: &str) {
        self.lock().tokens.remove(download_id);
    }

    /// Cancel the transfer (if any) for this download_id and
    /// return `true` if a transfer was cancelled. The entry stays
    /// until that transfer's guard drops.
    pub async fn cancel(&self, download_id: &str) -> bool {
        let token = self.lock().tokens.get(download_id).map(|e| e.token.clone());
        match token {
            Some(t) => {
                t.cancel();
                true
            }
            None => false,
        }
    }

    /// Cancel every registered transfer. Used on room_leave /
    /// shutdown. Entries stay until each transfer's guard drops.
    pub async fn cancel_all(&self) {
        let tokens: Vec<CancellationToken> = self
            .lock()
            .tokens
            .values()
            .map(|e| e.token.clone())
            .collect();
        for t in tokens {
            t.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn claim(r: &TransferRegistry, id: &str) -> RegisterGuard {
        match r.try_register(id.into(), CancellationToken::new()).await {
            Claim::Claimed(g) => g,
            _ => panic!("expected the id {id} to be free"),
        }
    }

    #[tokio::test]
    async fn second_try_register_is_refused_and_leaves_the_first_running() {
        let r = TransferRegistry::new();
        let g = claim(&r, "d").await;
        // Still in setup: a second caller must retry, not assume a
        // transfer is running.
        assert!(matches!(
            r.try_register("d".into(), CancellationToken::new()).await,
            Claim::Busy
        ));
        let real = CancellationToken::new();
        assert!(g.adopt_token(real.clone()));
        assert!(matches!(
            r.try_register("d".into(), CancellationToken::new()).await,
            Claim::Running
        ));
        assert!(!real.is_cancelled());
        assert!(r.is_active("d").await);
    }

    #[tokio::test]
    async fn guard_drop_releases_its_entry_at_once() {
        let r = TransferRegistry::new();
        drop(claim(&r, "d").await);
        assert!(!r.is_active("d").await);
        let _again = claim(&r, "d").await;
    }

    #[tokio::test]
    async fn a_cancelled_transfer_keeps_its_entry_until_its_guard_drops() {
        let r = TransferRegistry::new();
        let g = claim(&r, "d").await;
        let real = CancellationToken::new();
        assert!(g.adopt_token(real.clone()));
        r.cancel_all().await;
        assert!(real.is_cancelled());
        // Winding down: a reopen must wait instead of starting a
        // second transfer on the same row.
        assert!(matches!(
            r.try_register("d".into(), CancellationToken::new()).await,
            Claim::Busy
        ));
        drop(g);
        let _fresh = claim(&r, "d").await;
    }

    #[tokio::test]
    async fn an_old_guard_does_not_remove_a_newer_entry() {
        let r = TransferRegistry::new();
        let old = r.register("d".into(), CancellationToken::new()).await;
        let newer = CancellationToken::new();
        let _new = r.register("d".into(), newer.clone()).await;
        drop(old);
        assert!(r.is_active("d").await);
        assert!(!newer.is_cancelled());
    }

    #[tokio::test]
    async fn adopt_token_fails_after_a_cancel_and_cancels_the_new_token() {
        let r = TransferRegistry::new();
        let g = claim(&r, "d").await;
        assert!(r.cancel("d").await);
        let real = CancellationToken::new();
        assert!(!g.adopt_token(real.clone()));
        assert!(real.is_cancelled());
    }

    #[tokio::test]
    async fn adopted_token_is_what_cancel_all_cancels() {
        let r = TransferRegistry::new();
        let g = claim(&r, "d").await;
        let real = CancellationToken::new();
        assert!(g.adopt_token(real.clone()));
        r.cancel_all().await;
        assert!(real.is_cancelled());
    }
}
