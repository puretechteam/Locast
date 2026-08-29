-- Locast migration 0002: add the `recent_rooms` table for P2-T08.
--
-- The `/rooms` page renders two lists: an "Active" entry (the single
-- in-memory `RoomSummaryIpc` cache from the `RoomClient`) and a "Recent"
-- list of rooms the local client has joined or hosted in this profile.
-- "Recent" persists across restarts so the acceptance criterion
-- "a client that creates a room, restarts, and visits `/rooms` still
-- sees the room" holds. The recents row is written on every
-- `room://state` event with the host's display name captured at
-- write time; see `apps/client/src/pages/rooms.index/RoomsIndexPage.tsx`
-- and `apps/client/src-tauri/src/storage/rooms.rs`.

CREATE TABLE recent_rooms (
    room_id           TEXT PRIMARY KEY,
    code              TEXT NOT NULL,
    title             TEXT NOT NULL,
    host_user_id      TEXT NOT NULL,
    host_display_name TEXT NOT NULL,
    role              TEXT NOT NULL CHECK (role IN ('host', 'guest')),
    last_seen_ms      INTEGER NOT NULL,
    last_ended_ms     INTEGER,
    created_ms        INTEGER NOT NULL
);
CREATE INDEX ix_recent_rooms_last_seen ON recent_rooms(last_seen_ms DESC);
