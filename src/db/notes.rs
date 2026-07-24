use deadpool_postgres::Pool;
use serde_json::json;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::error::AppError;
use crate::models::note::{Note, NoteRevision, Visibility};

/// UUID encoded as a 32-char lowercase hex string with no hyphens — the only
/// UUID form that's a valid ltree label (labels are `[A-Za-z0-9_]+` only).
fn ltree_label(id: Uuid) -> String {
    id.simple().to_string()
}

fn row_to_note(row: &Row) -> Note {
    Note {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        team_id: row.get("team_id"),
        parent_id: row.get("parent_id"),
        visibility: Visibility::from_db_str(row.get("visibility")),
        body_markdown: row.get("body_markdown"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        reply_count: row.get("reply_count"),
    }
}

const NOTE_SELECT: &str = "
    SELECT n.id, n.organization_id, n.team_id, n.parent_id, n.visibility,
           n.created_by, n.created_at, n.updated_at,
           b.body_markdown,
           (SELECT COUNT(*) FROM notes r
            WHERE r.parent_id = n.id AND r.organization_id = n.organization_id AND r.deleted_at IS NULL
           ) AS reply_count
    FROM notes n
    JOIN note_bodies b ON b.note_id = n.id AND b.organization_id = n.organization_id
";

pub struct NewNote {
    pub organization_id: Uuid,
    pub team_id: Uuid,
    pub visibility: Visibility,
    pub created_by: Uuid,
    pub body_markdown: String,
}

/// Creates a top-level note: the note row, its body, its first revision, and
/// an outbox event, all in one transaction.
pub async fn create_note(pool: &Pool, new: NewNote) -> Result<Note, AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    let id = Uuid::new_v4();
    let thread_path = ltree_label(id);

    tx.execute(
        "INSERT INTO notes (id, organization_id, team_id, thread_path, visibility, created_by)
         VALUES ($1, $2, $3, $4::ltree, $5, $6)",
        &[&id, &new.organization_id, &new.team_id, &thread_path, &new.visibility.as_db_str(), &new.created_by],
    )
    .await?;

    insert_body_and_first_revision(&tx, id, new.organization_id, &new.body_markdown, new.created_by).await?;
    enqueue_outbox_event(&tx, new.organization_id, id, "created").await?;

    tx.commit().await?;

    get_note(pool, id, new.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("note {id} vanished immediately after insert"))
    })
}

/// Creates a reply: inherits organization_id/team_id/visibility from the
/// parent note (a reply can't have a broader or narrower audience than its
/// parent — same precedent as awe-server's own notes, which force
/// `is_shared=true` on replies to shared notes).
pub async fn create_reply(
    pool: &Pool,
    parent: &Note,
    created_by: Uuid,
    body_markdown: &str,
) -> Result<Note, AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    let id = Uuid::new_v4();
    let parent_row = tx
        .query_one(
            "SELECT thread_path::text FROM notes WHERE id = $1 AND organization_id = $2",
            &[&parent.id, &parent.organization_id],
        )
        .await?;
    let parent_path: String = parent_row.get(0);
    let thread_path = format!("{parent_path}.{}", ltree_label(id));

    tx.execute(
        "INSERT INTO notes (id, organization_id, team_id, thread_path, parent_id, visibility, created_by)
         VALUES ($1, $2, $3, $4::ltree, $5, $6, $7)",
        &[
            &id,
            &parent.organization_id,
            &parent.team_id,
            &thread_path,
            &parent.id,
            &parent.visibility.as_db_str(),
            &created_by,
        ],
    )
    .await?;

    insert_body_and_first_revision(&tx, id, parent.organization_id, body_markdown, created_by).await?;
    enqueue_outbox_event(&tx, parent.organization_id, id, "created").await?;

    tx.commit().await?;

    get_note(pool, id, parent.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("reply {id} vanished immediately after insert"))
    })
}

async fn insert_body_and_first_revision(
    tx: &deadpool_postgres::Transaction<'_>,
    note_id: Uuid,
    organization_id: Uuid,
    body_markdown: &str,
    edited_by: Uuid,
) -> Result<(), AppError> {
    tx.execute(
        "INSERT INTO note_bodies (note_id, organization_id, body_markdown) VALUES ($1, $2, $3)",
        &[&note_id, &organization_id, &body_markdown],
    )
    .await?;
    tx.execute(
        "INSERT INTO note_revisions (organization_id, note_id, version, body_markdown, edited_by)
         VALUES ($1, $2, 1, $3, $4)",
        &[&organization_id, &note_id, &body_markdown, &edited_by],
    )
    .await?;
    Ok(())
}

async fn enqueue_outbox_event(
    tx: &deadpool_postgres::Transaction<'_>,
    organization_id: Uuid,
    content_id: Uuid,
    event_type: &str,
) -> Result<(), AppError> {
    tx.execute(
        "INSERT INTO outbox_events (organization_id, content_type, content_id, event_type, payload)
         VALUES ($1, 'note', $2, $3, $4)",
        &[&organization_id, &content_id, &event_type, &json!({})],
    )
    .await?;
    Ok(())
}

pub async fn get_note(pool: &Pool, id: Uuid, organization_id: Uuid) -> Result<Option<Note>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{NOTE_SELECT} WHERE n.id = $1 AND n.organization_id = $2 AND n.deleted_at IS NULL");
    let row = client.query_opt(&sql, &[&id, &organization_id]).await?;
    Ok(row.as_ref().map(row_to_note))
}

