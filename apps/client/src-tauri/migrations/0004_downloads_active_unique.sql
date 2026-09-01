-- P3-T13: partial UNIQUE index preventing two in-flight active downloads
-- for the same content from the same user in the same room.
-- Active states: pending, connecting, transferring, verifying, paused.
-- Complete / failed / cancelled are intentionally NOT included -- a
-- finished download can coexist with a future re-download.
CREATE UNIQUE INDEX ux_downloads_active
    ON downloads(media_id, COALESCE(room_id, ''), user_id)
    WHERE state IN ('pending','connecting','transferring','verifying','paused');