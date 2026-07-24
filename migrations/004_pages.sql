-- Pages: hierarchical documents organized into spaces. Unlike Notes'
-- visibility enum, page access is resolved live from the page's position in
-- the space/page tree (explicit per-page overrides in `page_permissions`,
-- falling back to space membership) -- never denormalized or copied onto
-- children. That live resolution is what structurally rules out Confluence's
-- documented bug, where restrictions are copied once at creation time and
-- silently go stale when a parent's permissions later change. See
-- src/pages_acl.rs for the resolution logic.
--
-- Canonical content storage is a Yjs CRDT document (page_docs.yjs_doc_state)
-- once the Hocuspocus real-time sync server exists (a later step). Until
-- then, `content_markdown` is the write path: pages are created/edited
-- through this REST API only (single-writer, no live co-editing yet).
-- `yjs_doc_state` stays NULL until Hocuspocus is introduced, at which point
-- it becomes the source of truth and `content_markdown` becomes a derived
-- projection of it -- kept as a column now specifically so that transition
-- is additive, not a schema break.

CREATE TABLE IF NOT EXISTS pages (
    id              UUID        NOT NULL DEFAULT gen_random_uuid(),
    organization_id UUID        NOT NULL,
    space_id        UUID        NOT NULL,
    parent_id       UUID,
    -- Materialized path (e.g. 'p1' for a root page, 'p1.p2' for a child) --
    -- same technique as notes.thread_path: a single indexed ancestor-path
    -- query resolves "nearest ancestor with an explicit permission override"
    -- without an N-hop parent_id walk.
    path            LTREE       NOT NULL,
    title           TEXT        NOT NULL,
    is_template     BOOLEAN     NOT NULL DEFAULT FALSE,
    created_by      UUID        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleted_at      TIMESTAMPTZ,
    PRIMARY KEY (id, organization_id),
    FOREIGN KEY (space_id, organization_id) REFERENCES spaces (id, organization_id) ON DELETE CASCADE,
    FOREIGN KEY (parent_id, organization_id) REFERENCES pages (id, organization_id) ON DELETE CASCADE
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS pages_p%1$s PARTITION OF pages FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS pages_path_gist_idx ON pages USING GIST (path);
CREATE INDEX IF NOT EXISTS pages_space_id_idx ON pages (space_id);
CREATE INDEX IF NOT EXISTS pages_parent_id_idx ON pages (parent_id);

-- Large content lives in its own narrow table, split out from page
-- metadata/ACL (`pages`) -- same storage-performance decision as
-- note_bodies: listing/permission-check queries never touch TOAST'd content
-- for rows nobody asked to render.
CREATE TABLE IF NOT EXISTS page_docs (
    page_id          UUID        NOT NULL,
    organization_id  UUID        NOT NULL,
    content_markdown TEXT        NOT NULL DEFAULT '',
    yjs_doc_state    BYTEA,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (page_id, organization_id),
    FOREIGN KEY (page_id, organization_id) REFERENCES pages (id, organization_id) ON DELETE CASCADE
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS page_docs_p%1$s PARTITION OF page_docs FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;

-- Explicit permission overrides. A page with zero rows here inherits its
-- effective permission from the nearest ancestor (walking up `path`) that
-- has any rows, or falls back to space membership if no ancestor in the
-- chain has any rows at all. Deliberately NOT copied onto children at
-- creation time or when changed -- resolved live at read time on every
-- request (see src/pages_acl.rs).
--
-- Two levels only (view/edit), matching Confluence's actual page-restriction
-- model, not a made-up finer-grained scheme -- a 'comment' tier is deferred,
-- not designed away, if it ever turns out to be needed.
--
-- A page's own permission rows (once it has any) are used as an exhaustive
-- whitelist, not merged with an ancestor's -- there's no partial/additive
-- composition in this first pass (e.g. "inherit the parent's grants AND add
-- one more principal"). This matches how Confluence restrictions actually
-- behave: once a page is restricted, only the listed principals can access
-- it, full stop.
CREATE TABLE IF NOT EXISTS page_permissions (
    id              UUID        NOT NULL DEFAULT gen_random_uuid(),
    organization_id UUID        NOT NULL,
    page_id         UUID        NOT NULL,
    principal_type  TEXT        NOT NULL CHECK (principal_type IN ('team', 'user', 'organization')),
    principal_id    UUID,
    level           TEXT        NOT NULL CHECK (level IN ('view', 'edit')),
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, organization_id),
    CHECK (
        (principal_type = 'organization' AND principal_id IS NULL) OR
        (principal_type IN ('team', 'user') AND principal_id IS NOT NULL)
    ),
    FOREIGN KEY (page_id, organization_id) REFERENCES pages (id, organization_id) ON DELETE CASCADE
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS page_permissions_p%1$s PARTITION OF page_permissions FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS page_permissions_page_org_idx ON page_permissions (page_id, organization_id);