/// Admin-only fallback for resolving a note id when the caller has no
/// organization membership to try (e.g. an admin outside any Tack team).
/// Scans across all partitions (no organization_id predicate) — acceptable
/// for a rare admin-lookup path, not something to optimize until it's
/// actually a hot path.
pub async fn get_note_admin_any_org(pool: &Pool, id: Uuid) -> Result<Option<Note>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{NOTE_SELECT} WHERE n.id = $1 AND n.deleted_at IS NULL");
    let row = client.query_opt(&sql, &[&id]).await?;
    Ok(row.as_ref().map(row_to_note))
}

/// Top-level notes filed under a specific team. `caller_id` scopes out
/// private notes that don't belong to the caller — the handler has already
/// verified the caller is a member of `team_id`, so team- and
/// organization-visibility notes are unconditionally included here.
pub async fn list_team_notes(
    pool: &Pool,
    organization_id: Uuid,
    team_id: Uuid,
    caller_id: Uuid,
) -> Result<Vec<Note>, AppError> {
    let client = pool.get().await?;
    let sql = format!(
        "{NOTE_SELECT}
         WHERE n.organization_id = $1 AND n.team_id = $2 AND n.parent_id IS NULL AND n.deleted_at IS NULL
           AND (n.visibility != 'private' OR n.created_by = $3)
         ORDER BY n.created_at DESC"
    );
    let rows = client.query(&sql, &[&organization_id, &team_id, &caller_id]).await?;
    Ok(rows.iter().map(row_to_note).collect())
}

pub async fn list_replies(pool: &Pool, parent_id: Uuid, organization_id: Uuid) -> Result<Vec<Note>, AppError> {
    let client = pool.get().await?;
    let sql = format!(
        "{NOTE_SELECT} WHERE n.parent_id = $1 AND n.organization_id = $2 AND n.deleted_at IS NULL
         ORDER BY n.created_at ASC"
    );
    let rows = client.query(&sql, &[&parent_id, &organization_id]).await?;
    Ok(rows.iter().map(row_to_note).collect())
}

/// Edits a note's body (creating a new revision) and/or visibility.
/// Caller must already be authorized (creator or admin) — enforced by the handler.
pub async fn update_note(
    pool: &Pool,
    note: &Note,
    new_body: Option<&str>,
    new_visibility: Option<Visibility>,
    edited_by: Uuid,
) -> Result<Note, AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    if let Some(body) = new_body {
        tx.execute(
            "UPDATE note_bodies SET body_markdown = $1 WHERE note_id = $2 AND organization_id = $3",
            &[&body, &note.id, &note.organization_id],
        )
        .await?;

        let version_row = tx
            .query_one(
                "SELECT COALESCE(MAX(version), 0) + 1 FROM note_revisions
                 WHERE note_id = $1 AND organization_id = $2",
                &[&note.id, &note.organization_id],
            )
            .await?;
        let next_version: i32 = version_row.get(0);
        tx.execute(
            "INSERT INTO note_revisions (organization_id, note_id, version, body_markdown, edited_by)
             VALUES ($1, $2, $3, $4, $5)",
            &[&note.organization_id, &note.id, &next_version, &body, &edited_by],
        )
        .await?;
    }

    if let Some(vis) = new_visibility {
        tx.execute(
            "UPDATE notes SET visibility = $1, updated_at = NOW() WHERE id = $2 AND organization_id = $3",
            &[&vis.as_db_str(), &note.id, &note.organization_id],
        )
        .await?;
    } else if new_body.is_some() {
        tx.execute(
            "UPDATE notes SET updated_at = NOW() WHERE id = $1 AND organization_id = $2",
            &[&note.id, &note.organization_id],
        )
        .await?;
    }

    if new_body.is_some() || new_visibility.is_some() {
        enqueue_outbox_event(&tx, note.organization_id, note.id, "updated").await?;
    }

    tx.commit().await?;

    get_note(pool, note.id, note.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("note {} vanished immediately after update", note.id))
    })
}

/// Soft-deletes a note. Caller must already be authorized (creator or admin).
pub async fn soft_delete_note(pool: &Pool, note: &Note) -> Result<(), AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;
    tx.execute(
        "UPDATE notes SET deleted_at = NOW() WHERE id = $1 AND organization_id = $2",
        &[&note.id, &note.organization_id],
    )
    .await?;
    enqueue_outbox_event(&tx, note.organization_id, note.id, "deleted").await?;
    tx.commit().await?;
    Ok(())
}

pub async fn list_revisions(
    pool: &Pool,
    note_id: Uuid,
    organization_id: Uuid,
) -> Result<Vec<NoteRevision>, AppError> {
    let client = pool.get().await?;
    let rows = client
        .query(
            "SELECT id, note_id, version, body_markdown, edited_by, edited_at
             FROM note_revisions WHERE note_id = $1 AND organization_id = $2
             ORDER BY version DESC",
            &[&note_id, &organization_id],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| NoteRevision {
            id: r.get("id"),
            note_id: r.get("note_id"),
            version: r.get("version"),
            body_markdown: r.get("body_markdown"),
            edited_by: r.get("edited_by"),
            edited_at: r.get("edited_at"),
        })
        .collect())
}
