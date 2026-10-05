//! Environment-driven configuration for the Locast signaling server.
//!
//! P0-T03 establishes the loader and the small set of variables the skeleton
//! needs (`LOCAST_BIND_ADDR`, `LOCAST_LOG`). P2-T02 adds the auth /
//! handshake / transport limits the WebSocket layer needs:
//! database URL, bearer TTL, challenge TTL, max frame bytes,
//! and the post-TCP-accept handshake deadline.
//!
//! P8-T05: variables marked `sensitive` in `docs/ARCHITECTURE.md`
//! (`LOCAST_TURN_SHARED_SECRET`, `LOCAST_DB_KEY`) are read once
//! by [`Config::from_env`], removed from the process environment,
//! and held in a [`SecretString`] that never prints its value and
//! zeroizes its buffer on drop.

use std::env;
use std::fmt;
use std::net::SocketAddr;

use zeroize::Zeroize;

/// Default bind address if `LOCAST_BIND_ADDR` is not set.
pub const DEFAULT_BIND_ADDR: &str = "0.0.0.0:8787";

/// Default database URL. `:memory:` is fine for the test harness;
/// production deployments use a file on disk via the Dockerfile
/// and compose file.
pub const DEFAULT_DATABASE_URL: &str = "sqlite::memory:";

/// Default bearer TTL. 15 minutes per `docs/ARCHITECTURE.md`
/// sections 20.4.4 and 21.3.
pub const DEFAULT_BEARER_TTL_SECONDS: i64 = 15 * 60;

/// Default CHALLENGE nonce TTL. 30 seconds per section 18.4.1.
pub const DEFAULT_CHALLENGE_TTL_MS: i64 = 30_000;

/// Default per-frame ceiling at the WS transport layer.
/// 1 MiB per section 18.5 and 20.6.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1_048_576;

/// Default post-TCP-accept deadline for completing the full
/// HELLO -> AUTH_OK sequence. After this elapses the server
/// closes the connection.
pub const DEFAULT_HANDSHAKE_TIMEOUT_MS: i64 = 15_000;

/// Default per-connection msg/s sustained rate (§18.6, §20.6).
pub const DEFAULT_RATE_MSGS_PER_SEC: u32 = 100;
/// Default per-connection msg burst budget (§18.6).
pub const DEFAULT_RATE_MSG_BURST: u32 = 200;
/// Default per-connection bytes/s sustained rate (§18.6, §20.6).
pub const DEFAULT_RATE_BYTES_PER_SEC: u32 = 1_000_000;
/// Default per-connection bytes burst budget.
pub const DEFAULT_RATE_BYTES_BURST: u32 = 2_000_000;

/// Default room-code length. Pinned to 6 by the P2-T04 spec
/// and the architecture.
pub const DEFAULT_ROOM_CODE_LENGTH: usize = 6;

/// Default room-code alphabet. The 32-character unambiguous
/// set from `docs/ARCHITECTURE.md` §21.2.
pub const DEFAULT_ROOM_CODE_ALPHABET: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

/// Default max participants per room. 1 host + 7 viewers
/// per architecture §20.6.
pub const DEFAULT_ROOM_MAX_PARTICIPANTS: u8 = 8;

/// Default host-disconnect grace. The host has 30 seconds to
/// re-auth before the server elects a new host. Tests can
/// override via `LOCAST_HOST_DISCONNECT_GRACE_MS`.
pub const DEFAULT_HOST_DISCONNECT_GRACE_MS: i64 = 30_000;

/// Default max room-code generation collisions before
/// aborting the create call.
pub const DEFAULT_ROOM_CREATE_MAX_COLLISIONS: u8 = 5;

/// Default "stale participant" timeout. A viewer that has
/// not sent a `PRESENCE` in 5 minutes is removed from the
/// room and a `PARTICIPANT_LEFT { reason: "timeout" }` is
/// broadcast.
pub const DEFAULT_PARTICIPANT_STALE_AFTER_MS: i64 = 300_000;

/// P4-T08: presence-driven `DISCONNECTED` transition
/// threshold. After this many ms without an inbound
/// `PRESENCE`, a non-host participant whose status is
/// still `Connected` is flipped to `Disconnected` and a
/// `PARTICIPANT_LEFT { reason: "timeout" }` is broadcast
/// to the room. The participant record remains in
/// in-memory state for `DEFAULT_PARTICIPANT_STALE_AFTER_MS`
/// (5 minutes) so a quick reconnect can revive them
/// without re-creating the participant row. This matches
/// the roadmap's "3 missed = DISCONNECTED; PEER_LEAVE
/// broadcast" intent: 3 * `client PRESENCE_INTERVAL` (5 s)
/// = 15 s. Hosts are exempt; host liveness is owned by
/// the existing `host_disconnect_grace_ms` migration path.
pub const DEFAULT_PARTICIPANT_DISCONNECT_AFTER_MS: i64 = 15_000;

