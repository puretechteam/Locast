//! `commands::identity` - the Tauri command surface for the local
//! Ed25519 keypair.
//!
//! P2-T01 ships three commands:
//!
//! - `identity_get()` - get the local identity, creating it on
//!   first launch.
//! - `identity_rotate()` - generate a new keypair, overwrite the
//!   keyring entry, update the `user_identities` row.
//! - `identity_set_display_name(new_name)` - update the display
//!   name on the current `user_identities` row without rotating
//!   the keypair.
//!
//! None of these commands ever expose the private key to the
//! webview. The Tauri runtime hands the React side the
//! [`Identity`] struct (public key + display name + user_id); the
//! private key stays in the keyring.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use tauri::State as TauriState;

use crate::commands::error::AppError;
use crate::identity::keystore::IdentityService;

/// Tauri command: return the local identity. On first launch a
/// fresh Ed25519 keypair is generated, persisted to the OS
/// keyring, and mirrored into the `user_identities` table.
///
/// The `display_name` argument is required so the very first call
/// can seed the display name atomically with the keypair
/// generation; subsequent calls update the existing row's
/// `display_name` to the new value (useful for the Settings UI).
#[tauri::command]
#[specta::specta]
pub async fn identity_get(
    service: TauriState<'_, IdentityService>,
    display_name: String,
) -> Result<crate::identity::Identity, AppError> {
    service
        .get_or_create(&display_name)
        .await
        .map_err(AppError::from)
}

/// Tauri command: rotate the local identity. A fresh keypair is
/// generated, the keyring entry is overwritten, and a new
/// `user_identities` row is inserted for the new public key.
/// The old row is left in place (the architecture retains every
/// Locast user the local client has ever seen).
#[tauri::command]
#[specta::specta]
pub async fn identity_rotate(
    service: TauriState<'_, IdentityService>,
    display_name: String,
) -> Result<crate::identity::Identity, AppError> {
    service.rotate(&display_name).await.map_err(AppError::from)
}

/// Tauri command: update the local display name without rotating
/// the keypair. Validates the new name and updates the
/// `user_identities.display_name` column for the current user.
/// The webview never sees the private key.
#[tauri::command]
#[specta::specta]
pub async fn identity_set_display_name(
    service: TauriState<'_, IdentityService>,
    display_name: String,
) -> Result<crate::identity::Identity, AppError> {
    service.get(&display_name).await.map_err(AppError::from)
}
