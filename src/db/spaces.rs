use deadpool_postgres::Pool;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::error::AppError;
use crate::models::page::Space;

fn row_to_space(row: &Row) -> Space {
    Space {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        owning_service: row.get("owning_service"),
        team_id: row.get("team_id"),
        name: row.get("name"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

const SPACE_SELECT: &str =
    "SELECT id, organization_id, owning_service, team_id, name, created_at, updated_at FROM spaces";

pub async fn create_space(
    pool: &Pool,
    organization_id: Uuid,
    team_id: Uuid,
    name: &str,
) -> Result<Space, AppError> {
    let client = pool.get().await?;
    let row = client
        .query_one(
            &format!(
                "INSERT INTO spaces (organization_id, owning_service, team_id, name)
                 VALUES ($1, 'tack', $2, $3)
                 RETURNING id, organization_id, owning_service, team_id, name, created_at, updated_at"
            ),
            &[&organization_id, &team_id, &name],
        )
        .await?;
    Ok(row_to_space(&row))
}

pub async fn get_space(pool: &Pool, id: Uuid, organization_id: Uuid) -> Result<Option<Space>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{SPACE_SELECT} WHERE id = $1 AND organization_id = $2");
    let row = client.query_opt(&sql, &[&id, &organization_id]).await?;
    Ok(row.as_ref().map(row_to_space))
}

/// Admin-only fallback, mirroring `notes::get_note_admin_any_org`.
pub async fn get_space_admin_any_org(pool: &Pool, id: Uuid) -> Result<Option<Space>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{SPACE_SELECT} WHERE id = $1");
    let row = client.query_opt(&sql, &[&id]).await?;
    Ok(row.as_ref().map(row_to_space))
}

/// Spaces the caller can see: any space belonging to one of their teams, or
/// any org-wide space (`team_id IS NULL`) in one of their organizations.
/// Mirrors the same "resolve across each of the caller's orgs" shape as
/// notes, since a space's organization_id isn't known up front either.
pub async fn list_spaces_for_teams(
    pool: &Pool,
    organization_id: Uuid,
    team_ids: &[Uuid],
) -> Result<Vec<Space>, AppError> {
    let client = pool.get().await?;
    let sql = format!(
        "{SPACE_SELECT} WHERE organization_id = $1 AND (team_id IS NULL OR team_id = ANY($2))
         ORDER BY name ASC"
    );
    let rows = client.query(&sql, &[&organization_id, &team_ids]).await?;
    Ok(rows.iter().map(row_to_space).collect())
}
