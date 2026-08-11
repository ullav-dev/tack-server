-- Idea Boards CRUD API (Phase 5) addresses a sticky by its note_id alone
-- (PATCH/DELETE /stickies/{note_id} -- a note is only ever a sticky on the
-- one board it's filed in, so board_id isn't needed in the URL), and looks
-- up a sticky by its soft-linked external entity (GET /stickies/by-entity,
-- the tack-server equivalent of awe-server's get_sticky_by_workflow).
-- idea_board_stickies' own PRIMARY KEY is (board_id, note_id,
-- organization_id) -- great for "this board's stickies," useless for either
-- lookup above without a full partition scan. Add the two indexes that
-- shape actually needs.
CREATE INDEX IF NOT EXISTS idea_board_stickies_note_idx
    ON idea_board_stickies (note_id, organization_id);

CREATE INDEX IF NOT EXISTS idea_board_stickies_entity_idx
    ON idea_board_stickies (organization_id, linked_entity_type, linked_entity_id)
    WHERE linked_entity_type IS NOT NULL;
