-- Named-snapshot version history for Pages (implementation sequencing step
-- 8c). Explicit, user-triggered only -- same model as note_revisions, per
-- the same reasoning: "a Version is really a solid decision made by an
-- authorised user", not an automatic side effect of every autosave-style
-- edit. This is deliberately NOT the periodic-automatic-snapshot mechanism
-- this project's earlier storage-architecture research floated for bounding
-- Yjs update-log bloat via GC -- that's a distinct, still-separate,
-- unaddressed operational concern (see tack/CLAUDE.md's Pages section);
-- this table exists purely for user-visible, browsable history.
--
-- Stores content_markdown (not a Yjs binary snapshot): simple, human-
-- readable, matches note_revisions.body_markdown exactly, and page_docs
-- .content_markdown is already kept accurate by tack-hocuspocus's
-- onStoreDocument (see that repo's src/markdown.ts). View-only history --
-- no "restore this version into the live Yjs doc" feature, same as Notes.
CREATE TABLE IF NOT EXISTS page_revisions (
    id               UUID        NOT NULL DEFAULT gen_random_uuid(),
    organization_id  UUID        NOT NULL,
    page_id          UUID        NOT NULL,
    version          INTEGER     NOT NULL,
    content_markdown TEXT        NOT NULL,
    edited_by        UUID        NOT NULL,
    edited_at        TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, organization_id),
    UNIQUE (page_id, organization_id, version),
    FOREIGN KEY (page_id, organization_id) REFERENCES pages (id, organization_id) ON DELETE CASCADE
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS page_revisions_p%1$s PARTITION OF page_revisions FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;
