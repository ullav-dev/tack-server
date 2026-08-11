use deadpool_postgres::Pool;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::error::AppError;
use crate::models::note::NoteFolder;

// `note_count`'s subquery deliberately mirrors `list_team_notes`' own
// visibility predicate (`visibility != 'private' OR created_by = caller`),
// parameterized as `$N` by each caller below -- otherwise the badge (and
// `DeleteNoteFolderModal`'s "N notes will be unfiled" body) would count a
// colleague's private notes the caller can't even see in the list beneath
// it, which is both a wrong number and a small ACL leak (it discloses that
// private notes exist and how many).
const FOLDER_SELECT: &str = "
    SELECT f.id, f.organization_id, f.team_id, f.name, f.owning_service, f.entity_type, f.entity_id,
           f.created_at, f.updated_at,
           (SELECT COUNT(*) FROM notes n
            WHERE n.folder_id = f.id AND n.organization_id = f.organization_id AND n.deleted_at IS NULL
              AND (n.visibility != 'private' OR n.created_by = {caller_id})
           ) AS note_count
    FROM note_folders f
";

/// Admin's `note_count` counts every note regardless of visibility --
/// admins already bypass visibility everywhere else in this API (see
/// `notes_acl::can_view`), so there's no caller to scope against.
const FOLDER_SELECT_ADMIN: &str = "
    SELECT f.id, f.organization_id, f.team_id, f.name, f.owning_service, f.entity_type, f.entity_id,
           f.created_at, f.updated_at,
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
        owning_service: row.get("owning_service"),
        entity_type: row.get("entity_type"),
        entity_id: row.get("entity_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        note_count: row.get("note_count"),
    }
}

/// `attach`, if given, scopes the new folder to one entity instead of
/// leaving it team-wide -- see `NoteFolder`'s doc comment for the
/// all-or-nothing invariant (enforced again at the DB level by
/// `note_folders_entity_scope_all_or_nothing`).
pub async fn create_folder(
    pool: &Pool,
    organization_id: Uuid,
    team_id: Uuid,
    name: &str,
    caller_id: Uuid,
    attach: Option<&crate::models::note::AttachRequest>,
) -> Result<NoteFolder, AppError> {
    let client = pool.get().await?;
    let row = client
        .query_one(
            "INSERT INTO note_folders (organization_id, team_id, name, owning_service, entity_type, entity_id)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
            &[
                &organization_id,
                &team_id,
                &name,
                &attach.map(|a| a.owning_service.as_str()),
                &attach.map(|a| a.entity_type.as_str()),
                &attach.map(|a| a.entity_id.as_str()),
            ],
        )
        .await?;
    let id: Uuid = row.get("id");
    get_folder(pool, id, organization_id, caller_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("folder {id} vanished immediately after insert"))
    })
}

pub async fn get_folder(pool: &Pool, id: Uuid, organization_id: Uuid, caller_id: Uuid) -> Result<Option<NoteFolder>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{} WHERE f.id = $1 AND f.organization_id = $2", FOLDER_SELECT.replace("{caller_id}", "$3"));
    let row = client.query_opt(&sql, &[&id, &organization_id, &caller_id]).await?;
    Ok(row.as_ref().map(row_to_folder))
}

/// Admin-only fallback, mirroring `notes::get_note_admin_any_org` /
/// `spaces::get_space_admin_any_org`. Uses `FOLDER_SELECT_ADMIN` (counts
/// every note, no visibility filter) since only an admin caller ever
/// reaches this path.
pub async fn get_folder_admin_any_org(pool: &Pool, id: Uuid) -> Result<Option<NoteFolder>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{FOLDER_SELECT_ADMIN} WHERE f.id = $1");
    let row = client.query_opt(&sql, &[&id]).await?;
    Ok(row.as_ref().map(row_to_folder))
}

/// Folders visible to the caller: every folder belonging to any of their
/// teams, within one organization (mirrors `spaces::list_spaces_for_teams`'
/// per-organization call shape).
/// Paginated within *this* organization -- the handler loops one call per
/// organization the caller belongs to (same shape as every other
/// per-organization list in this API) and sums the totals, so a caller with
/// folders spread across multiple organizations sees `limit` folders from
/// *each* org per page rather than one true globally-ordered page. A
/// documented tradeoff, not an oversight: merging and re-slicing across
/// organizations would need a fan-out query this API doesn't otherwise do
/// anywhere, for a case (a caller's folders split across orgs) that's rare
/// in practice.
/// Team-wide folders only -- `f.owning_service IS NULL` excludes
/// entity-scoped folders (see `NoteFolder`'s doc comment), so a cunav
/// ticket's own sub-folders never show up in tack's team-wide Navigator
/// folder list or another entity's folder picker.
pub async fn list_folders_for_teams(
    pool: &Pool,
    organization_id: Uuid,
    team_ids: &[Uuid],
    caller_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<NoteFolder>, i64), AppError> {
    let client = pool.get().await?;
    let sql = format!(
        "{} WHERE f.organization_id = $1 AND f.team_id = ANY($2) AND f.owning_service IS NULL
         ORDER BY lower(f.name) ASC LIMIT $4 OFFSET $5",
        FOLDER_SELECT.replace("{caller_id}", "$3")
    );
    let rows = client.query(&sql, &[&organization_id, &team_ids, &caller_id, &limit, &offset]).await?;
    let folders = rows.iter().map(row_to_folder).collect();

    let total: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM note_folders WHERE organization_id = $1 AND team_id = ANY($2) AND owning_service IS NULL",
            &[&organization_id, &team_ids],
        )
        .await?
        .get(0);

    Ok((folders, total))
}

/// A specific entity's own folders (e.g. one cunav ticket's sub-folders),
/// oldest-first -- the folder analogue of `list_notes_by_attachment`. Not
/// scoped by `team_ids` the way `list_folders_for_teams` is: the handler
/// instead scans the caller's organizations the same way
/// `list_notes_by_attachment`'s handler does, since an entity's folders --
/// like its notes -- only ever belong to one organization in practice.
pub async fn list_folders_for_entity(
    pool: &Pool,
    organization_id: Uuid,
    owning_service: &str,
    entity_type: &str,
    entity_id: &str,
    caller_id: Uuid,
) -> Result<Vec<NoteFolder>, AppError> {
    let client = pool.get().await?;
    let sql = format!(
        "{} WHERE f.organization_id = $1 AND f.owning_service = $2 AND f.entity_type = $3 AND f.entity_id = $4
         ORDER BY lower(f.name) ASC",
        FOLDER_SELECT.replace("{caller_id}", "$5")
    );
    let rows = client.query(&sql, &[&organization_id, &owning_service, &entity_type, &entity_id, &caller_id]).await?;
    Ok(rows.iter().map(row_to_folder).collect())
}

pub async fn rename_folder(pool: &Pool, folder: &NoteFolder, name: &str, caller_id: Uuid) -> Result<NoteFolder, AppError> {
    let client = pool.get().await?;
    client
        .execute(
            "UPDATE note_folders SET name = $1, updated_at = NOW() WHERE id = $2 AND organization_id = $3",
            &[&name, &folder.id, &folder.organization_id],
        )
        .await?;
    get_folder(pool, folder.id, folder.organization_id, caller_id).await?.ok_or_else(|| {
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