/// Env var holding the coturn `use-auth-secret` shared secret.
/// Marked `sensitive`.
pub const ENV_TURN_SECRET: &str = "LOCAST_TURN_SHARED_SECRET";

/// Env var holding the database encryption key. Marked `sensitive`.
pub const ENV_DB_KEY: &str = "LOCAST_DB_KEY";

/// Every env var marked `sensitive`. These are read once, removed
/// from the environment, and never echoed in errors or logs.
pub const SENSITIVE_ENV_VARS: &[&str] = &[ENV_TURN_SECRET, ENV_DB_KEY];

/// A secret read from a `sensitive` env var.
///
/// Deliberately not `Clone` or `Serialize`, so a copy cannot be made
/// by accident; `Debug` and `Display` print a fixed placeholder.
/// The buffer is overwritten with zeros when the value is dropped.
pub struct SecretString(Vec<u8>);

impl SecretString {
    /// Take ownership of `value` without copying it.
    pub fn new(value: String) -> Self {
        Self(value.into_bytes())
    }

    /// The secret. Callers must not log or store the result.
    pub fn expose(&self) -> &str {
        // Built from a `String`, so always valid UTF-8.
        std::str::from_utf8(&self.0).unwrap_or_default()
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True if the secret is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        // Zero the bytes in place first (length unchanged) so the
        // test witness can check the live buffer, then let
        // `Vec::zeroize` also clear the spare capacity.
        self.0.as_mut_slice().zeroize();
        #[cfg(test)]
        zeroize_witness::record(&self.0);
        self.0.zeroize();
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Test-only record of what each `SecretString` buffer held right
/// after it was zeroized on drop: (length, all bytes zero).
#[cfg(test)]
pub(crate) mod zeroize_witness {
    use std::cell::RefCell;

    thread_local! {
        static SEEN: RefCell<Vec<(usize, bool)>> = const { RefCell::new(Vec::new()) };
    }

    pub(crate) fn record(buf: &[u8]) {
        SEEN.with(|s| {
            s.borrow_mut()
                .push((buf.len(), buf.iter().all(|b| *b == 0)))
        });
    }

    pub(crate) fn take() -> Vec<(usize, bool)> {
        SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }
}

/// The `sensitive` part of the configuration. Absent values are
/// `None`; nothing here is ever logged or serialized.
#[derive(Debug, Default)]
pub struct SensitiveConfig {
    /// `LOCAST_TURN_SHARED_SECRET`: coturn shared secret.
    pub turn_secret: Option<SecretString>,
    /// `LOCAST_DB_KEY`: database encryption key.
    pub db_key: Option<SecretString>,
}

/// Runtime configuration resolved from environment variables.
///
/// Not `Clone`: it owns the `sensitive` secrets, and those must
/// exist exactly once in memory.
#[derive(Debug)]
pub struct Config {
    pub bind_addr: SocketAddr,
    pub log_filter: String,
    pub database_url: String,
    pub bearer_ttl_seconds: i64,
    pub challenge_ttl_ms: i64,
    pub max_frame_bytes: usize,
    pub handshake_timeout_ms: i64,
    /// Per-connection msg/s sustained rate. Exposed so tests can
    /// pin a small rate and exercise the throttle logic
    /// deterministically.
    pub rate_msgs_per_sec: u32,
    /// Per-connection msg burst budget.
    pub rate_msg_burst: u32,
    /// Per-connection bytes/s sustained rate.
    pub rate_bytes_per_sec: u32,
    /// Per-connection bytes burst budget.
    pub rate_bytes_burst: u32,
    /// Number of characters in a generated room code.
    pub room_code_length: usize,
    /// The room-code alphabet. Production MUST be the
    /// 32-char default; tests may shrink it.
    pub room_code_alphabet: String,
    /// The cap on participants per room. Includes the host.
    pub room_max_participants: u8,
    /// The grace period (ms) the host gets to re-auth before
    /// the server elects a new host.
    pub host_disconnect_grace_ms: i64,
    /// Max room-code generation collisions before
    /// `create` returns an `Internal` error.
    pub room_create_max_collisions: u8,
    /// A viewer that has not sent a `PRESENCE` in this
    /// many ms is removed from the room.
    pub participant_stale_after_ms: i64,
    /// P4-T08: a viewer that has not sent a `PRESENCE` in
    /// this many ms is flipped to `Disconnected` and a
    /// `ParticipantLeft { reason: "timeout" }` broadcast
    /// is emitted. The record remains in memory for the
    /// `participant_stale_after_ms` window so a quick
    /// reconnect can revive it. Hosts are exempt; the
    /// host-disconnect path uses `host_disconnect_grace_ms`
    /// instead.
    pub participant_disconnect_after_ms: i64,
    /// P8-T05: values from env vars marked `sensitive`.
    pub sensitive: SensitiveConfig,
}

impl Config {
    /// Load configuration from the process environment. Falls back to the
    /// defaults declared above when a variable is missing.
    ///
    /// The `sensitive` variables are read exactly once here and then
    /// removed from the environment, so later code and child processes
    /// cannot read them through it. This is best-effort: the OS may
    /// keep its own copy of the original environment block (for
    /// example `/proc/<pid>/environ` on Linux), and the platform's
    /// conversion buffers are freed without zeroizing. Call this before
    /// any other thread starts, since changing the environment while
    /// another thread reads it is unsound on some platforms.
    pub fn from_env() -> Result<Self, ConfigError> {
        let config = Self::from_lookup(|name| env::var(name))?;
        for name in SENSITIVE_ENV_VARS {
            env::remove_var(name);
        }
        Ok(config)
    }

    /// Load configuration from `get`, which behaves like
    /// `std::env::var`. Lets tests supply values without mutating the
    /// process environment.
    pub fn from_lookup<F>(get: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Result<String, env::VarError>,
    {
        let bind_addr = get("LOCAST_BIND_ADDR").unwrap_or_else(|_| DEFAULT_BIND_ADDR.to_string());
        let bind_addr: SocketAddr = bind_addr
            .parse()
            .map_err(|e| ConfigError::InvalidBindAddr(bind_addr, e))?;

        let log_filter = get("LOCAST_LOG").unwrap_or_else(|_| "info".to_string());
        let database_url =
            get("LOCAST_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());
        let bearer_ttl_seconds = parse_env_i64(&get, "LOCAST_BIND_TOKEN_TTL_SECONDS")?
            .unwrap_or(DEFAULT_BEARER_TTL_SECONDS);
        let challenge_ttl_ms = parse_env_i64(&get, "LOCAST_BIND_CHALLENGE_TTL_MS")?
            .unwrap_or(DEFAULT_CHALLENGE_TTL_MS);
        let max_frame_bytes = parse_env_usize(&get, "LOCAST_BIND_MAX_FRAME_BYTES")?
            .unwrap_or(DEFAULT_MAX_FRAME_BYTES);
        let handshake_timeout_ms = parse_env_i64(&get, "LOCAST_BIND_HANDSHAKE_TIMEOUT_MS")?
            .unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT_MS);
        let rate_msgs_per_sec =
            parse_env_u32(&get, "LOCAST_RATE_MSGS_PER_SEC")?.unwrap_or(DEFAULT_RATE_MSGS_PER_SEC);
        let rate_msg_burst =
            parse_env_u32(&get, "LOCAST_RATE_MSG_BURST")?.unwrap_or(DEFAULT_RATE_MSG_BURST);
        let rate_bytes_per_sec =
            parse_env_u32(&get, "LOCAST_RATE_BYTES_PER_SEC")?.unwrap_or(DEFAULT_RATE_BYTES_PER_SEC);
        let rate_bytes_burst =
            parse_env_u32(&get, "LOCAST_RATE_BYTES_BURST")?.unwrap_or(DEFAULT_RATE_BYTES_BURST);
        let room_code_length =
            parse_env_usize(&get, "LOCAST_ROOM_CODE_LENGTH")?.unwrap_or(DEFAULT_ROOM_CODE_LENGTH);
        let room_code_alphabet = get("LOCAST_ROOM_CODE_ALPHABET")
            .unwrap_or_else(|_| DEFAULT_ROOM_CODE_ALPHABET.to_string());
        let room_max_participants = parse_env_u8(&get, "LOCAST_ROOM_MAX_PARTICIPANTS")?
            .unwrap_or(DEFAULT_ROOM_MAX_PARTICIPANTS);
        let host_disconnect_grace_ms = parse_env_i64(&get, "LOCAST_HOST_DISCONNECT_GRACE_MS")?
            .unwrap_or(DEFAULT_HOST_DISCONNECT_GRACE_MS);
        let room_create_max_collisions = parse_env_u8(&get, "LOCAST_ROOM_CREATE_MAX_COLLISIONS")?
            .unwrap_or(DEFAULT_ROOM_CREATE_MAX_COLLISIONS);
        let participant_stale_after_ms = parse_env_i64(&get, "LOCAST_PARTICIPANT_STALE_AFTER_MS")?
            .unwrap_or(DEFAULT_PARTICIPANT_STALE_AFTER_MS);
        let participant_disconnect_after_ms =
            parse_env_i64(&get, "LOCAST_PARTICIPANT_DISCONNECT_AFTER_MS")?
                .unwrap_or(DEFAULT_PARTICIPANT_DISCONNECT_AFTER_MS);
        let sensitive = SensitiveConfig {
            turn_secret: read_sensitive(&get, ENV_TURN_SECRET)?,
            db_key: read_sensitive(&get, ENV_DB_KEY)?,
        };

        Ok(Self {
            bind_addr,
            log_filter,
            database_url,
            bearer_ttl_seconds,
            challenge_ttl_ms,
            max_frame_bytes,
            handshake_timeout_ms,
            rate_msgs_per_sec,
            rate_msg_burst,
            rate_bytes_per_sec,
            rate_bytes_burst,
            room_code_length,
            room_code_alphabet,
            room_max_participants,
            host_disconnect_grace_ms,
            room_create_max_collisions,
            participant_stale_after_ms,
            participant_disconnect_after_ms,
            sensitive,
        })
    }
}

/// Read a `sensitive` variable. An empty value counts as unset. Errors
/// name the variable but never include its value: `VarError`'s own
/// message for non-Unicode input prints the raw bytes.
fn read_sensitive<F>(get: &F, name: &str) -> Result<Option<SecretString>, ConfigError>
where
    F: Fn(&str) -> Result<String, env::VarError>,
{
    match get(name) {
        Ok(s) if s.is_empty() => Ok(None),
        Ok(s) => Ok(Some(SecretString::new(s))),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            Err(ConfigError::SensitiveNotUnicode(name.to_string()))
        }
    }
}

fn parse_env_i64<F>(get: &F, name: &str) -> Result<Option<i64>, ConfigError>
where
    F: Fn(&str) -> Result<String, env::VarError>,
{
    match get(name) {
        Ok(s) => s
            .parse::<i64>()
            .map(Some)
            .map_err(|e| ConfigError::InvalidNumber(name.to_string(), s, e.to_string())),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(ConfigError::Env(name.to_string(), e)),
    }
}

fn parse_env_usize<F>(get: &F, name: &str) -> Result<Option<usize>, ConfigError>
where
    F: Fn(&str) -> Result<String, env::VarError>,
{
    match get(name) {
        Ok(s) => s
            .parse::<usize>()
            .map(Some)
            .map_err(|e| ConfigError::InvalidNumber(name.to_string(), s, e.to_string())),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(ConfigError::Env(name.to_string(), e)),
    }
}

fn parse_env_u32<F>(get: &F, name: &str) -> Result<Option<u32>, ConfigError>
where
    F: Fn(&str) -> Result<String, env::VarError>,
{
    match get(name) {
        Ok(s) => s
            .parse::<u32>()
            .map(Some)
            .map_err(|e| ConfigError::InvalidNumber(name.to_string(), s, e.to_string())),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(ConfigError::Env(name.to_string(), e)),
    }
}

/// Parse a `u8` setting. A value above 255 is an error: parsing it as a
/// `u32` and casting would silently wrap it (256 becomes 0, 300 becomes 44).
fn parse_env_u8<F>(get: &F, name: &str) -> Result<Option<u8>, ConfigError>
where
    F: Fn(&str) -> Result<String, env::VarError>,
{
    match get(name) {
        Ok(s) => s
            .parse::<u8>()
            .map(Some)
            .map_err(|e| ConfigError::InvalidNumber(name.to_string(), s, e.to_string())),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(ConfigError::Env(name.to_string(), e)),
    }
}

/// Errors raised while loading configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("LOCAST_BIND_ADDR={0} is not a valid socket address: {1}")]
    InvalidBindAddr(String, std::net::AddrParseError),

    #[error("environment variable {0} is not a valid number (got {1:?}): {2}")]
    InvalidNumber(String, String, String),

    #[error("failed to read environment variable {0}: {1}")]
    Env(String, env::VarError),

    /// A `sensitive` variable held non-Unicode bytes. The value is
    /// deliberately not included.
    #[error("environment variable {0} is not valid Unicode")]
    SensitiveNotUnicode(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Deliberately unusual values so a leak is unambiguous.
    const TURN: &str = "turn-secret-Q7v1pZ-9f3a";
    const DBKEY: &str = "db-key-K2m8xT-00c4e1";

    fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, env::VarError> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| map.get(name).cloned().ok_or(env::VarError::NotPresent)
    }

    #[test]
    fn sensitive_values_are_loaded() {
        let cfg = Config::from_lookup(lookup(&[(ENV_TURN_SECRET, TURN), (ENV_DB_KEY, DBKEY)]))
            .expect("config");
        assert_eq!(
            cfg.sensitive.turn_secret.as_ref().map(SecretString::expose),
            Some(TURN)
        );
        assert_eq!(
            cfg.sensitive.db_key.as_ref().map(SecretString::expose),
            Some(DBKEY)
        );
    }

    #[test]
    fn small_numeric_settings_reject_values_that_do_not_fit() {
        // These used to be parsed as u32 and cast to u8: 256 became 0 and
        // 300 became 44, silently.
        for (name, bad) in [
            ("LOCAST_ROOM_MAX_PARTICIPANTS", "256"),
            ("LOCAST_ROOM_MAX_PARTICIPANTS", "300"),
            ("LOCAST_ROOM_CREATE_MAX_COLLISIONS", "256"),
            ("LOCAST_ROOM_CREATE_MAX_COLLISIONS", "70000"),
            ("LOCAST_ROOM_MAX_PARTICIPANTS", "-1"),
        ] {
            let err = Config::from_lookup(lookup(&[(name, bad)]))
                .expect_err(&format!("{name}={bad} must be refused"));
            assert!(
                matches!(&err, ConfigError::InvalidNumber(n, v, _) if n == name && v == bad),
                "{name}={bad}: {err:?}"
            );
        }
        let cfg = Config::from_lookup(lookup(&[
            ("LOCAST_ROOM_MAX_PARTICIPANTS", "255"),
            ("LOCAST_ROOM_CREATE_MAX_COLLISIONS", "7"),
        ]))
        .expect("values that fit are accepted");
        assert_eq!(cfg.room_max_participants, 255);
        assert_eq!(cfg.room_create_max_collisions, 7);
    }

    #[test]
    fn unset_or_empty_sensitive_values_are_none() {
        let cfg = Config::from_lookup(lookup(&[(ENV_DB_KEY, "")])).expect("config");
        assert!(cfg.sensitive.turn_secret.is_none());
        assert!(cfg.sensitive.db_key.is_none());
    }

    #[test]
    fn debug_output_never_contains_secret_values() {
        let cfg = Config::from_lookup(lookup(&[(ENV_TURN_SECRET, TURN), (ENV_DB_KEY, DBKEY)]))
            .expect("config");
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains(TURN), "turn secret leaked: {dbg}");
        assert!(!dbg.contains(DBKEY), "db key leaked: {dbg}");
        assert!(dbg.contains("SecretString(<redacted>)"));
        // Non-secret diagnostics remain.
        assert!(dbg.contains("bind_addr"));
        let shown = cfg.sensitive.turn_secret.as_ref().map(|s| s.to_string());
        assert_eq!(shown.as_deref(), Some("<redacted>"));
    }

