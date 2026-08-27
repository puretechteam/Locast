-- Locast initial schema. Migration 0001_init.
--
-- This file is the source of truth for the desktop client's local SQLite
-- database. The Rust side runs it via `sqlx::migrate!()` at startup.
-- See docs/ARCHITECTURE.md section 7 for the design rationale and
-- section 26.2.1 for the file location.
--
-- The CREATE TABLE order below is chosen so that every foreign key
-- references a table that has already been created in the same
-- migration. SQLite's foreign_keys enforcement is per-connection and
-- applies to DDL as well as DML; with foreign_keys = ON an unresolved
-- forward reference at CREATE TABLE time would fail. Reordering here
-- is the cleanest way to satisfy that constraint without resorting to
-- `PRAGMA defer_foreign_keys` or `OFF` for the migration.

-- -----------------------------------------------------------------------------
-- Tables (in dependency order)
-- -----------------------------------------------------------------------------

-- user_identities: every Locast user we have ever met. Display name is
-- local-only; the public key is the stable identifier.
CREATE TABLE user_identities (
    id           TEXT PRIMARY KEY,                                 -- sha256(public_key) hex
    public_key   TEXT NOT NULL UNIQUE,                             -- base64 ed25519 public key
    display_name TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    last_seen    INTEGER NOT NULL
);
CREATE INDEX ix_identities_last_seen ON user_identities(last_seen DESC);

-- rooms: a watch-together session.
CREATE TABLE rooms (
    id              TEXT PRIMARY KEY,
    code            TEXT NOT NULL UNIQUE,                          -- 6-char invite code
    host_user_id    TEXT NOT NULL REFERENCES user_identities(id) ON DELETE RESTRICT,
    created_at      INTEGER NOT NULL,
    ended_at        INTEGER,
    state           TEXT NOT NULL CHECK (state IN ('open','playing','paused','ended','cancelled')),
    manifest_id     TEXT REFERENCES room_manifests(id) ON DELETE SET NULL,
    settings        TEXT NOT NULL DEFAULT '{}'                     -- JSON
);
CREATE INDEX ix_rooms_state      ON rooms(state);
CREATE INDEX ix_rooms_host       ON rooms(host_user_id);
CREATE INDEX ix_rooms_created    ON rooms(created_at DESC);

-- room_manifests: signed manifest snapshots. Immutable; new manifest = new row.
CREATE TABLE room_manifests (
    id          TEXT PRIMARY KEY,
    room_id     TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL,
    media       TEXT NOT NULL,                                     -- JSON array
    subtitles   TEXT NOT NULL DEFAULT '[]',                        -- JSON array
    version     INTEGER NOT NULL,
    UNIQUE (room_id, version)
);
CREATE INDEX ix_manifests_room ON room_manifests(room_id, version DESC);

-- media_items: every file we know about.
CREATE TABLE media_items (
    id              TEXT PRIMARY KEY,                              -- uuid v4
    sha256          TEXT NOT NULL UNIQUE,                          -- 64 hex chars
    blake3          TEXT NOT NULL,                                 -- 64 hex chars
    size_bytes      INTEGER NOT NULL CHECK (size_bytes >= 0),
    filename        TEXT NOT NULL,                                 -- sanitized
    relative_path   TEXT NOT NULL UNIQUE COLLATE NOCASE,           -- library-relative
    mime            TEXT NOT NULL,                                 -- e.g. video/mp4
    duration_ms     INTEGER CHECK (duration_ms IS NULL OR duration_ms >= 0),
    width           INTEGER CHECK (width  IS NULL OR width  > 0),
    height          INTEGER CHECK (height IS NULL OR height > 0),
    video_codec     TEXT,                                          -- h264, hevc, av1, vp9, ...
    audio_codec     TEXT,                                          -- aac, opus, ...
    container       TEXT,                                          -- mp4, matroska, webm
    status          TEXT NOT NULL CHECK (status IN ('permanent','temporary')),
    created_at      INTEGER NOT NULL,                              -- unix ms
    last_seen_at    INTEGER NOT NULL,
    last_room_id    TEXT REFERENCES rooms(id) ON DELETE SET NULL,
    source_url      TEXT,                                          -- optional, for provenance
    provenance      TEXT NOT NULL DEFAULT '{}'                     -- JSON
);
CREATE INDEX ix_media_status        ON media_items(status);
CREATE INDEX ix_media_last_seen     ON media_items(last_seen_at DESC);
CREATE INDEX ix_media_last_room     ON media_items(last_room_id);
CREATE INDEX ix_media_size          ON media_items(size_bytes);

