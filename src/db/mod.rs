pub mod note_folders;
pub mod note_reads;
pub mod notes;
pub mod pages;
pub mod spaces;
pub mod system_principals;

use anyhow::{Context, Result};
use deadpool_postgres::{Config as PgConfig, ManagerConfig, Pool, RecyclingMethod, Runtime};
use include_dir::{include_dir, Dir};
use tokio_postgres::NoTls;

static MIGRATIONS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");

pub type DbPool = Pool;

pub fn create_pool(database_url: &str) -> Result<Pool> {
    let mut cfg = PgConfig::new();
    cfg.url = Some(database_url.to_string());
    cfg.manager = Some(ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    });

    cfg.create_pool(Some(Runtime::Tokio1), NoTls)
        .context("Failed to create database pool")
}

/// Applies every `migrations/*.sql` file, in filename order, on every startup.
/// Each file must be safe to re-run (`CREATE TABLE IF NOT EXISTS`, `ADD COLUMN
/// IF NOT EXISTS`, etc.) — matches ullav-dam-server's convention, not
/// ullav-user-management's separate migrate-sidecar + schema_migrations
/// tracking table.
pub async fn run_migrations(pool: &Pool) -> Result<()> {
    let client = pool.get().await.context("Failed to get DB connection for migrations")?;

    let mut files: Vec<_> = MIGRATIONS_DIR.files().collect();
    files.sort_by_key(|f| f.path());

    for file in files {
        let sql = file
            .contents_utf8()
            .with_context(|| format!("Migration file is not valid UTF-8: {}", file.path().display()))?;
        client
            .batch_execute(sql)
            .await
            .with_context(|| format!("Failed to run migration: {}", file.path().display()))?;
    }

    tracing::info!("Migrations applied successfully");
    Ok(())
}
