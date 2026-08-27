//! `library` - filesystem-facing library operations.
//!
//! `core` is pure (no I/O, no env, no clock); `library` is where the
//! library-root layout meets the operating system. P1-T02 adds the
//! atomic completion routine (`library::fs::complete_download`); P1-T07
//! adds the on-disk scanner (`library::scan::scan`) that reconciles
//! the content-addressed tree against the `media_items` table.

pub mod fs;
pub mod scan;

pub use scan::{scan as scan_library, ScanError, ScanResult};
