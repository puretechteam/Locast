//! `commands::settings` - the user-facing server address.
//!
//! A packaged build ships without a production endpoint (the client never
//! bakes one in), and the only way to point it at a server used to be the
//! `LOCAST_SIGNALING_URL` environment variable. These two commands let the
//! Settings page store the address instead:
//!
//! - `settings_get_server` returns the saved address, the address the running
//!   client is actually using, and whether the env var overrides both.
//! - `settings_set_server_url` validates and saves a new address, or clears it.
//!
//! The signaling client reads its address once at startup (see
//! `net::config`), so a change applies on the next launch. The page shows
//! that whenever `configured_url` differs from `active_url`.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::State as TauriState;

use crate::commands::error::AppError;
use crate::net::config::{validate_signaling_url, SignalingConfig, ENV_URL, SETTING_URL_KEY};
use crate::net::signaling::SignalingClient;
use crate::storage::{settings, Storage};

/// The server address as the Settings page needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ServerSettingsIpc {
    /// The address saved in Settings, if any.
    pub configured_url: Option<String>,
    /// The address the running client connects to.
    pub active_url: String,
    /// The address the next launch will connect to: the env var if set,
    /// else the saved address if it is valid, else the default. When it
    /// differs from `active_url` the app needs a restart.
    pub next_url: String,
    /// `true` when `LOCAST_SIGNALING_URL` is set: it wins over the saved
    /// address, so editing the saved one has no effect until it is unset.
    pub env_override: bool,
}

fn env_override_active() -> bool {
    std::env::var(ENV_URL).is_ok_and(|s| !s.trim().is_empty())
}

fn storage_err(e: settings::SettingsError) -> AppError {
    AppError::other(format!("settings: {e}"))
}

/// Read the saved address and pair it with the one in use.
pub async fn read_server_settings(
    storage: &Storage,
    active_url: &str,
) -> Result<ServerSettingsIpc, AppError> {
    // A value that is not a JSON string (a damaged row) counts as "not
    // configured", so the page still loads and saving overwrites it.
    let configured_url = settings::get_raw(storage, SETTING_URL_KEY)
        .await
        .map_err(storage_err)?
        .and_then(|raw| serde_json::from_str::<String>(&raw).ok());
    let next_url = SignalingConfig::from_env_with_stored(configured_url.as_deref()).url;
    Ok(ServerSettingsIpc {
        configured_url,
        active_url: active_url.to_string(),
        next_url,
        env_override: env_override_active(),
    })
}

/// Validate and save `url`, or clear the saved address when it is `None`
/// or blank. A rejected address is reported to the caller and nothing is
/// written.
pub async fn write_server_url(
    storage: &Storage,
    active_url: &str,
    url: Option<String>,
) -> Result<ServerSettingsIpc, AppError> {
    match url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        None => {
            settings::delete(storage, SETTING_URL_KEY)
                .await
                .map_err(storage_err)?;
        }
        Some(raw) => {
            let normalized =
                validate_signaling_url(raw).map_err(|e| AppError::other(e.to_string()))?;
            settings::set_json(storage, SETTING_URL_KEY, &normalized)
                .await
                .map_err(storage_err)?;
        }
    }
    read_server_settings(storage, active_url).await
}

/// Tauri command: the saved and active server address.
#[tauri::command]
#[specta::specta]
pub async fn settings_get_server(
    storage: TauriState<'_, Storage>,
    signaling: TauriState<'_, std::sync::Arc<SignalingClient>>,
) -> Result<ServerSettingsIpc, AppError> {
    let active = signaling.snapshot().await.server_url;
    read_server_settings(storage.inner(), &active).await
}

/// Tauri command: save (or, with `None`, clear) the server address. It
/// takes effect the next time the app starts.
#[tauri::command]
#[specta::specta]
pub async fn settings_set_server_url(
    storage: TauriState<'_, Storage>,
    signaling: TauriState<'_, std::sync::Arc<SignalingClient>>,
    url: Option<String>,
) -> Result<ServerSettingsIpc, AppError> {
    let active = signaling.snapshot().await.server_url;
    write_server_url(storage.inner(), &active, url).await
}
