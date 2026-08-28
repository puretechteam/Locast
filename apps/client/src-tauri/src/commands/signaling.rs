//! `commands::signaling` - the Tauri command surface for the
//! native WebSocket client.
//!
//! P2-T03 ships three commands:
//!
//! - `signaling_get_state` - return the current `ConnectionState`
//!   (no bearer, no signature, no nonce).
//! - `signaling_connect` - start the connection loop. Idempotent.
//! - `signaling_disconnect` - cancel the connection loop and
//!   await its exit.
//!
//! The frontend listens for state changes through the
//! `signaling://state` event; the actual `emit` happens from
//! inside the `SignalingClient` when the state changes.
//! Subscribing to events is a separate concern (a later task
//! will add a generic `signaling_subscribe` command); the
//! current commands just expose read and control.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use tauri::State as TauriState;

use crate::commands::error::AppError;
use crate::net::signaling::SignalingClient;
use crate::net::state::ConnectionState;

/// Tauri command: return a redacted snapshot of the current
/// connection state. The bearer, the AUTH signature, the
/// challenge nonce, and the private key are NEVER returned.
#[tauri::command]
#[specta::specta]
pub async fn signaling_get_state(
    client: TauriState<'_, SignalingClient>,
) -> Result<ConnectionState, AppError> {
    Ok(client.snapshot().await)
}

/// Tauri command: start the connection loop. Idempotent: a
/// second call while the loop is alive is a no-op.
#[tauri::command]
#[specta::specta]
pub async fn signaling_connect(client: TauriState<'_, SignalingClient>) -> Result<(), AppError> {
    client
        .start()
        .await
        .map_err(|e| AppError::other(e.to_string()))
}

/// Tauri command: cancel the connection loop and await its
/// exit. Safe to call multiple times.
#[tauri::command]
#[specta::specta]
pub async fn signaling_disconnect(client: TauriState<'_, SignalingClient>) -> Result<(), AppError> {
    client.shutdown().await;
    Ok(())
}
