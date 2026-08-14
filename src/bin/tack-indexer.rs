//! tack-indexer: drains `outbox_events` and keeps the OpenSearch content
//! index in sync with Postgres (the source of truth). Runs as its own
//! process/container so indexing load never competes with tack-server's
//! API-request-serving resources — see the architecture plan.
//!
//! Claim strategy: `SELECT ... FOR UPDATE SKIP LOCKED` inside an explicit
//! transaction that stays open across the OpenSearch calls for the claimed
//! batch, committing (marking `processed_at`) only once the batch is done —
//! this holds Postgres row locks for the duration of a few HTTP calls, an
//! acceptable trade-off at today's single-worker, modest-volume scale (and
//! is what makes this safe to later run >1 worker without double-processing).
//! A crash mid-batch means the transaction never commits, so nothing is lost
//! and the same rows get reclaimed on restart — at-least-once delivery,
//! which is fine because indexing is a pure idempotent upsert keyed by
//! content id.
//!
//! Wake-up is a fixed short poll interval, not `LISTEN`/`NOTIFY` — a real
//! latency optimization for later, not a correctness gap now (a 2s poll
//! already gives near-real-time indexing for this stage).
//!
//! `--backfill`: a separate, rerunnable one-shot mode (not a new binary --
//! reuses this one's config/pool/embedder setup) that walks every
//! non-deleted note and page and re-indexes it, instead of only reacting to
//! future outbox events. Exists because adding a new indexed field (e.g.
//! `title`, `folder_id`, `space_id`, `parent_id` -- see `search.rs`) never
//! retroactively populates *already-indexed* documents; without this, that
//! content just never gets those fields until it happens to be edited
//! again. Safe to run any time and as many times as needed: indexing is
//! already an idempotent upsert keyed by content id, this mode doesn't
//! touch `outbox_events` at all, and it runs fully independently of (and
//! can overlap with) the normal poll loop.

use anyhow::{Context, Result};
use std::time::Duration;
use tack_server::{config::Config, db, embeddings::Embedder, search::SearchClient};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use uuid::Uuid;

const BATCH_SIZE: i64 = 20;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const BACKFILL_BATCH_SIZE: i64 = 100;

struct OutboxEvent {
    id: Uuid,
    organization_id: Uuid,
    content_type: String,
    content_id: Uuid,
    event_type: String,
}

/// Not `#[tokio::main]` -- see `Config::tokio_worker_threads`'s own doc
/// comment (`src/config.rs`) and `tack-server`'s own `main.rs`, which has
/// the identical restructuring for the identical reason.
fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let _ = dotenvy::dotenv();
    let cfg = Config::from_env()?;

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.tokio_worker_threads)
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?
        .block_on(run(cfg))
}

