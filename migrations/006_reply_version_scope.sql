-- Ties a reply to whichever version of its parent note was current when the
-- reply was made. A reply written against v1 is only relevant to v1 -- once
-- the owner explicitly saves a new version (v2), that reply shouldn't keep
-- showing up as if it were a comment on the live v2 body. `NULL` for
-- top-level notes (this only ever applies to replies) and for pre-existing
-- rows, which have no recorded context and are treated by the application
-- layer as belonging to whatever the current version is.
ALTER TABLE notes ADD COLUMN IF NOT EXISTS in_reply_to_version INTEGER;
