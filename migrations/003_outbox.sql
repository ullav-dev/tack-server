-- Transactional outbox: the Postgres -> OpenSearch indexing pipeline.
-- A content write inserts its outbox_events row in the same transaction, so
-- the event exists iff the write committed. The (not-yet-built) tack-indexer
-- worker claims unprocessed rows (SELECT ... FOR UPDATE SKIP LOCKED, safe
-- even with >1 worker), indexes them, and marks processed_at. Delivery is
-- at-least-once, which is fine because indexing is a pure idempotent upsert
-- keyed by content_id.
--
-- Deliberately a plain (non-partitioned) table for now: unlike the content
-- tables above, this isn't a one-way-door decision — it's a queue, not a
-- data store, processed rows get pruned on a schedule, and time-range
-- partitioning (e.g. monthly) can be added later once real volume justifies
-- the operational complexity, without any FK/PK entanglement to unwind.
CREATE TABLE IF NOT EXISTS outbox_events (
    id              UUID        NOT NULL DEFAULT gen_random_uuid() PRIMARY KEY,
    organization_id UUID        NOT NULL,
    content_type    TEXT        NOT NULL CHECK (content_type IN ('note', 'page')),
    content_id      UUID        NOT NULL,
    event_type      TEXT        NOT NULL,
    payload         JSONB       NOT NULL DEFAULT '{}'::jsonb,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    processed_at    TIMESTAMPTZ
);

-- Partial index: only unprocessed rows are ever scanned by the worker.
CREATE INDEX IF NOT EXISTS outbox_events_unprocessed_idx
    ON outbox_events (created_at) WHERE processed_at IS NULL;
