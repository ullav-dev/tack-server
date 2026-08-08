-- Notes folders: a flat (non-nested), per-team grouping for top-level notes
-- -- explicitly scoped to top-level notes only. A reply is a `notes` row too
-- (`parent_id` set), but replies live inside their parent's thread, not in a
-- folder of their own -- the CHECK constraint below enforces that at the
-- schema level rather than trusting every write path to get it right.
--
-- Deliberately flat, not a `pages`-style ltree tree: nothing asked for in
-- this pass needs nested folders, and a flat table is trivially extensible
-- to nesting later (add a `parent_id` self-FK) without a breaking migration.
--
-- Hash-partitioned by organization_id into the same 32 fixed buckets as
-- every other tenant-scoped content table, for the same reasons documented
-- in 001_notes.sql's header.
CREATE TABLE IF NOT EXISTS note_folders (
    id              UUID        NOT NULL DEFAULT gen_random_uuid(),
    organization_id UUID        NOT NULL,
    team_id         UUID        NOT NULL,
    name            TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, organization_id)
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS note_folders_p%1$s PARTITION OF note_folders FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS note_folders_team_id_idx ON note_folders (team_id);

-- A note's folder. NULL = unfiled. Only ever set on a top-level note (see
-- the CHECK constraint below) -- moving a whole thread's folder happens by
-- reassigning the top-level note; replies aren't independently filed.
ALTER TABLE notes ADD COLUMN IF NOT EXISTS folder_id UUID;

CREATE INDEX IF NOT EXISTS notes_folder_id_idx ON notes (folder_id);

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'notes_folder_id_top_level_only'
    ) THEN
        ALTER TABLE notes ADD CONSTRAINT notes_folder_id_top_level_only
            CHECK (parent_id IS NULL OR folder_id IS NULL);
    END IF;
END $$;

-- No ON DELETE SET NULL here: it's a composite FK (organization_id is along
-- for the ride, per the partition-key-in-every-FK rule from 001_notes.sql),
-- and SET NULL on a composite FK nulls *every* referencing column -- which
-- would try to null notes.organization_id too and fail its NOT NULL
-- constraint. Instead `db::note_folders::delete_folder` explicitly clears
-- `folder_id` on every note in the folder in the same transaction before
-- deleting the folder row, so this default RESTRICT never actually fires in
-- practice.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'notes_folder_id_fkey'
    ) THEN
        ALTER TABLE notes ADD CONSTRAINT notes_folder_id_fkey
            FOREIGN KEY (folder_id, organization_id) REFERENCES note_folders (id, organization_id);
    END IF;
END $$;
