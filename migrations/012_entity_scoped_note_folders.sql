-- Entity-scoped note folders: lets a folder organize one specific attached
-- entity's own notes (e.g. a cunav ticket's "Investigation" / "Customer
-- comms" sub-folders) rather than only ever being a team-wide grouping.
--
-- `team_id` stays required on every folder, entity-scoped or not -- it's
-- still how a folder resolves its organization and who may act on it
-- (`resolve_folder_for_caller`'s team-membership check doesn't change).
-- The three new columns are purely additional scope on top of that, all-
-- or-nothing together -- the same invariant `AttachRequest` already uses
-- for notes themselves (see `models/note.rs`'s `CreateNoteRequest.attach`):
-- either all three are NULL (today's behavior: a team-wide folder, listed
-- in `GET /note-folders` and tack's own Navigator) or all three are set (an
-- entity-scoped folder, listed only via `GET /note-folders/by-entity` and
-- invisible to team-wide listing).
ALTER TABLE note_folders ADD COLUMN IF NOT EXISTS owning_service TEXT;
ALTER TABLE note_folders ADD COLUMN IF NOT EXISTS entity_type TEXT;
ALTER TABLE note_folders ADD COLUMN IF NOT EXISTS entity_id TEXT;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'note_folders_entity_scope_all_or_nothing'
    ) THEN
        ALTER TABLE note_folders ADD CONSTRAINT note_folders_entity_scope_all_or_nothing
            CHECK (
                (owning_service IS NULL AND entity_type IS NULL AND entity_id IS NULL)
                OR (owning_service IS NOT NULL AND entity_type IS NOT NULL AND entity_id IS NOT NULL)
            );
    END IF;
END $$;

-- Looked up by (organization_id, owning_service, entity_type, entity_id),
-- same triple `content_attachments`/`list_notes_by_attachment` already
-- index on -- partial, since the large majority of rows (team-wide
-- folders) never match this predicate.
CREATE INDEX IF NOT EXISTS note_folders_entity_idx
    ON note_folders (organization_id, owning_service, entity_type, entity_id)
    WHERE owning_service IS NOT NULL;
