# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**tack-server** is the backend for **Tack**, a standalone Notes & Pages content platform for the Ullav ecosystem — see `/Users/colin/github/CLAUDE.md` for full workspace context.

Tack provides two content types on one shared platform:
- **Notes** — threaded, entity-attached comments/dialogs (private / team / global visibility), markdown as the canonical source of truth.
- **Pages** — Confluence/Notion-class hierarchical long-form documents, organized into spaces, with live (never denormalized) cross-references into other Ullav apps. Canonical storage is a Yjs CRDT document, enabling real-time collaborative editing; markdown/plaintext is generated on demand for search, API responses, and export.

This is currently **Phase 1**: build the server and its dedicated UI (`tack`) standalone, proven out in isolation. No existing Ullav app/service is being modified or migrated onto Tack yet — that is a separate, later phase.

## Tech Stack

- **Language:** Rust, `axum` 0.7 (matches `ullav-dam-server`'s stack exactly: `tokio-postgres`+`deadpool-postgres`, `utoipa`+Swagger UI at `/docs`, `ullav-mcp-auth` for JWT validation)
- **Database:** Postgres — raw SQL only, no ORM. Migrations live in `migrations/*.sql`, embedded at compile time (`include_dir!`) and applied on every startup in filename order (`src/db/mod.rs`; query modules live alongside it, e.g. `src/db/notes.rs`) — **every migration file must be safe to re-run** (`CREATE TABLE IF NOT EXISTS`, `ADD COLUMN IF NOT EXISTS`, etc.). This matches `ullav-dam-server`'s convention, not `ullav-user-management`'s separate `migrate`-sidecar + `schema_migrations`-tracking-table convention.
  - **Local-dev gotcha:** `include_dir!` embeds `migrations/` at compile time, but Cargo doesn't always notice a `.sql`-only change as a reason to recompile `db.rs` (observed directly while building this schema — a `cargo build` after adding new migration files finished suspiciously fast and silently ran against a stale, empty embed). If you edit *only* files under `migrations/` and the app doesn't seem to pick them up, `touch src/db.rs` (or `cargo clean -p tack-server`) before rebuilding.
  - **Tenant partitioning:** every tenant-scoped content table (`notes`, `note_bodies`, `note_revisions`, `content_attachments`, `content_references`, `spaces`) is `PARTITION BY HASH (organization_id)` into 32 fixed buckets, from the very first migration — Postgres requires the partition key in every primary key, and in every foreign key referencing a partitioned table, so every one of these tables carries `organization_id` even where it isn't otherwise needed, and FKs are always composite `(x_id, organization_id)`. Verified empirically (composite FKs, self-referential FK on `notes.parent_id`, cascading deletes, `ltree` ancestor queries, idempotent re-application) before writing the migration files — see `migrations/001_notes.sql`'s header comment. `content_attachments`/`content_references` deliberately have **no FK** to notes/pages (content is polymorphic — note or page — and can also reference entities in other services' databases entirely); that's an application-layer integrity concern, same as awe-server's own notes table.
  - `outbox_events` is a plain (non-partitioned) table for now — it's a queue, not a data store, and time-range partitioning can be added later without the one-way-door risk the content tables' hash-partitioning carries.
- **Search:** OpenSearch (secondary, rebuildable index — Postgres remains the source of truth); not wired up yet, lands with the outbox worker (`tack-indexer`, not yet built — see the architecture plan's implementation sequencing)
- **Auth:** `ullav-mcp-auth`'s shared JWT `TokenValidator`, issued by `ullav-user-management` — same pattern as every other first-party Ullav service. `src/auth.rs`'s `TackUser` extractor (an axum `FromRequestParts` impl, mirroring `ullav-dam-server/src/auth.rs`'s `AuthUser`) decodes the caller's JWT and enforces the team-granted `tack` product gate (admin bypasses), exposing `user_id`, `is_admin`, and only the caller's **Tack-enabled** teams (with their `organization_id`, from the Organizations migration in `ullav-user-management`).
- **Dev port:** 8087 (reserved; matches `tack` frontend's `API_URL` default)

## Current State (Phase 1, server scaffold + Notes vertical slice)

Health check (`GET /health`) and a whoami endpoint (`GET /me`, gated by `TackUser`) are implemented and verified end-to-end against a real `ullav-user-management` instance. The Notes schema exists (see below). **Notes CRUD is implemented and working**: `src/handlers/notes.rs` + `src/db/notes.rs`.

- `POST /notes`, `GET /notes?team_id=`, `GET /notes/:id`, `PATCH /notes/:id`, `DELETE /notes/:id` (soft delete), `POST`/`GET /notes/:id/replies`, `GET /notes/:id/revisions`.
- Creating a note requires `team_id` — that's how `organization_id` (the shard key) is resolved (`TackUser.teams[team_id].organization_id`); a team with no organization assigned yet gets a clean 400, not a confusing failure.
- Visibility (`private`/`team`/`organization`) is enforced by `can_view`/`can_edit` in `handlers/notes.rs`, resolved **live** from the caller's current JWT team/org claims on every request — never cached or denormalized. Exhaustively unit-tested (the full private/team/organization × creator/team-member/org-member/stranger/admin matrix, 9 cases).
- Every write (create, reply, edit, delete) enqueues an `outbox_events` row in the same transaction — verified end-to-end that the right event sequence lands (`created`/`created`/`updated`/`deleted`) even though nothing consumes the queue yet (`tack-indexer` doesn't exist).
- A note's org isn't in the URL, so `GET`/`PATCH`/`DELETE /notes/:id` resolve it by trying each of the caller's organizations (`resolve_visible_note` in `handlers/notes.rs`) — fine at today's scale (a handful of orgs per user); revisit only if this becomes a real hot path.

No search, no MCP surface, no `tack-indexer` worker binary, no Pages yet — see the architecture plan's "Implementation sequencing" section for what's next.

### Notes schema

`migrations/001_notes.sql` (`spaces`/`notes`/`note_bodies`/`note_revisions`), `002_content_links.sql` (`content_attachments`/`content_references`), `003_outbox.sql` (`outbox_events`) — verified (fresh apply, idempotent restart, cascades, `ltree` queries — all against a real Postgres instance via the actual compiled binary, not just raw SQL).

## Branch Policy

Feature branches merge to `main` via PR; do not commit directly to `main`.
