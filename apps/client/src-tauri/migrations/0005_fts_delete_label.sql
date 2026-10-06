-- The FTS5 'delete' command must be given exactly the values that were indexed.
-- The insert trigger indexes the provenance label, but the delete and update
-- triggers passed '' for it, so removing or updating a row that had a label
-- left phantom tokens in the index (and can surface as a corrupt-vtab error on
-- later MATCH queries). Recreate both triggers with the old row's label.
-- (No 'rebuild': the external-content table names a `display_label` column
-- that media_items does not have, so a rebuild cannot read its content.)

DROP TRIGGER media_items_ad;
DROP TRIGGER media_items_au;

CREATE TRIGGER media_items_ad AFTER DELETE ON media_items BEGIN
    INSERT INTO media_items_fts(media_items_fts, rowid, filename, display_label)
    VALUES ('delete', old.rowid, old.filename, COALESCE(json_extract(old.provenance, '$.label'), ''));
END;
CREATE TRIGGER media_items_au AFTER UPDATE ON media_items BEGIN
    INSERT INTO media_items_fts(media_items_fts, rowid, filename, display_label)
    VALUES ('delete', old.rowid, old.filename, COALESCE(json_extract(old.provenance, '$.label'), ''));
    INSERT INTO media_items_fts(rowid, filename, display_label)
    VALUES (new.rowid, new.filename, COALESCE(json_extract(new.provenance, '$.label'), ''));
END;
