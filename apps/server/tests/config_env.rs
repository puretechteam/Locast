//! P8-T05: `Config::from_env` reads the `sensitive` env vars once and
//! removes them from the process environment.
//!
//! This file holds exactly one test so it runs alone in its own
//! process: changing the environment while another thread reads it is
//! unsound on some platforms.

use locast_server::config::{Config, ENV_DB_KEY, ENV_TURN_SECRET};

// Deliberately unusual values so a leak is unambiguous.
const TURN: &str = "turn-secret-Wx4r8Lq-71b2";
const DBKEY: &str = "db-key-Pz9e3Hn-a05f";

#[test]
fn from_env_reads_sensitive_vars_once_and_removes_them() {
    std::env::set_var(ENV_TURN_SECRET, TURN);
    std::env::set_var(ENV_DB_KEY, DBKEY);

    let cfg = Config::from_env().expect("config");

    assert_eq!(
        cfg.sensitive.turn_secret.as_ref().map(|s| s.expose()),
        Some(TURN)
    );
    assert_eq!(
        cfg.sensitive.db_key.as_ref().map(|s| s.expose()),
        Some(DBKEY)
    );
    assert!(
        std::env::var_os(ENV_TURN_SECRET).is_none(),
        "turn secret left in env"
    );
    assert!(std::env::var_os(ENV_DB_KEY).is_none(), "db key left in env");

    let dbg = format!("{cfg:?}");
    assert!(
        !dbg.contains(TURN) && !dbg.contains(DBKEY),
        "secret in Debug: {dbg}"
    );
}