async fn run(cfg: Config) -> Result<()> {
    // Own pool, separate from tack-server's — a backfill/indexing burst here
    // must never starve the API server's request-serving connections.
    let pool = db::create_pool(&cfg.database_url, cfg.db_pool_max_size)?;
    let search = SearchClient::new(cfg.opensearch_url);
    search.ensure_index().await?;

    // Same non-fatal-degrade posture as tack-server: if the model can't
    // load, keep indexing lexically rather than refusing to start -- a
    // model that comes back later (e.g. cache volume mounted on redeploy)
    // just needs a re-index, not a code change.
    let embedder = {
        let cache_dir = cfg.embedding_model_cache_dir.clone();
        let intra_threads = cfg.embedding_intra_threads;
        match tokio::task::spawn_blocking(move || Embedder::new(cache_dir, intra_threads)).await {
            Ok(Ok(embedder)) => Some(embedder),
            Ok(Err(e)) => {
                tracing::warn!("embedding model failed to load, indexing lexical-only: {e:#}");
                None
            }
            Err(e) => {
                tracing::warn!("embedding model load task panicked, indexing lexical-only: {e:#}");
                None
            }
        }
    };

    if std::env::args().any(|a| a == "--backfill") {
        return run_backfill(&pool, &search, embedder.as_ref()).await;
    }

    tracing::info!("tack-indexer started, polling every {POLL_INTERVAL:?}");

    loop {
        match process_batch(&pool, &search, embedder.as_ref()).await {
            Ok(0) => tokio::time::sleep(POLL_INTERVAL).await,
            Ok(n) => tracing::info!("processed {n} outbox event(s)"),
            Err(e) => {
                tracing::error!("batch processing error: {e:#}");
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}

/// Walks every non-deleted note, then every non-deleted page, re-indexing
/// each -- see this file's module doc comment for why this exists and why
/// it's safe to rerun. Logs progress every batch and a final summary;
/// exits non-zero (via the error return) if any batch's *fetch* fails, but
/// a single document's indexing failure is logged and skipped rather than
/// aborting the whole run, so one bad row can't block backfilling
/// everything after it.
async fn run_backfill(pool: &db::DbPool, search: &SearchClient, embedder: Option<&Embedder>) -> Result<()> {
    let notes_indexed = backfill_notes(pool, search, embedder).await?;
    let pages_indexed = backfill_pages(pool, search, embedder).await?;
    tracing::info!("backfill complete: {notes_indexed} note(s), {pages_indexed} page(s) re-indexed");
    Ok(())
}

async fn backfill_notes(pool: &db::DbPool, search: &SearchClient, embedder: Option<&Embedder>) -> Result<usize> {
    let mut offset = 0;
    let mut total = 0;
    loop {
        let notes = db::notes::list_all_for_backfill(pool, BACKFILL_BATCH_SIZE, offset).await?;
        if notes.is_empty() {
            return Ok(total);
        }
        for note in &notes {
            if let Err(e) = search.index_note(note, embedder).await {
                tracing::error!("backfill: failed to index note {}: {e:#}", note.id);
            } else {
                total += 1;
            }
        }
        tracing::info!("backfill: {total} note(s) indexed so far");
        offset += BACKFILL_BATCH_SIZE;
    }
}

async fn backfill_pages(pool: &db::DbPool, search: &SearchClient, embedder: Option<&Embedder>) -> Result<usize> {
    let mut offset = 0;
    let mut total = 0;
    loop {
        let pages = db::pages::list_all_for_backfill(pool, BACKFILL_BATCH_SIZE, offset).await?;
        if pages.is_empty() {
            return Ok(total);
        }
        for page in &pages {
            if let Err(e) = search.index_page(page, embedder).await {
                tracing::error!("backfill: failed to index page {}: {e:#}", page.id);
            } else {
                total += 1;
            }
        }
        tracing::info!("backfill: {total} page(s) indexed so far");
        offset += BACKFILL_BATCH_SIZE;
    }
}

async fn process_batch(pool: &db::DbPool, search: &SearchClient, embedder: Option<&Embedder>) -> Result<usize> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    let rows = tx
        .query(
            "SELECT id, organization_id, content_type, content_id, event_type
             FROM outbox_events
             WHERE processed_at IS NULL
             ORDER BY created_at
             LIMIT $1
             FOR UPDATE SKIP LOCKED",
            &[&BATCH_SIZE],
        )
        .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    let events: Vec<OutboxEvent> = rows
        .iter()
        .map(|r| OutboxEvent {
            id: r.get("id"),
            organization_id: r.get("organization_id"),
            content_type: r.get("content_type"),
            content_id: r.get("content_id"),
            event_type: r.get("event_type"),
        })
        .collect();

    let mut processed = 0;
    for event in &events {
        match apply_event(pool, search, event, embedder).await {
            Ok(()) => {
                tx.execute("UPDATE outbox_events SET processed_at = NOW() WHERE id = $1", &[&event.id])
                    .await?;
                processed += 1;
            }
            Err(e) => {
                // Left unmarked -- retried on the next poll. Indexing is an
                // idempotent upsert, so a retry is always safe.
                tracing::error!(
                    "failed to apply outbox event {} ({} {}): {e:#}",
                    event.id, event.content_type, event.event_type
                );
            }
        }
    }

    tx.commit().await?;
    Ok(processed)
}

async fn apply_event(
    pool: &db::DbPool,
    search: &SearchClient,
    event: &OutboxEvent,
    embedder: Option<&Embedder>,
) -> Result<()> {
    match event.content_type.as_str() {
        "note" => {
            if event.event_type == "deleted" {
                return search.delete_note(event.content_id).await;
            }
            match db::notes::get_note(pool, event.content_id, event.organization_id).await {
                Ok(Some(note)) => search.index_note(&note, embedder).await,
                // Soft-deleted or otherwise gone by the time we got to it -- nothing to index.
                Ok(None) => Ok(()),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
        "page" => {
            if event.event_type == "deleted" {
                return search.delete_page(event.content_id).await;
            }
            match db::pages::get_page(pool, event.content_id, event.organization_id).await {
                Ok(Some(page)) => search.index_page(&page, embedder).await,
                Ok(None) => Ok(()),
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
        other => {
            tracing::warn!("unrecognized outbox content_type {other:?}, skipping");
            Ok(())
        }
    }
}
