-- Cross-cutting linking primitives shared by both content types (Notes now,
-- Pages later). Deliberately no DB-level FK from either table to notes/pages:
-- content_id is polymorphic (a 'note' or a 'page'), so a single FK target
-- isn't possible — and content_attachments/content_references also need to
-- reference entities owned by *other services* (Togra workflows, Cunav
-- tickets, ...) which live in different databases entirely. Referential
-- integrity here is an application-layer concern, same precedent as
-- awe-server's own notes table (no DB-level FK, by design, for exactly this
-- portability reason).

-- One join table subsumes three different attachment shapes seen across the
-- existing per-app notes implementations (awe-server's single entity_type+
-- entity_id, clann-server's multi-attach-by-name, Cartlann's multi-attach-by-id
-- join table) without a lossy conversion to any one of them.
CREATE TABLE IF NOT EXISTS content_attachments (
    id              UUID        NOT NULL DEFAULT gen_random_uuid(),
    organization_id UUID        NOT NULL,
    content_type    TEXT        NOT NULL CHECK (content_type IN ('note', 'page')),
    content_id      UUID        NOT NULL,
    -- Namespaced by owning_service to avoid collisions between e.g. two apps
    -- both having an entity_type of "ticket".
    owning_service  TEXT        NOT NULL,
    entity_type     TEXT        NOT NULL,
    entity_id       TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, organization_id)
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS content_attachments_p%1$s PARTITION OF content_attachments FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;

-- "Get attachments for this note/page" (rendering a content item's entity chips).
CREATE INDEX IF NOT EXISTS content_attachments_content_idx
    ON content_attachments (content_type, content_id, organization_id);
-- "Get notes/pages attached to this entity" (e.g. a Togra workflow's notes panel).
CREATE INDEX IF NOT EXISTS content_attachments_entity_idx
    ON content_attachments (owning_service, entity_type, entity_id, organization_id);

-- Typed cross-references (backlinks, live embeds — mentions of another
-- entity or content item inside a Note/Page body). Populated at save/index
-- time, not query time, so this one table serves both the backlinks UI and
-- the AI/search reference graph. Resolution is always live at render time
-- (never a denormalized snapshot) — the structural fix for the Jira/
-- Confluence stale-copy problem.
CREATE TABLE IF NOT EXISTS content_references (
    id                  UUID        NOT NULL DEFAULT gen_random_uuid(),
    organization_id     UUID        NOT NULL,
    source_content_type TEXT        NOT NULL CHECK (source_content_type IN ('note', 'page')),
    source_content_id   UUID        NOT NULL,
    owning_service      TEXT        NOT NULL,
    entity_type         TEXT        NOT NULL,
    entity_id           TEXT        NOT NULL,
    -- e.g. 'togra_workflow' | 'cunav_ticket' | 'page' — what entity_id resolved to.
    resolved_kind       TEXT        NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (id, organization_id)
) PARTITION BY HASH (organization_id);

DO $$
BEGIN
    FOR i IN 0..31 LOOP
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS content_references_p%1$s PARTITION OF content_references FOR VALUES WITH (modulus 32, remainder %1$s)',
            i
        );
    END LOOP;
END $$;

CREATE INDEX IF NOT EXISTS content_references_source_idx
    ON content_references (source_content_type, source_content_id, organization_id);
