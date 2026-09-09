//! Panic boundary helpers for tokio task spawning.
//!
//! Provides utilities to catch panics in async tasks, log them,
//! and ensure cleanup (state transition, registry unregister,
//! token cancellation, event emission).

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use std::future::Future;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use tracing::{error, warn};

use crate::storage::Storage;
use crate::transfer::events::DownloadStateEvent;
use crate::transfer::registry::TransferRegistry;
use crate::transfer::state::DownloadStore;

/// Sanitize a panic payload into a string suitable for `last_error`.
fn sanitize_panic(payload: &Box<dyn std::any::Any + Send + 'static>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else if let Some(s) = payload.downcast_ref::<&String>() {
        s.as_str().to_string()
    } else {
        "internal: panic".to_string()
    }
}

/// Wrap a future with a panic boundary.
///
/// On panic:
/// 1. Logs the panic with context
/// 2. Sets download state to Failed with sanitized error message
/// 3. Emits Failed state event via global emitter
/// 4. Cancels the provided token
/// 4. Unregisters from registry if provided
/// 5. Runs optional cleanup closure
///
/// The future must be `Send + 'static` to be used with `tokio::spawn`.
///
/// # Arguments
/// * `download_id` - ID of the download for error tracking
/// * `media_id` - Media ID for event emission
/// * `storage` - Storage for DownloadStore access
/// * `registry` - Optional TransferRegistry for unregister
/// * `cancel` - Optional CancellationToken to cancel on panic
/// * `cleanup` - Optional async closure for additional cleanup
/// * `fut` - The future to run with panic protection
///
/// # Returns
/// A future that resolves to `Result<(), PanicError>` where `PanicError`
/// contains the sanitized panic message if a panic occurred.
#[derive(Debug, thiserror::Error)]
#[error("task panicked: {0}")]
pub struct PanicError(pub String);

pub async fn spawn_panic_safe<F, Fut>(
    download_id: String,
    media_id: String,
    storage: Arc<Storage>,
    registry: Option<Arc<TransferRegistry>>,
    cancel: Option<CancellationToken>,
    cleanup: Option<Box<dyn FnOnce() -> Fut + Send + 'static>>,
    fut: F,
) -> Result<(), PanicError>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let store = DownloadStore::new(storage.pool().clone());
    // catch_unwind doesn't return a future, we need to handle the closure properly
    let result = panic::catch_unwind(AssertUnwindSafe(fut));

    match result {
        Ok(inner_fut) => {
            // No panic - await the inner future
            inner_fut.await;
            Ok(())
        }
        Err(payload) => {
            let panic_msg = sanitize_panic(&payload);
            let sanitized = format!("internal: panic: {}", panic_msg);
            error!(%download_id, %media_id, %panic_msg, "transfer task panicked");

            // 1. Mark download as failed with sanitized error
            if let Err(e) = store.mark_failed(&download_id, &sanitized).await {
                error!(%download_id, %e, "failed to mark download as failed after panic");
            }

            // 2. Emit Failed state event via global emitter
            let emitter = crate::get_download_event_emitter();
            let event = DownloadStateEvent {
                v: 1,
                id: download_id.clone(),
                media_id: media_id.clone(),
                state: "failed".to_string(),
                error_message: Some(sanitized.clone()),
            };
            emitter.record_state(event);

            // 3. Cancel token
            if let Some(c) = cancel {
                c.cancel();
            }

            // 4. Unregister from registry
            if let Some(r) = registry {
                r.unregister(&download_id).await;
            }

            // 5. Run cleanup
            if let Some(cleanup_fn) = cleanup {
                cleanup_fn().await;
            }

            Err(PanicError(sanitized))
        }
    }
}

/// Convenience macro for spawning a panic-safe task.
///
/// Usage:
/// ```ignore
/// tokio::spawn(spawn_panic_safe!(
///     download_id: id.clone(),
///     media_id: media_id.clone(),
///     storage: storage.clone(),
///     registry: Some(registry.clone()),
///     cancel: Some(cancel.clone()),
///     cleanup: || async { /* cleanup */ },
///     async move {
///         // task body
///     }
/// ));
/// ```
#[macro_export]
macro_rules! spawn_panic_safe {
    (
        download_id: $download_id:expr,
        media_id: $media_id:expr,
        storage: $storage:expr,
        registry: $registry:expr,
        cancel: $cancel:expr,
        cleanup: $cleanup:expr,
        $fut:expr
    ) => {
        tokio::spawn($crate::transfer::panic_boundary::spawn_panic_safe(
            $download_id,
            $media_id,
            $storage,
            $registry,
            $cancel,
            $cleanup,
            $fut,
        ))
    };
}
