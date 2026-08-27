//! `identity` - the local Ed25519 keypair, OS-keyring storage, and
//! first-launch keypair generation.
//!
//! P2-T01 is the only consumer at this phase. The module exposes:
//!
//! - [`Identity`] - the loaded identity (public key + display name).
//! - [`IdentityKeyring`] - a trait that abstracts keyring access so
//!   tests can substitute an in-memory store and never touch the
//!   developer's real keychain.
//! - [`OsKeyring`] - the production implementation backed by the
//!   `keyring` crate.
//! - [`generate`] - first-launch Ed25519 keypair generation using
//!   `rand::rngs::OsRng`.
//! - [`derive_user_id`] - `sha256(public_key)` hex, the stable
//!   `user_identities.id` value.
//!
//! The public key is the only thing that leaves the device. The
//! private key is read from / written to the OS keyring; it never
//! crosses the Tauri IPC boundary, never appears in a log line, and
//! never touches SQLite. The Tauri command surface (in
//! `crate::commands::identity`) is the only place private key
//! material is touched, and even there the private key is used to
//! sign and immediately dropped; only the public key and the
//! signature ever leave the function.

#![deny(unsafe_code)]
#![warn(rust_2018_idioms)]

pub mod keystore;
pub mod types;

pub use keystore::{IdentityKeyring, MockKeyring, OsKeyring};
pub use types::{
    derive_user_id, generate, validate_display_name, DisplayNameError, Identity, Keypair,
};
