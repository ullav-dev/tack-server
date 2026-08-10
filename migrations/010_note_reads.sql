-- Per-user unread tracking for Notes. Generic -- not tied to any one app's
-- ticket/unread-badge convention -- a read marker is just "this user has
-- seen this thread as of this timestamp"; "is it unread" is a live
-- comparison against the thread's own activity, computed at read time by
-- `db::note_reads::unread_status`, never denormalized onto the note itself
-- (same "resolved live, not cached" posture as visibility ACLs and page
-- permissions elsewhere in this schema).
--
-- Only top-level notes are marked read/unread -- a reply has no independent
-- read state of its own, same as a reply having no independent folder (see
-- 008_note_folders.sql's `notes_folder_id_top_level_only` CHECK). Marking a
-- thread read covers everything in it; new replies since then are what make
-- it unread again.
--
-- Hash-partitioned by organization_id into the same 32 fixed buckets as
-- every other tenant-scoped content table, for the same reasons documented
-- in 001_notes.sql's header.
CREATE TABLE IF NOT EXISTS note_reads (
    user_id         UUID        NOT NULL,
    note_id         UUID        NOT NULL,
    organization_id UUID        NOT NULL,
    read_at         TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, note_id, organization_id),
    FOREIGN KEY (note_id, organization_id) REFERENCES notes (id, organization_id) ON DELETE CASCADE
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS note_reads_p%1$s PARTITION OF note_reads FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;
