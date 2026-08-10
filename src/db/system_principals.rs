use deadpool_postgres::Pool;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::error::AppError;
use crate::models::system_principal::SystemPrincipal;

fn row_to_principal(row: &Row) -> SystemPrincipal {
    SystemPrincipal {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        label: row.get("label"),
        created_at: row.get("created_at"),
    }
}

pub async fn create_principal(pool: &Pool, organization_id: Uuid, label: &str) -> Result<SystemPrincipal, AppError> {
    let client = pool.get().await?;
    let row = client
        .query_one(
            "INSERT INTO system_principals (organization_id, label) VALUES ($1, $2)
             RETURNING id, organization_id, label, created_at",
            &[&organization_id, &label],
        )
        .await?;
    Ok(row_to_principal(&row))
}

/// Paginated, alphabetical by label -- same shape as every other list in
/// this API (`note_folders::list_folders_for_teams`, etc.), not a
/// "load everything" call, even though a given org's system-principal count
/// is small today -- see the standing "design for scale by default" rule.
pub async fn list_principals(pool: &Pool, organization_id: Uuid, limit: i64, offset: i64) -> Result<(Vec<SystemPrincipal>, i64), AppError> {
    let client = pool.get().await?;
    let rows = client
        .query(
            "SELECT id, organization_id, label, created_at FROM system_principals
             WHERE organization_id = $1 ORDER BY lower(label) ASC LIMIT $2 OFFSET $3",
            &[&organization_id, &limit, &offset],
        )
        .await?;
    let principals = rows.iter().map(row_to_principal).collect();

    let total: i64 = client
        .query_one("SELECT COUNT(*) FROM system_principals WHERE organization_id = $1", &[&organization_id])
        .await?
        .get(0);

    Ok((principals, total))
}

pub async fn get_principal(pool: &Pool, id: Uuid, organization_id: Uuid) -> Result<Option<SystemPrincipal>, AppError> {
    let client = pool.get().await?;
    let row = client
        .query_opt(
            "SELECT id, organization_id, label, created_at FROM system_principals WHERE id = $1 AND organization_id = $2",
            &[&id, &organization_id],
        )
        .await?;
    Ok(row.as_ref().map(row_to_principal))
}

/// Deletes a principal by id alone, without knowing its organization_id up
/// front -- only reachable from the admin-only delete handler, mirroring
/// `note_folders::get_folder_admin_any_org`'s "id is a real gen_random_uuid(),
/// effectively globally unique, so scanning across partitions on id alone is
/// correct, just not partition-pruned" reasoning. Returns `NotFound` if no
/// row matched.
pub async fn delete_principal(pool: &Pool, id: Uuid) -> Result<(), AppError> {
    let client = pool.get().await?;
    let deleted = client.execute("DELETE FROM system_principals WHERE id = $1", &[&id]).await?;
    if deleted == 0 {
        return Err(AppError::NotFound("System principal not found.".into()));
    }
    Ok(())
}
