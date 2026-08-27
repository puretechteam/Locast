-- Locast signaling server - initial schema.
--
-- P2-T02. Mirrors the user_identities / bearer_tokens layout described
-- in `docs/ARCHITECTURE.md` section 20.4.4 and 21.3. The `user_id`
-- is a server-assigned UUID v7; the `pubkey` is the canonical
-- identifier the architecture treats as the user identity.
--
-- This file is consumed by `sqlx::migrate!()` in `apps/server/src/db.rs`.

CREATE TABLE user_identities (
    user_id    TEXT PRIMARY KEY,
    pubkey     BLOB NOT NULL UNIQUE,
    first_seen INTEGER NOT NULL,
    last_seen  INTEGER NOT NULL
);
CREATE INDEX idx_user_pubkey ON user_identities(pubkey);

CREATE TABLE bearer_tokens (
    token_hash BLOB PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES user_identities(user_id) ON DELETE CASCADE,
    pubkey     BLOB NOT NULL,
    expires_ms INTEGER NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX idx_bearer_user ON bearer_tokens(user_id);
CREATE INDEX idx_bearer_expires ON bearer_tokens(expires_ms);
