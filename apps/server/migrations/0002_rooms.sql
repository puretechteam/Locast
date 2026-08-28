-- P2-T04: room registry and participant tracking.
-- Mirrors the v1 subset of `docs/ARCHITECTURE.md` §7 and §20.5.

CREATE TABLE rooms (
    id                       TEXT PRIMARY KEY,
    code                     TEXT NOT NULL UNIQUE,
    title                    TEXT NOT NULL,
    host_user_id             TEXT NOT NULL REFERENCES user_identities(user_id) ON DELETE RESTRICT,
    host_pubkey              BLOB NOT NULL,
    host_migration_enabled   INTEGER NOT NULL DEFAULT 0,
    password_hash            BLOB,                       -- nullable; reserved for future Argon2id
    state                    TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open','ended','cancelled')),
    host_disconnect_deadline_ms INTEGER,                  -- set when host transport loss + migration on
    created_ms               INTEGER NOT NULL,
    ended_ms                 INTEGER,
    last_activity_ms         INTEGER NOT NULL
);
CREATE INDEX ix_rooms_code       ON rooms(code);
CREATE INDEX ix_rooms_host       ON rooms(host_user_id);
CREATE INDEX ix_rooms_state      ON rooms(state);

CREATE TABLE room_participants (
    room_id           TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    user_id           TEXT NOT NULL REFERENCES user_identities(user_id) ON DELETE RESTRICT,
    pubkey            BLOB NOT NULL,
    display_name      TEXT NOT NULL,
    is_host           INTEGER NOT NULL DEFAULT 0,
    joined_ms         INTEGER NOT NULL,
    left_ms           INTEGER,
    last_seen_ms      INTEGER NOT NULL,
    status            TEXT NOT NULL DEFAULT 'connected' CHECK (status IN ('joining','connected','reconnecting','disconnected','left')),
    cap_set           INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (room_id, user_id)
);
CREATE INDEX ix_room_part_room     ON room_participants(room_id);
CREATE INDEX ix_room_part_user     ON room_participants(user_id);
CREATE INDEX ix_room_part_status   ON room_participants(status);
