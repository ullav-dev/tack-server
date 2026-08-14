# tack-server

The backend for **Tack** — a standalone Notes & Pages content platform for the Ullav ecosystem.

Tack provides two content types on one shared platform:

- **Notes** — threaded, entity-attached comments/dialogs (private / team / organization-wide visibility). Markdown is the canonical source of truth, with append-only version history and reply-to-version scoping.
- **Pages** — Confluence/Notion-class hierarchical long-form documents organized into spaces, with real-time collaborative editing (via the companion [`tack-hocuspocus`](https://github.com/ullav-dev/tack-hocuspocus) service), named-snapshot version history, and live page-to-page cross-references.

Both content types share one Postgres-backed storage layer, one hybrid (lexical + semantic) OpenSearch index, one live-resolved ACL model, and one MCP tool surface.

## Status

**Phase 1 (standalone server + UI) is complete.** Notes, Pages, DAM asset embedding, export, versioning, page-to-page cross-references, and search indexing are all implemented and live-verified. **Phase 2 (migrating existing apps' Notes onto this platform) is underway** — [`lagan`](https://github.com/ullav-dev/lagan-server)'s PR discussion notes were the first app migrated (see below). No other existing Ullav app has been touched yet.

## Architecture

![Tack Platform — Backend Architecture](docs/architecture.svg)

*(Source: [`docs/architecture.svg`](docs/architecture.svg) — a single self-contained SVG, safe to drop directly into a slide deck or export to PNG; regenerate/edit by hand, no build step or diagramming tool required.)*

- **Postgres is the only source of truth.** OpenSearch is a secondary, fully rebuildable index, fed by a transactional outbox (`outbox_events`) rather than dual-written from the API layer.
- **Tenant sharding**: every content table is hash-partitioned by `organization_id` (32 fixed buckets) from the first migration — Organizations are a new, additive tenant concept living in `ullav-user-management`, one level above Team.
- **Two binaries, one crate**: `tack-server` (the API) and `tack-indexer` (the outbox-draining OpenSearch worker) share this crate's lib code but run and scale as separate processes/containers.
- **Auth** is `ullav-mcp-auth`'s shared JWT validator, same as every other first-party Ullav service — a team must have the `tack` product slug enabled for its members to use Tack (admin accounts bypass this gate).

## Tech stack

- **Language**: Rust, [axum](https://github.com/tokio-rs/axum) 0.7
- **Database**: Postgres, raw SQL only (no ORM) — plain `.sql` migrations in `migrations/`, embedded at compile time and applied idempotently on every startup
- **Search**: [OpenSearch](https://opensearch.org/), hybrid BM25 + kNN via Reciprocal Rank Fusion
- **Embeddings**: self-hosted local inference via [`fastembed`](https://github.com/Anush008/fastembed-rs) (`multilingual-e5-small`, 384-dim, ONNX Runtime) — no external embedding API
- **API docs**: [utoipa](https://github.com/juhaku/utoipa) + Swagger UI at `/docs`
- **MCP**: [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk)-based Streamable HTTP server at `/mcp`

## Getting started

### Prerequisites

- Rust (stable toolchain)
- Postgres (with the `pgcrypto` and `ltree` extensions available)
- OpenSearch (single-node is fine for local dev)
- A running [`ullav-user-management`](https://github.com/ullav-dev/ullav-user-management) instance (for JWT issuance/validation)

### Configuration

Copy `.env.example` to `.env` and adjust as needed:

```bash
HOST=0.0.0.0
PORT=8087
DATABASE_URL=postgresql://tack:tack@localhost:5432/tack
OAUTH2_JWKS_URL=http://localhost:8081/oauth2/jwks   # ullav-user-management
OAUTH2_ISSUER=http://localhost:8081                 # ullav-user-management
OPENSEARCH_URL=http://localhost:9200
RUST_LOG=info

# Not in .env.example yet, but read by src/config.rs — set explicitly if
# your local setup needs a non-default value:
# TACK_MCP_CANONICAL_URI=http://localhost:8087/mcp
# EMBEDDING_MODEL_CACHE_DIR=./.embedding-models
```

### Running locally

```bash
cargo run --bin tack-server    # the API server, port 8087
cargo run --bin tack-indexer   # the OpenSearch outbox worker (separate process)
```

Migrations run automatically at startup (idempotent — safe to restart against an existing database). Both the embedding model and OpenSearch degrade gracefully if unavailable at startup: the server logs a warning and continues in a reduced mode (lexical-only search, no MCP semantic ranking) rather than failing to boot.

### Tests

```bash
cargo test
```

### Docker

Two images: `Dockerfile` (the API server) and `Dockerfile.indexer` (the outbox worker) — both deployed as separate containers/processes so indexing load never competes with request-serving resources.

## API surface

Full interactive docs are served at `GET /docs` (Swagger UI) once the server is running. Highlights:

| Area | Endpoints |
|---|---|
| Health/identity | `GET /health`, `GET /me` |
| Notes | `POST/GET /notes`, `GET/PATCH/DELETE /notes/:id`, `POST/GET /notes/:id/replies`, `GET /notes/:id/revisions`, `POST/DELETE /notes/:id/revisions/:id`, `GET /notes/by-entity` (notes attached to an external entity, e.g. a lagan PR) |
| Spaces & Pages | `POST/GET /spaces`, `GET /spaces/:id/pages`, `POST /pages`, `GET/PATCH/DELETE /pages/:id`, `GET /pages/search`, `GET /pages/:id/permission`, `GET/POST /pages/:id/permissions`, `DELETE /pages/:id/permissions/:id` |
| Page versioning | `POST/GET /pages/:id/revisions`, `DELETE /pages/:id/revisions/:id` |
| Page cross-references | `POST/GET /pages/:id/references`, `DELETE /pages/:id/references/:id`, `GET /pages/:id/backlinks` |
| Search | `GET /search?q=` — hybrid, ACL-filtered |
| MCP | `POST /mcp` (Streamable HTTP, gated by the `tack:tools` scope): `search_content`, `get_note_thread`, `create_note`, `reply_to_note` |

## Key design decisions

- **Live-resolved ACL, never denormalized.** Both Notes' visibility enum and Pages' ancestor/space-tree permission model are resolved fresh on every request — this structurally rules out the class of bug where Confluence-style restrictions are copied once and silently go stale.
- **`content_attachments` and `content_references`** are generic, polymorphic join tables (namespaced by `owning_service`) designed to subsume every per-app attachment/reference pattern seen across the Ullav ecosystem (awe-server's single entity, clann's multi-tree, Cartlann's multi-object) without a lossy conversion to any one of them.
- **Explicit-only versioning**, for both Notes and Pages: a version is a deliberate snapshot a user takes, never an automatic side effect of every save.
- **Storage-layer split**: large content (`note_bodies`, `page_docs`) lives in its own narrow table, separate from metadata/ACL — the single highest-leverage decision for keeping listing/permission-check queries fast at scale.

## Related repos

- [`tack`](https://github.com/ullav-dev/tack) — the Next.js frontend
- [`tack-hocuspocus`](https://github.com/ullav-dev/tack-hocuspocus) — real-time Yjs sync server for collaborative Page editing
- [`ullav-user-management`](https://github.com/ullav-dev/ullav-user-management) — auth/identity, including the Organizations tenant model Tack's sharding relies on

## Branch policy

Feature branches merge to `main` via PR — never commit directly to `main`.
