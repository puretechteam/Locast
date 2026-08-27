//! Typed settings repository for the `settings` table.
//!
//! P1-T05 introduces the per-user preferences (starting with the disk
//! quota). The `settings` table is `CREATE TABLE settings (key TEXT
//! PRIMARY KEY, value TEXT)` (see `migrations/0001_init.sql`); every
//! value is a JSON string. This module is a thin repository over the
//! table: typed getters and setters that serialize / deserialize JSON,
//! plus a raw-string variant for callers that already have a JSON
//! string in hand.
//!
//! # Locked surface
//!
//! The `Storage` type is locked by P0-T05; this module only consumes
//! its `pool()` accessor. No new methods are added to `Storage`.
//!
//! # Error mapping
//!
//! `SettingsError` is the only error type in this module. Callers that
//! need a Tauri-layer error convert via `From<SettingsError> for
//! AppError` in the `commands` tree.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

use thiserror::Error;

use crate::storage::{Storage, StorageError};

/// Errors raised by the settings repository.
///
/// `Storage` is for `Storage::open` failures (the pool handle is
/// unusable; the caller will need to rebuild the storage). `Sqlx` is
/// for runtime SQL errors. `Json` is for deserialization failures on
/// `get_json` and for serialization failures on `set_json`.
#[derive(Debug, Error)]
pub enum SettingsError {
    /// The storage handle itself failed (open / pool error).
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),

    /// A SQLite statement failed at runtime.
    #[error("settings sql error: {0}")]
    Sqlx(#[from] sqlx::Error),

    /// A JSON (de)serialization call failed.
    #[error("settings json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Read a JSON-typed setting.
///
/// Returns `Ok(None)` if the key is absent. Returns
/// `Err(SettingsError::Json(...))` if the value is present but does not
/// deserialize into `T`. The JSON value is the verbatim content of
/// `settings.value`; for primitive caps the caller can pass `T = i64`.
pub async fn get_json<T: serde::de::DeserializeOwned>(
    storage: &Storage,
    key: &str,
) -> Result<Option<T>, SettingsError> {
    let raw = get_raw(storage, key).await?;
    match raw {
        None => Ok(None),
        Some(s) => Ok(Some(serde_json::from_str(&s)?)),
    }
}

/// UPSERT a JSON-typed setting.
///
/// The value is serialized to a JSON string and stored in
/// `settings.value`. The key is treated as the primary key; the row
/// is replaced if it already exists.
pub async fn set_json<T: serde::Serialize>(
    storage: &Storage,
    key: &str,
    value: &T,
) -> Result<(), SettingsError> {
    let s = serde_json::to_string(value)?;
    set_raw(storage, key, &s).await
}

/// Read a setting's raw JSON string.
///
/// Returns `Ok(None)` if the key is absent. Returns
/// `Err(SettingsError::Sqlx(...))` on a runtime SQL failure.
pub async fn get_raw(storage: &Storage, key: &str) -> Result<Option<String>, SettingsError> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?1")
        .bind(key)
        .fetch_optional(&storage.pool())
        .await?;
    Ok(row.map(|(v,)| v))
}

/// UPSERT a raw JSON string setting.
///
/// The caller is responsible for ensuring the value is a valid JSON
/// document. SQLite does not validate; we store the string verbatim.
/// (The settings value is opaque to the rest of the storage layer.)
pub async fn set_raw(storage: &Storage, key: &str, value: &str) -> Result<(), SettingsError> {
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(&storage.pool())
    .await?;
    Ok(())
}

/// Delete a setting.
///
/// Returns `true` if a row was deleted, `false` if the key was
/// absent. The settings layer does not require keys to be present;
/// this is the "unset" verb.
pub async fn delete(storage: &Storage, key: &str) -> Result<bool, SettingsError> {
    let n = sqlx::query("DELETE FROM settings WHERE key = ?1")
        .bind(key)
        .execute(&storage.pool())
        .await?
        .rows_affected();
    Ok(n > 0)
}
