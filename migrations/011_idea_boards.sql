-- Idea Boards: a canvas whiteboard built on top of Notes/note_folders,
-- mirroring awe-server's proven shape (034_idea_boards.sql,
-- 036_sticky_workflow_link.sql, 040_board_shapes.sql, 050_board_shape_image.sql,
-- 035_note_link_ports.sql) but adapted to this schema's own conventions --
-- see the deviations called out inline below. Schema only in this pass: the
-- CRUD API for stickies/shapes/links is deliberately deferred to when the
-- canvas UI is actually being built (Phase 5 of the AWE-apps Notes
-- migration), so the API shape is designed against a real consumer instead
-- of guessed at now.
--
-- A "board" is just a note_folders row with folder_type = 'ideas_board' --
-- same "layout state lives separately from note content" split awe-server
-- proved out (idea_board_stickies holds x/y/color, not the notes table
-- itself), so notes stay pure content.

-- ── note_folders: add the type discriminator ───────────────────────────────
--
-- Deliberately NOT adding awe-server's entity_type/entity_id columns here --
-- this schema already has a generic mechanism for "this content is attached
-- to an entity owned by another service" (content_attachments, from
-- 002_content_links.sql), so a board attaches to e.g. a togra project the
-- exact same way a note attaches to a lagan pull request, via
-- content_attachments with content_type = 'folder', rather than duplicating
-- entity-scoping columns onto note_folders a second time.
ALTER TABLE note_folders ADD COLUMN IF NOT EXISTS folder_type TEXT NOT NULL DEFAULT 'general';

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint WHERE conname = 'note_folders_folder_type_check'
    ) THEN
        ALTER TABLE note_folders ADD CONSTRAINT note_folders_folder_type_check
            CHECK (folder_type IN ('general', 'ideas_board'));
    END IF;
END $$;

-- content_attachments.content_type widens to 'folder' so a board (a
-- note_folders row) can be attached to an external entity the same way a
-- note or page can.
ALTER TABLE content_attachments DROP CONSTRAINT IF EXISTS content_attachments_content_type_check;
ALTER TABLE content_attachments ADD CONSTRAINT content_attachments_content_type_check
    CHECK (content_type IN ('note', 'page', 'folder'));