    #[test]
    fn dropping_config_zeroizes_secret_buffers() {
        // Best-effort memory check (roadmap P8-T05): the drop hook
        // records each secret buffer right after zeroizing it, while
        // the allocation is still live, so the bytes the secrets
        // occupied are observed to be zero without `unsafe`.
        zeroize_witness::take();
        let cfg = Config::from_lookup(lookup(&[(ENV_TURN_SECRET, TURN), (ENV_DB_KEY, DBKEY)]))
            .expect("config");
        drop(cfg);
        let mut seen = zeroize_witness::take();
        seen.sort_unstable();
        let mut want = vec![(TURN.len(), true), (DBKEY.len(), true)];
        want.sort_unstable();
        assert_eq!(seen, want);
    }

    // `Config::from_env` (env read + removal) is tested in
    // `tests/config_env.rs`, its own process, so mutating the
    // environment cannot race these parallel unit tests.

    #[test]
    fn non_unicode_sensitive_value_error_omits_the_value() {
        let raw = std::ffi::OsString::from(TURN);
        let get = move |name: &str| {
            if name == ENV_TURN_SECRET {
                Err(env::VarError::NotUnicode(raw.clone()))
            } else {
                Err(env::VarError::NotPresent)
            }
        };
        let err = Config::from_lookup(get).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains(ENV_TURN_SECRET));
        assert!(!msg.contains(TURN), "secret leaked into error: {msg}");
        assert!(!format!("{err:?}").contains(TURN));
    }
}