-- media_subtitles: sidecar subtitle tracks linked to a media item.
CREATE TABLE media_subtitles (
    id              TEXT PRIMARY KEY,
    media_id        TEXT NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    language        TEXT,                                          -- BCP-47, best-effort
    label           TEXT NOT NULL,                                 -- user-visible
    filename        TEXT NOT NULL,                                 -- sanitized
    relative_path   TEXT NOT NULL,                                 -- under media dir
    sha256          TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL CHECK (size_bytes >= 0),
    codec           TEXT NOT NULL CHECK (codec IN ('srt','ass','ssa','vtt','webvtt')),
    UNIQUE (media_id, filename COLLATE NOCASE)
);
CREATE INDEX ix_subtitles_media ON media_subtitles(media_id);

-- room_participants: who has joined which room and in what role.
CREATE TABLE room_participants (
    id                TEXT PRIMARY KEY,
    room_id           TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    user_id           TEXT NOT NULL REFERENCES user_identities(id) ON DELETE RESTRICT,
    display_name      TEXT NOT NULL,
    role              TEXT NOT NULL CHECK (role IN ('host','cohost','guest')),
    joined_at         INTEGER NOT NULL,
    left_at           INTEGER,
    connection_state  TEXT NOT NULL CHECK (connection_state IN
                          ('connecting','connected','reconnecting','disconnected','left')),
    capabilities      TEXT NOT NULL DEFAULT '{}',                  -- JSON
    UNIQUE (room_id, user_id)
);
CREATE INDEX ix_participants_room     ON room_participants(room_id);
CREATE INDEX ix_participants_user     ON room_participants(user_id);
CREATE INDEX ix_participants_state    ON room_participants(connection_state);

-- downloads: one row per file being fetched into the library.
CREATE TABLE downloads (
    id                TEXT PRIMARY KEY,
    media_id          TEXT NOT NULL REFERENCES media_items(id) ON DELETE CASCADE,
    room_id           TEXT REFERENCES rooms(id) ON DELETE SET NULL,
    user_id           TEXT NOT NULL REFERENCES user_identities(id) ON DELETE RESTRICT,
    state             TEXT NOT NULL CHECK (state IN
                          ('pending','connecting','transferring','verifying',
                           'complete','failed','paused','cancelled')),
    total_bytes       INTEGER NOT NULL CHECK (total_bytes >= 0),
    transferred_bytes INTEGER NOT NULL DEFAULT 0 CHECK (transferred_bytes >= 0),
    started_at        INTEGER,
    completed_at      INTEGER,
    last_error        TEXT,
    source_peer_id    TEXT,                                         -- primary; multi-source uses bitmap
    chunk_size_bytes  INTEGER NOT NULL DEFAULT 262144               -- 256 KiB
);
CREATE INDEX ix_downloads_state     ON downloads(state);
CREATE INDEX ix_downloads_media     ON downloads(media_id);
CREATE INDEX ix_downloads_room      ON downloads(room_id);

-- download_chunks: per-chunk bookkeeping. The union of state=verified/recv'd
-- chunks is the source of truth for resumability.
-- The column is named `index` to match docs/ARCHITECTURE.md section 7.
-- It is double-quoted throughout the migration because `INDEX` is a
-- SQL keyword in some SQLite grammar contexts (notably `CREATE INDEX`
-- column lists) and would otherwise raise a syntax error there.
CREATE TABLE download_chunks (
    id          TEXT PRIMARY KEY,
    download_id TEXT NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    "index"     INTEGER NOT NULL CHECK ("index" >= 0),
    offset      INTEGER NOT NULL CHECK (offset >= 0),
    length      INTEGER NOT NULL CHECK (length > 0),
    sha256      TEXT NOT NULL,
    state       TEXT NOT NULL CHECK (state IN
                    ('pending','in_flight','received','verified','failed')),
    UNIQUE (download_id, "index")
);
CREATE INDEX ix_chunks_download_state ON download_chunks(download_id, state);
-- Fast "give me the next pending chunk" query:
CREATE INDEX ix_chunks_pending        ON download_chunks(download_id, "index")
    WHERE state = 'pending';

