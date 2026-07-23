# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**tack-server** is the backend for **Tack**, a standalone Notes & Pages content platform for the Ullav ecosystem — see `/Users/colin/github/CLAUDE.md` for full workspace context.

Tack provides two content types on one shared platform:
- **Notes** — threaded, entity-attached comments/dialogs (private / team / global visibility), markdown as the canonical source of truth.
- **Pages** — Confluence/Notion-class hierarchical long-form documents, organized into spaces, with live (never denormalized) cross-references into other Ullav apps. Canonical storage is a Yjs CRDT document, enabling real-time collaborative editing; markdown/plaintext is generated on demand for search, API responses, and export.

This is currently **Phase 1**: build the server and its dedicated UI (`tack`) standalone, proven out in isolation. No existing Ullav app/service is being modified or migrated onto Tack yet — that is a separate, later phase.

## Tech Stack

- **Language:** Rust
- **Database:** Postgres — raw SQL only, no ORM (matches `ullav-dam-server`/`awe-server` convention: plain `.sql` migration files run as an idempotent migration job, `tokio-postgres`-style access with explicit type casts on parameterized queries)
- **Search:** OpenSearch (secondary, rebuildable index — Postgres remains the source of truth)
- **Auth:** `ullav-mcp-auth`'s shared JWT `TokenValidator`, issued by `ullav-user-management` — same pattern as every other first-party Ullav service

## Branch Policy

Feature branches merge to `main` via PR; do not commit directly to `main`.
