use deadpool_postgres::Pool;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::error::AppError;
use crate::models::note::NoteFolder;

const FOLDER_SELECT: &str = "
    SELECT f.id, f.organization_id, f.team_id, f.name, f.created_at, f.updated_at,
           (SELECT COUNT(*) FROM notes n
            WHERE n.folder_id = f.id AND n.organization_id = f.organization_id AND n.deleted_at IS NULL
           ) AS note_count
    FROM note_folders f
";

fn row_to_folder(row: &Row) -> NoteFolder {
    NoteFolder {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        team_id: row.get("team_id"),
        name: row.get("name"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        note_count: row.get("note_count"),
    }
}

pub async fn create_folder(pool: &Pool, organization_id: Uuid, team_id: Uuid, name: &str) -> Result<NoteFolder, AppError> {
    let client = pool.get().await?;
    let row = client
        .query_one(
            "INSERT INTO note_folders (organization_id, team_id, name) VALUES ($1, $2, $3) RETURNING id",
            &[&organization_id, &team_id, &name],
        )
        .await?;
    let id: Uuid = row.get("id");
    get_folder(pool, id, organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("folder {id} vanished immediately after insert"))
    })
}

pub async fn get_folder(pool: &Pool, id: Uuid, organization_id: Uuid) -> Result<Option<NoteFolder>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{FOLDER_SELECT} WHERE f.id = $1 AND f.organization_id = $2");
    let row = client.query_opt(&sql, &[&id, &organization_id]).await?;
    Ok(row.as_ref().map(row_to_folder))
}

/// Admin-only fallback, mirroring `notes::get_note_admin_any_org` /
/// `spaces::get_space_admin_any_org`.
pub async fn get_folder_admin_any_org(pool: &Pool, id: Uuid) -> Result<Option<NoteFolder>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{FOLDER_SELECT} WHERE f.id = $1");
    let row = client.query_opt(&sql, &[&id]).await?;
    Ok(row.as_ref().map(row_to_folder))
}

/// Folders visible to the caller: every folder belonging to any of their
/// teams, within one organization (mirrors `spaces::list_spaces_for_teams`'
/// per-organization call shape).
pub async fn list_folders_for_teams(pool: &Pool, organization_id: Uuid, team_ids: &[Uuid]) -> Result<Vec<NoteFolder>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{FOLDER_SELECT} WHERE f.organization_id = $1 AND f.team_id = ANY($2) ORDER BY f.name ASC");
    let rows = client.query(&sql, &[&organization_id, &team_ids]).await?;
    Ok(rows.iter().map(row_to_folder).collect())
}

pub async fn rename_folder(pool: &Pool, folder: &NoteFolder, name: &str) -> Result<NoteFolder, AppError> {
    let client = pool.get().await?;
    client
        .execute(
            "UPDATE note_folders SET name = $1, updated_at = NOW() WHERE id = $2 AND organization_id = $3",
            &[&name, &folder.id, &folder.organization_id],
        )
        .await?;
    get_folder(pool, folder.id, folder.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("folder {} vanished immediately after rename", folder.id))
    })
}

/// Deletes a folder. Every note currently filed in it is explicitly unfiled
/// (`folder_id = NULL`) in the same transaction first -- notes are never
/// deleted along with their folder, matching "removing the folder just
/// removes the grouping" (the same semantics Apple Notes/Evernote-style
/// apps use, and consistent with `notes_folder_id_fkey` carrying no
/// `ON DELETE` action of its own; see `008_note_folders.sql`).
pub async fn delete_folder(pool: &Pool, folder: &NoteFolder) -> Result<(), AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;
    tx.execute(
        "UPDATE notes SET folder_id = NULL, updated_at = NOW() WHERE folder_id = $1 AND organization_id = $2",
        &[&folder.id, &folder.organization_id],
    )
    .await?;
    let deleted = tx
        .execute("DELETE FROM note_folders WHERE id = $1 AND organization_id = $2", &[&folder.id, &folder.organization_id])
        .await?;
    if deleted == 0 {
        return Err(AppError::NotFound("Folder not found.".into()));
    }
    tx.commit().await?;
    Ok(())
}
