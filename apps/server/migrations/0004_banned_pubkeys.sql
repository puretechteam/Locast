-- P7-T01: §21.3 banlist table. Server operators seed this
-- table with the raw 32-byte Ed25519 public keys of users
-- who are blocked from connecting. The auth path looks up
-- every AUTH's pubkey here and emits AUTH_FAIL(Banned)
-- before any signature verification work.

CREATE TABLE banned_pubkeys (
    pubkey BLOB PRIMARY KEY,
    reason TEXT,
    created_ms INTEGER NOT NULL
);