-- room_events: append-only log. (room_id, seq) is strictly monotonic.
CREATE TABLE room_events (
    id          TEXT PRIMARY KEY,
    room_id     TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    sender_id   TEXT REFERENCES user_identities(id) ON DELETE SET NULL,
    type        TEXT NOT NULL,                                     -- play|pause|seek|chat|draw|...
    payload     TEXT NOT NULL,                                     -- JSON
    created_at  INTEGER NOT NULL,                                  -- client clock
    server_ts   INTEGER,                                           -- server clock; NULL if offline
    UNIQUE (room_id, seq)
);
CREATE INDEX ix_events_room_seq ON room_events(room_id, seq);

-- presence: a thin "who is currently connected" table, used for the
-- participant strip without scanning room_events.
CREATE TABLE presence (
    id                TEXT PRIMARY KEY,
    room_id           TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    user_id           TEXT NOT NULL REFERENCES user_identities(id) ON DELETE CASCADE,
    last_seen         INTEGER NOT NULL,
    connection_state  TEXT NOT NULL CHECK (connection_state IN
                          ('online','away','reconnecting','offline')),
    UNIQUE (room_id, user_id)
);
CREATE INDEX ix_presence_room ON presence(room_id);
CREATE INDEX ix_presence_user ON presence(user_id);

-- room_invites: pre-generated invite codes, with optional expiry and max uses.
CREATE TABLE room_invites (
    id          TEXT PRIMARY KEY,
    room_id     TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    code        TEXT NOT NULL UNIQUE,
    created_by  TEXT NOT NULL REFERENCES user_identities(id) ON DELETE RESTRICT,
    expires_at  INTEGER,                                           -- NULL = never
    used_at     INTEGER,                                           -- first use; NULL if unused
    max_uses    INTEGER NOT NULL DEFAULT 1 CHECK (max_uses > 0)
);
CREATE INDEX ix_invites_room ON room_invites(room_id);
CREATE INDEX ix_invites_code ON room_invites(code);

-- settings: typed key/value bag. Values are JSON; keys are dotted namespaces.
CREATE TABLE settings (
    key    TEXT PRIMARY KEY,
    value  TEXT NOT NULL                                           -- JSON
);

-- -----------------------------------------------------------------------------
-- FTS5 virtual table for library search
-- -----------------------------------------------------------------------------

CREATE VIRTUAL TABLE media_items_fts USING fts5(
    filename,
    display_label,                                                 -- user-set label, stored in media_items.provenance
    tokenize = 'unicode61 remove_diacritics 2',
    content = 'media_items',
    content_rowid = 'rowid'
);

-- Triggers to keep FTS in sync with media_items.
CREATE TRIGGER media_items_ai AFTER INSERT ON media_items BEGIN
    INSERT INTO media_items_fts(rowid, filename, display_label)
    VALUES (new.rowid, new.filename, COALESCE(json_extract(new.provenance, '$.label'), ''));
END;
CREATE TRIGGER media_items_ad AFTER DELETE ON media_items BEGIN
    INSERT INTO media_items_fts(media_items_fts, rowid, filename, display_label)
    VALUES ('delete', old.rowid, old.filename, '');
END;
CREATE TRIGGER media_items_au AFTER UPDATE ON media_items BEGIN
    INSERT INTO media_items_fts(media_items_fts, rowid, filename, display_label)
    VALUES ('delete', old.rowid, old.filename, '');
    INSERT INTO media_items_fts(rowid, filename, display_label)
    VALUES (new.rowid, new.filename, COALESCE(json_extract(new.provenance, '$.label'), ''));
END;
