-- P7-T01: §22.3.1 session_resume_tokens table. The server
-- mints a fresh 32-byte token on every AUTH_OK and stores
-- sha256(token) keyed to the (user_id, connection_epoch,
-- room_id) binding. The plaintext is held by the client only.

CREATE TABLE session_resume_tokens (
    token_hash         BLOB PRIMARY KEY,
    user_id            TEXT NOT NULL REFERENCES user_identities(user_id) ON DELETE CASCADE,
    connection_epoch   INTEGER NOT NULL,
    room_id            TEXT REFERENCES rooms(id) ON DELETE CASCADE,
    expires_ms         INTEGER NOT NULL,
    created_ms         INTEGER NOT NULL
);
CREATE INDEX idx_resume_user ON session_resume_tokens(user_id);
CREATE INDEX idx_resume_expires ON session_resume_tokens(expires_ms);