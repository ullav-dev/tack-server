-- Extensions needed by later migrations.
-- pgcrypto: gen_random_uuid() for primary keys.
-- ltree: materialized-path columns (note thread_path, page path) — see the
-- architecture plan for why ltree over adjacency-list-only threading.
CREATE EXTENSION IF NOT EXISTS "pgcrypto";
CREATE EXTENSION IF NOT EXISTS "ltree";
