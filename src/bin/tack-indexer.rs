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

use anyhow::Result;
use std::time::Duration;
use tack_server::{config::Config, db, embeddings::Embedder, search::SearchClient};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use uuid::Uuid;

const BATCH_SIZE: i64 = 20;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

struct OutboxEvent {
    id: Uuid,
    organization_id: Uuid,
    content_type: String,
    content_id: Uuid,
    event_type: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let _ = dotenvy::dotenv();
    let cfg = Config::from_env()?;

    // Own pool, separate from tack-server's — a backfill/indexing burst here
    // must never starve the API server's request-serving connections.
    let pool = db::create_pool(&cfg.database_url)?;
    let search = SearchClient::new(cfg.opensearch_url);
    search.ensure_index().await?;

    // Same non-fatal-degrade posture as tack-server: if the model can't
    // load, keep indexing lexically rather than refusing to start -- a
    // model that comes back later (e.g. cache volume mounted on redeploy)
    // just needs a re-index, not a code change.
    let embedder = {
        let cache_dir = cfg.embedding_model_cache_dir.clone();
        match tokio::task::spawn_blocking(move || Embedder::new(cache_dir)).await {
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
    if event.content_type != "note" {
        // Pages don't exist yet -- nothing to do, not an error.
        return Ok(());
    }
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
