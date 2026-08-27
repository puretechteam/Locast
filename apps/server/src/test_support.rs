//! Shared test helpers. Visible only to `#[cfg(test)]` modules and
//! integration tests via `pub` re-exports.

#![allow(dead_code)]

/// An `off` tracing filter that satisfies `Config::log_filter`.
pub fn null_log_filter() -> String {
    "off".to_string()
}
