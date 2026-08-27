//! `core::library` - pure-Rust library-domain logic.
//!
//! P1-T01 adds the filename sanitizer used by `media_import` and the
//! `locast://` handler (architecture section 6). Future tasks in this module
//! will add the on-disk layout (`fs`) and the scanner (`scan`); both will
//! depend on this sanitizer.

pub mod sanitize;