-- ── board_shapes: non-note diagram elements on a board ─────────────────────
--
-- organization_id + hash partitioning added (awe-server's table has neither --
-- it predates this schema's tenant-sharding convention); every other
-- deviation below follows from that: PRIMARY KEY and every FK become
-- composite (id/board_id, organization_id), matching 001_notes.sql's header.
-- created_by is UUID (a real user id), not awe-server's legacy TEXT
-- username -- consistent with every other created_by in this schema.
CREATE TABLE IF NOT EXISTS board_shapes (
    id              UUID             NOT NULL DEFAULT gen_random_uuid(),
    organization_id UUID             NOT NULL,
    board_id        UUID             NOT NULL,
    shape_type      TEXT             NOT NULL
                                      CHECK (shape_type IN ('rect', 'circle', 'diamond', 'database', 'cloud', 'actor', 'image')),
    x               DOUBLE PRECISION NOT NULL DEFAULT 200,
    y               DOUBLE PRECISION NOT NULL DEFAULT 200,
    width           DOUBLE PRECISION NOT NULL DEFAULT 160,
    height          DOUBLE PRECISION NOT NULL DEFAULT 100,
    fill_color      TEXT             NOT NULL DEFAULT '#ffffff',
    stroke_color    TEXT             NOT NULL DEFAULT '#64748b',
    stroke_width    DOUBLE PRECISION NOT NULL DEFAULT 2,
    label           TEXT,
    label_color     TEXT             NOT NULL DEFAULT '#1e293b',
    label_size      INTEGER          NOT NULL DEFAULT 13,
    -- Only meaningful for shape_type = 'image' -- a DAM asset URL, same
    -- "plain HTTPS image, no proxy" posture as TeamAvatar (see tack's own
    -- CLAUDE.md) until Tack grows a DAM proxy of its own.
    image_url       TEXT,
    created_by      UUID             NOT NULL,
    created_at      TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ      NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, organization_id),
    FOREIGN KEY (board_id, organization_id) REFERENCES note_folders (id, organization_id) ON DELETE CASCADE
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS board_shapes_p%1$s PARTITION OF board_shapes FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS board_shapes_board_idx ON board_shapes (board_id, organization_id);

-- ── idea_board_stickies: layout state for a note on a board ────────────────
CREATE TABLE IF NOT EXISTS idea_board_stickies (
    board_id        UUID             NOT NULL,
    note_id         UUID             NOT NULL,
    organization_id UUID             NOT NULL,
    x               DOUBLE PRECISION NOT NULL DEFAULT 40,
    y               DOUBLE PRECISION NOT NULL DEFAULT 40,
    color           TEXT             NOT NULL DEFAULT 'yellow'
                                      CHECK (color IN ('yellow', 'pink', 'blue', 'green', 'purple', 'orange')),
    width           DOUBLE PRECISION NOT NULL DEFAULT 220,
    height          DOUBLE PRECISION NOT NULL DEFAULT 160,
    -- Soft-link to an external entity (e.g. a togra backlog story) -- no FK,
    -- same "notes domain stays decoupled from any one app" reasoning as
    -- awe-server's 036_sticky_workflow_link.sql. A dangling reference (the
    -- story since deleted) is handled at the app layer, not the schema --
    -- consistent with content_attachments/content_references never having
    -- an FK into another service's data for the same reason.
    linked_entity_type TEXT,
    linked_entity_id   TEXT,
    PRIMARY KEY (board_id, note_id, organization_id),
    FOREIGN KEY (board_id, organization_id) REFERENCES note_folders (id, organization_id) ON DELETE CASCADE,
    FOREIGN KEY (note_id, organization_id) REFERENCES notes (id, organization_id) ON DELETE CASCADE
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS idea_board_stickies_p%1$s PARTITION OF idea_board_stickies FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;

-- ── note_links: directed graph edges between notes and/or shapes ───────────
--
-- board_id is denormalized onto every link (awe-server has no equivalent --
-- board membership there is only derivable indirectly, via whichever note/
-- shape each endpoint points at) so "list this board's links" is a single
-- indexed query instead of a join through both possible endpoint tables --
-- worth the redundancy given every other list in this schema is a direct,
-- partition-friendly lookup.
CREATE TABLE IF NOT EXISTS note_links (
    id              UUID        NOT NULL DEFAULT gen_random_uuid(),
    organization_id UUID        NOT NULL,
    board_id        UUID        NOT NULL,
    from_note_id    UUID,
    to_note_id      UUID,
    from_shape_id   UUID,
    to_shape_id     UUID,
    from_port       TEXT        CHECK (from_port IS NULL OR from_port IN ('top', 'right', 'bottom', 'left')),
    to_port         TEXT        CHECK (to_port IS NULL OR to_port IN ('top', 'right', 'bottom', 'left')),
    label           TEXT,
    created_by      UUID        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, organization_id),
    FOREIGN KEY (board_id, organization_id) REFERENCES note_folders (id, organization_id) ON DELETE CASCADE,
    FOREIGN KEY (from_note_id, organization_id) REFERENCES notes (id, organization_id) ON DELETE CASCADE,
    FOREIGN KEY (to_note_id, organization_id) REFERENCES notes (id, organization_id) ON DELETE CASCADE,
    FOREIGN KEY (from_shape_id, organization_id) REFERENCES board_shapes (id, organization_id) ON DELETE CASCADE,
    FOREIGN KEY (to_shape_id, organization_id) REFERENCES board_shapes (id, organization_id) ON DELETE CASCADE,
    -- Exactly one of (from_note_id, from_shape_id), same for to_ -- a
    -- composite FK with any NULL member simply isn't enforced (Postgres'
    -- default MATCH SIMPLE), so leaving the other endpoint's columns NULL
    -- is exactly what lets these two nullable FK pairs coexist correctly.
    CHECK ((from_note_id IS NOT NULL) <> (from_shape_id IS NOT NULL)),
    CHECK ((to_note_id IS NOT NULL) <> (to_shape_id IS NOT NULL)),
    UNIQUE (from_note_id, to_note_id, organization_id)
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS note_links_p%1$s PARTITION OF note_links FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS note_links_board_idx ON note_links (board_id, organization_id);
