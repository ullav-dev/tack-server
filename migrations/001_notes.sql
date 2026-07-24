-- Notes: threaded, entity-attached comments/dialogs. Markdown is the
-- canonical source of truth (see architecture plan for why, vs. Pages'
-- Yjs-CRDT canonical storage).
--
-- Every tenant-scoped table here is hash-partitioned by organization_id from
-- day one (32 fixed buckets) — this is the one schema decision that's cheap
-- now and genuinely painful to retrofit once real data exists (Postgres
-- partitioning requires the partition key in every primary key, and every
-- foreign key referencing a partitioned table must include it too). Verified
-- empirically against a real Postgres instance before writing this file:
-- composite FKs across partitioned tables, self-referential FKs, cascading
-- deletes, ltree ancestor queries, and idempotent re-application all work
-- as expected.
--
-- Large content lives in its own narrow table (note_bodies), split out from
-- note metadata/ACL (notes) — the single highest-leverage read-performance
-- decision from the storage-architecture research: listing/permission-check
-- queries never touch TOAST'd body text for rows nobody asked to render.

CREATE TABLE IF NOT EXISTS spaces (
    id              UUID        NOT NULL DEFAULT gen_random_uuid(),
    organization_id UUID        NOT NULL,
    owning_service  TEXT        NOT NULL,
    team_id         UUID,
    name            TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, organization_id)
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS spaces_p%1$s PARTITION OF spaces FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;

CREATE TABLE IF NOT EXISTS notes (
    id              UUID        NOT NULL DEFAULT gen_random_uuid(),
    organization_id UUID        NOT NULL,
    -- Scopes 'team' visibility; NULL is fine for private/organization-visibility notes.
    team_id         UUID,
    -- Materialized path for the note's thread (e.g. 'n1' for a top-level note,
    -- 'n1.n2' for a reply) — a single indexed ancestor-path query resolves a
    -- whole thread, not an N-hop parent_id walk.
    thread_path     LTREE       NOT NULL,
    parent_id       UUID,
    visibility      TEXT        NOT NULL DEFAULT 'private'
                                CHECK (visibility IN ('private', 'team', 'organization')),
    created_by      UUID        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at      TIMESTAMPTZ,
    PRIMARY KEY (id, organization_id),
    FOREIGN KEY (parent_id, organization_id) REFERENCES notes (id, organization_id) ON DELETE CASCADE
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS notes_p%1$s PARTITION OF notes FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS notes_thread_path_gist_idx ON notes USING GIST (thread_path);
CREATE INDEX IF NOT EXISTS notes_team_id_idx ON notes (team_id);

CREATE TABLE IF NOT EXISTS note_bodies (
    note_id         UUID NOT NULL,
    organization_id UUID NOT NULL,
    body_markdown   TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (note_id, organization_id),
    FOREIGN KEY (note_id, organization_id) REFERENCES notes (id, organization_id) ON DELETE CASCADE
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS note_bodies_p%1$s PARTITION OF note_bodies FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;

-- Append-only revision history, populated from day one even before any
-- "show history" UI exists — retrofitting history onto a table that was
-- only ever storing "current text" is impossible once old versions are gone.
-- Retention/pruning policy is deferred (not user-facing yet), but the table
-- itself must exist now.
CREATE TABLE IF NOT EXISTS note_revisions (
    id              UUID        NOT NULL DEFAULT gen_random_uuid(),
    organization_id UUID        NOT NULL,
    note_id         UUID        NOT NULL,
    version         INTEGER     NOT NULL,
    body_markdown   TEXT        NOT NULL,
    edited_by       UUID        NOT NULL,
    edited_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, organization_id),
    UNIQUE (note_id, organization_id, version),
    FOREIGN KEY (note_id, organization_id) REFERENCES notes (id, organization_id) ON DELETE CASCADE
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS note_revisions_p%1$s PARTITION OF note_revisions FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;
