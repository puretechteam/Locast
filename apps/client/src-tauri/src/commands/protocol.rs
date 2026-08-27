//! `commands::protocol` - the Tauri command surface for the
//! `locast://` URL resolver.
//!
//! P1-T08 ships one command:
//!
//! - `media_resolve_url(media_id)` - return the
//!   `locast://media/<sha-prefix>/<filename>` URL for a given
//!   `media_id`. The React side passes this URL to `<video src>`
//!   or `<track src>`. The Tauri URI scheme handler (registered
//!   in `lib.rs`) then serves the file out of the library root.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use tauri::State as TauriState;

use crate::commands::error::AppError;
use crate::library::protocol::resolve_media_url;
use crate::storage::Storage;

/// Tauri command: resolve a `media_id` to a `locast://media/...`
/// URL. The returned URL is opaque to the webview; it must not
/// be parsed.
#[tauri::command]
#[specta::specta]
pub async fn media_resolve_url(
    storage: TauriState<'_, Storage>,
    media_id: String,
) -> Result<String, AppError> {
    resolve_media_url(storage.inner(), &media_id)
        .await
        .map_err(AppError::from)
}
