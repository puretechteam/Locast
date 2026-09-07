-- P7-T01 review-fix: add (connection_epoch, room_id) to
-- bearer_tokens so the WS layer can assert the triple
-- binding advertised by BearerBinding::matches on every
-- AUTH_RESUME / post-handshake bearer validation. Older
-- bearer rows are stamped with the original mint-time
-- sentinel (epoch=0, room_id=NULL) which the WS layer
-- treats as "pre-binding; legacy"; new AUTH_OK mints must
-- always include the current connection_epoch.

ALTER TABLE bearer_tokens ADD COLUMN connection_epoch INTEGER NOT NULL DEFAULT 0;
ALTER TABLE bearer_tokens ADD COLUMN room_id TEXT;