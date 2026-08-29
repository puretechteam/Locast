-- P3-T03: room manifest publication.
--
-- Mirrors the client-side `room_manifests` table introduced by
-- `apps/client/src-tauri/migrations/0001_init.sql`. The server is the
-- authoritative store of every manifest published into a room: the
-- host's MANIFEST_PUBLISH envelope is INSERTed here before the
-- server rebroadcasts MANIFEST_PUBLISHED to the other participants.
-- Viewers that join mid-room can fetch the latest manifest by
-- (room_id, max(version)).
--
-- The host's signature is verified by the viewer against the
-- invite's `h=` parameter via TOFU; the server's defense-in-depth
-- check is a `locast_manifest::verify_manifest` call at the relay,
-- but it is NOT the trust boundary.
--
-- `version` is per-room monotonic (max+1). `manifest_hash` is the
-- BLAKE3 of the canonical bytes the host signed; storing it lets
-- P3-T07 detect duplicate publishes without re-canonicalizing.
--
-- `manifest_json` is the host's exact signed blob (host_signature
-- block populated, as on the wire). P3-T07 will add a `version`
-- cache on the in-memory `RoomState` so `dispatch_authed` can
-- look it up cheaply.

CREATE TABLE room_manifests (
    id              TEXT PRIMARY KEY,
    room_id         TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    version         INTEGER NOT NULL,
    created_at      INTEGER NOT NULL,
    manifest_json   TEXT NOT NULL,
    manifest_hash   BLOB NOT NULL,
    host_user_id    TEXT NOT NULL REFERENCES user_identities(user_id) ON DELETE RESTRICT,
    UNIQUE (room_id, version)
);
CREATE INDEX ix_room_manifests_room ON room_manifests(room_id, version DESC);
