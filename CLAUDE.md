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
- **Database:** Postgres — raw SQL only, no ORM. Migrations live in `migrations/*.sql`, embedded at compile time (`include_dir!`) and applied on every startup in filename order (`src/db.rs`) — **every migration file must be safe to re-run** (`CREATE TABLE IF NOT EXISTS`, `ADD COLUMN IF NOT EXISTS`, etc.). This matches `ullav-dam-server`'s convention, not `ullav-user-management`'s separate `migrate`-sidecar + `schema_migrations`-tracking-table convention.
- **Search:** OpenSearch (secondary, rebuildable index — Postgres remains the source of truth); not wired up yet, lands with the outbox worker (`tack-indexer`, not yet built — see the architecture plan's implementation sequencing)
- **Auth:** `ullav-mcp-auth`'s shared JWT `TokenValidator`, issued by `ullav-user-management` — same pattern as every other first-party Ullav service. `src/auth.rs`'s `TackUser` extractor (an axum `FromRequestParts` impl, mirroring `ullav-dam-server/src/auth.rs`'s `AuthUser`) decodes the caller's JWT and enforces the team-granted `tack` product gate (admin bypasses), exposing `user_id`, `is_admin`, and only the caller's **Tack-enabled** teams (with their `organization_id`, from the Organizations migration in `ullav-user-management`).
- **Dev port:** 8087 (reserved; matches `tack` frontend's `API_URL` default)

## Current State (Phase 1, server scaffold)

Health check (`GET /health`) and a whoami endpoint (`GET /me`, gated by `TackUser`) are implemented and verified end-to-end against a real `ullav-user-management` instance — this is the proof that JWT validation and the `tack` product/organization claim decoding work correctly before any real content endpoints are built. No Notes/Pages schema exists yet (`migrations/000_extensions.sql` only enables the `pgcrypto`/`ltree` extensions those tables will need). No search, no MCP surface, no `tack-indexer` worker binary yet — see the architecture plan's "Implementation sequencing" section for what's next.

## Branch Policy

Feature branches merge to `main` via PR; do not commit directly to `main`.
