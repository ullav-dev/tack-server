use chrono::{DateTime, Utc};
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
        folder_id: row.get("folder_id"),
        visibility: Visibility::from_db_str(row.get("visibility")),
        title: row.get("title"),
        body_markdown: row.get("body_markdown"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        reply_count: row.get("reply_count"),
        in_reply_to_version: row.get("in_reply_to_version"),
    }
}

const NOTE_SELECT: &str = "
    SELECT n.id, n.organization_id, n.team_id, n.parent_id, n.folder_id, n.visibility, n.title,
           n.created_by, n.created_at, n.updated_at, n.in_reply_to_version,
           b.body_markdown,
           (SELECT COUNT(*) FROM notes r
            WHERE r.parent_id = n.id AND r.organization_id = n.organization_id AND r.deleted_at IS NULL
           ) AS reply_count
    FROM notes n
    JOIN note_bodies b ON b.note_id = n.id AND b.organization_id = n.organization_id
";

/// An external entity to attach a newly created note to, e.g. a lagan pull
/// request's discussion thread — backs `content_attachments`.
pub struct NewAttachment {
    pub owning_service: String,
    pub entity_type: String,
    pub entity_id: String,
}

pub struct NewNote {
    pub organization_id: Uuid,
    pub team_id: Uuid,
    pub visibility: Visibility,
    pub created_by: Uuid,
    pub title: String,
    pub body_markdown: String,
    /// Must belong to `team_id` -- checked by the handler before this is
    /// called (`db::note_folders::folder_belongs_to_team`).
    pub folder_id: Option<Uuid>,
    pub attach: Option<NewAttachment>,
    /// Backfill-only override — see `CreateNoteRequest::created_at`. `None`
    /// means "now," same as before this field existed.
    pub created_at: Option<DateTime<Utc>>,
}

/// Creates a top-level note: the note row, its body, its first revision, an
/// optional `content_attachments` row, and an outbox event, all in one
/// transaction.
pub async fn create_note(pool: &Pool, new: NewNote) -> Result<Note, AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    let id = Uuid::new_v4();
    let thread_path = ltree_label(id);
    let created_at = new.created_at.unwrap_or_else(Utc::now);

    tx.execute(
        "INSERT INTO notes (id, organization_id, team_id, thread_path, visibility, title, created_by, folder_id, created_at, updated_at)
         VALUES ($1, $2, $3, $4::ltree, $5, $6, $7, $8, $9, $9)",
        &[
            &id,
            &new.organization_id,
            &new.team_id,
            &thread_path,
            &new.visibility.as_db_str(),
            &new.title,
            &new.created_by,
            &new.folder_id,
            &created_at,
        ],
    )
    .await?;

    insert_body_and_first_revision(&tx, id, new.organization_id, &new.body_markdown, new.created_by, created_at)
        .await?;

    if let Some(attach) = &new.attach {
        insert_attachment(&tx, new.organization_id, id, attach, created_at).await?;
    }

    enqueue_outbox_event(&tx, new.organization_id, id, "created").await?;

    tx.commit().await?;

    get_note(pool, id, new.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("note {id} vanished immediately after insert"))
    })
}

async fn insert_attachment(
    tx: &deadpool_postgres::Transaction<'_>,
    organization_id: Uuid,
    note_id: Uuid,
    attach: &NewAttachment,
    created_at: DateTime<Utc>,
) -> Result<(), AppError> {
    tx.execute(
        "INSERT INTO content_attachments (organization_id, content_type, content_id, owning_service, entity_type, entity_id, created_at)
         VALUES ($1, 'note', $2, $3, $4, $5, $6)",
        &[&organization_id, &note_id, &attach.owning_service, &attach.entity_type, &attach.entity_id, &created_at],
    )
    .await?;
    Ok(())
}

fn row_to_attachment(row: &Row) -> crate::models::note::NoteAttachment {
    crate::models::note::NoteAttachment {
        id: row.get("id"),
        note_id: row.get("content_id"),
        owning_service: row.get("owning_service"),
        entity_type: row.get("entity_type"),
        entity_id: row.get("entity_id"),
        created_at: row.get("created_at"),
    }
}

/// Attaches an *already-created* note to another entity -- the standalone
/// counterpart to `insert_attachment` (which only runs inside
/// `create_note`'s transaction). Lets a caller like Cartlann link one note to
/// several of its own objects over time, since `CreateNoteRequest.attach`
/// only ever supports a single attachment made at creation.
pub async fn attach_note(
    pool: &Pool,
    organization_id: Uuid,
    note_id: Uuid,
    attach: &NewAttachment,
) -> Result<crate::models::note::NoteAttachment, AppError> {
    let client = pool.get().await?;
    let row = client
        .query_one(
            "INSERT INTO content_attachments (organization_id, content_type, content_id, owning_service, entity_type, entity_id)
             VALUES ($1, 'note', $2, $3, $4, $5)
             RETURNING id, content_id, owning_service, entity_type, entity_id, created_at",
            &[&organization_id, &note_id, &attach.owning_service, &attach.entity_type, &attach.entity_id],
        )
        .await?;
    Ok(row_to_attachment(&row))
}

/// A note's own attachments -- the reverse of `list_notes_by_attachment`
/// (that finds notes given an entity; this finds entities given a note).
pub async fn list_note_attachments(
    pool: &Pool,
    organization_id: Uuid,
    note_id: Uuid,
) -> Result<Vec<crate::models::note::NoteAttachment>, AppError> {
    let client = pool.get().await?;
    let rows = client
        .query(
            "SELECT id, content_id, owning_service, entity_type, entity_id, created_at
             FROM content_attachments
             WHERE organization_id = $1 AND content_type = 'note' AND content_id = $2
             ORDER BY created_at ASC",
            &[&organization_id, &note_id],
        )
        .await?;
    Ok(rows.iter().map(row_to_attachment).collect())
}

pub async fn delete_note_attachment(
    pool: &Pool,
    organization_id: Uuid,
    note_id: Uuid,
    attachment_id: Uuid,
) -> Result<(), AppError> {
    let client = pool.get().await?;
    let deleted = client
        .execute(
            "DELETE FROM content_attachments
             WHERE id = $1 AND organization_id = $2 AND content_type = 'note' AND content_id = $3",
            &[&attachment_id, &organization_id, &note_id],
        )
        .await?;
    if deleted == 0 {
        return Err(AppError::NotFound("Attachment not found.".into()));
    }
    Ok(())
}

/// Top-level notes attached to a specific external entity (e.g. a lagan pull
/// request's discussion thread), newest-first — mirrors `list_team_notes`'
/// shape but joined through `content_attachments` instead of filtered by
/// `team_id`. Replies aren't attached rows themselves (only their parent is)
/// so callers fetch each note's replies separately via the existing
/// `list_replies`, same as the top-level/reply split everywhere else in this
/// API.
pub async fn list_notes_by_attachment(
    pool: &Pool,
    organization_id: Uuid,
    owning_service: &str,
    entity_type: &str,
    entity_id: &str,
) -> Result<Vec<Note>, AppError> {
    let client = pool.get().await?;
    let sql = format!(
        "{NOTE_SELECT}
         JOIN content_attachments a ON a.content_type = 'note' AND a.content_id = n.id AND a.organization_id = n.organization_id
         WHERE n.organization_id = $1 AND n.deleted_at IS NULL
           AND a.owning_service = $2 AND a.entity_type = $3 AND a.entity_id = $4
         ORDER BY n.created_at ASC"
    );
    let rows = client.query(&sql, &[&organization_id, &owning_service, &entity_type, &entity_id]).await?;
    Ok(rows.iter().map(row_to_note).collect())
}

/// Creates a reply: inherits organization_id/team_id/visibility from the
/// parent note (a reply can't have a broader or narrower audience than its
/// parent — same precedent as awe-server's own notes, which force
/// `is_shared=true` on replies to shared notes). Also tags the reply with
/// the parent's latest saved version number, so it can later be shown only
/// while browsing that version (see `in_reply_to_version` on the model) —
/// there's always at least version 1 (auto-created at note-creation time),
/// so this is never null for a freshly created reply.
pub async fn create_reply(
    pool: &Pool,
    parent: &Note,
    created_by: Uuid,
    body_markdown: &str,
    created_at: Option<DateTime<Utc>>,
) -> Result<Note, AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    let id = Uuid::new_v4();
    let created_at = created_at.unwrap_or_else(Utc::now);
    let parent_row = tx
        .query_one(
            "SELECT thread_path::text FROM notes WHERE id = $1 AND organization_id = $2",
            &[&parent.id, &parent.organization_id],
        )
        .await?;
    let parent_path: String = parent_row.get(0);
    let thread_path = format!("{parent_path}.{}", ltree_label(id));

    let version_row = tx
        .query_one(
            "SELECT COALESCE(MAX(version), 1) FROM note_revisions
             WHERE note_id = $1 AND organization_id = $2",
            &[&parent.id, &parent.organization_id],
        )
        .await?;
    let in_reply_to_version: i32 = version_row.get(0);

    tx.execute(
        "INSERT INTO notes (id, organization_id, team_id, thread_path, parent_id, visibility, created_by, in_reply_to_version, created_at, updated_at)
         VALUES ($1, $2, $3, $4::ltree, $5, $6, $7, $8, $9, $9)",
        &[
            &id,
            &parent.organization_id,
            &parent.team_id,
            &thread_path,
            &parent.id,
            &parent.visibility.as_db_str(),
            &created_by,
            &in_reply_to_version,
            &created_at,
        ],
    )
    .await?;

    insert_body_and_first_revision(&tx, id, parent.organization_id, body_markdown, created_by, created_at).await?;
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
    edited_at: DateTime<Utc>,
) -> Result<(), AppError> {
    tx.execute(
        "INSERT INTO note_bodies (note_id, organization_id, body_markdown) VALUES ($1, $2, $3)",
        &[&note_id, &organization_id, &body_markdown],
    )
    .await?;
    tx.execute(
        "INSERT INTO note_revisions (organization_id, note_id, version, body_markdown, edited_by, edited_at)
         VALUES ($1, $2, 1, $3, $4, $5)",
        &[&organization_id, &note_id, &body_markdown, &edited_by, &edited_at],
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

/// Narrows `list_team_notes` by folder. `None` (the default, via
/// `GET /notes?team_id=` with no folder params) preserves the original
/// unfiltered behavior -- every existing caller (`NotesList.tsx`, the MCP
/// `search_content`/`get_note_thread` path) keeps working unchanged.
pub enum FolderScope {
    /// Only notes filed in this folder.
    Folder(Uuid),
    /// Only notes with no folder at all.
    Unfiled,
}

/// Top-level notes filed under a specific team. `caller_id` scopes out
/// private notes that don't belong to the caller — the handler has already
/// verified the caller is a member of `team_id`, so team- and
/// organization-visibility notes are unconditionally included here.
///
/// Simple offset pagination: fetches `limit + 1` rows so `has_more` can be
/// derived from whether that extra row came back, without a separate
/// `COUNT(*)` query — a team's note volume doesn't yet warrant cursor-based
/// pagination's extra complexity.
pub async fn list_team_notes(
    pool: &Pool,
    organization_id: Uuid,
    team_id: Uuid,
    caller_id: Uuid,
    folder: Option<FolderScope>,
    limit: i64,
    offset: i64,
) -> Result<crate::models::note::NotesPage, AppError> {
    let client = pool.get().await?;
    let folder_clause = match folder {
        None => "",
        Some(FolderScope::Folder(_)) => "AND n.folder_id = $6",
        Some(FolderScope::Unfiled) => "AND n.folder_id IS NULL",
    };
    let sql = format!(
        "{NOTE_SELECT}
         WHERE n.organization_id = $1 AND n.team_id = $2 AND n.parent_id IS NULL AND n.deleted_at IS NULL
           AND (n.visibility != 'private' OR n.created_by = $3)
           {folder_clause}
         ORDER BY n.created_at DESC
         LIMIT $4 OFFSET $5"
    );
    let rows = match folder {
        Some(FolderScope::Folder(folder_id)) => {
            client
                .query(&sql, &[&organization_id, &team_id, &caller_id, &(limit + 1), &offset, &folder_id])
                .await?
        }
        _ => client.query(&sql, &[&organization_id, &team_id, &caller_id, &(limit + 1), &offset]).await?,
    };
    let has_more = rows.len() as i64 > limit;
    let notes = rows.iter().take(limit as usize).map(row_to_note).collect();

    // A separate COUNT(*) against the bare `notes` table (not the
    // NOTE_SELECT join, which pulls note_bodies/reply_count along for every
    // row -- wasted work for a query that only needs a number) with the
    // exact same predicate, so the frontend can render "Page N of M"
    // instead of just "there might be more." Its own folder clause, not
    // `folder_clause` above -- that one's `$6` placeholder is only valid
    // alongside the main query's limit/offset params ($4/$5); this query
    // has no limit/offset, so a folder id (if any) is `$4` here instead.
    let count_folder_clause = match folder {
        None => "",
        Some(FolderScope::Folder(_)) => "AND n.folder_id = $4",
        Some(FolderScope::Unfiled) => "AND n.folder_id IS NULL",
    };
    let count_sql = format!(
        "SELECT COUNT(*) FROM notes n
         WHERE n.organization_id = $1 AND n.team_id = $2 AND n.parent_id IS NULL AND n.deleted_at IS NULL
           AND (n.visibility != 'private' OR n.created_by = $3)
           {count_folder_clause}"
    );
    let total: i64 = match folder {
        Some(FolderScope::Folder(folder_id)) => {
            client.query_one(&count_sql, &[&organization_id, &team_id, &caller_id, &folder_id]).await?.get(0)
        }
        _ => client.query_one(&count_sql, &[&organization_id, &team_id, &caller_id]).await?.get(0),
    };

    Ok(crate::models::note::NotesPage { notes, has_more, total })
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

/// Edits a note's title, body, and/or visibility. Caller must already be
/// authorized (creator or admin) — enforced by the handler.
///
/// Does **not** create a new revision — a revision is a deliberate snapshot
/// ("this is a version worth being able to come back to"), taken only via
/// `create_revision` below, not implicitly on every autosave-style edit.
/// Version 1 is still created automatically at note-creation time (see
/// `insert_body_and_first_revision`) as a reasonable baseline snapshot.
pub async fn update_note(
    pool: &Pool,
    note: &Note,
    new_title: Option<&str>,
    new_body: Option<&str>,
    new_visibility: Option<Visibility>,
    // Tri-state, mirroring `UpdateNoteRequest::folder_id`: `None` = leave
    // unchanged, `Some(None)` = unfile, `Some(Some(id))` = file/move there.
    // Already validated by the handler (top-level note only, folder belongs
    // to the note's own team).
    new_folder_id: Option<Option<Uuid>>,
) -> Result<Note, AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    if let Some(folder_id) = new_folder_id {
        tx.execute(
            "UPDATE notes SET folder_id = $1, updated_at = NOW() WHERE id = $2 AND organization_id = $3",
            &[&folder_id, &note.id, &note.organization_id],
        )
        .await?;
    }

    if let Some(title) = new_title {
        tx.execute(
            "UPDATE notes SET title = $1, updated_at = NOW() WHERE id = $2 AND organization_id = $3",
            &[&title, &note.id, &note.organization_id],
        )
        .await?;
    }

    if let Some(body) = new_body {
        tx.execute(
            "UPDATE note_bodies SET body_markdown = $1 WHERE note_id = $2 AND organization_id = $3",
            &[&body, &note.id, &note.organization_id],
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

    if new_title.is_some() || new_body.is_some() || new_visibility.is_some() {
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

/// Snapshots a note's *current* body as a new named version — a deliberate
/// action by an authorized user (caller must already be authorized; enforced
/// by the handler), not something that happens implicitly on every save. The
/// snapshot is of whatever `note_bodies.body_markdown` holds right now, so
/// any edits made via `update_note` since the last version become part of
/// this new one.
pub async fn create_revision(pool: &Pool, note: &Note, edited_by: Uuid) -> Result<NoteRevision, AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    let body_row = tx
        .query_one(
            "SELECT body_markdown FROM note_bodies WHERE note_id = $1 AND organization_id = $2",
            &[&note.id, &note.organization_id],
        )
        .await?;
    let body_markdown: String = body_row.get(0);

    let version_row = tx
        .query_one(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM note_revisions
             WHERE note_id = $1 AND organization_id = $2",
            &[&note.id, &note.organization_id],
        )
        .await?;
    let next_version: i32 = version_row.get(0);

    let row = tx
        .query_one(
            "INSERT INTO note_revisions (organization_id, note_id, version, body_markdown, edited_by)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING id, note_id, version, body_markdown, edited_by, edited_at",
            &[&note.organization_id, &note.id, &next_version, &body_markdown, &edited_by],
        )
        .await?;

    tx.commit().await?;

    Ok(NoteRevision {
        id: row.get("id"),
        note_id: row.get("note_id"),
        version: row.get("version"),
        body_markdown: row.get("body_markdown"),
        edited_by: row.get("edited_by"),
        edited_at: row.get("edited_at"),
    })
}

/// Deletes a single saved version. Caller must already be authorized
/// (creator or admin) — enforced by the handler. Refuses to delete the last
/// remaining revision: a note always needs at least one version to anchor
/// against (both as a baseline snapshot and as the fallback
/// `in_reply_to_version` for replies made before any explicit save).
///
/// Any reply tagged with the deleted version's number is reassigned to the
/// nearest surviving version -- preferring the nearest *lower* one (the
/// reply was made while that version's body was live, so the closest
/// still-existing snapshot from before this deletion is the most accurate
/// remaining context), falling back to the nearest higher one if the
/// deleted version was the oldest surviving one. Without this, those
/// replies would become permanently unreachable from history browsing —
/// orphaned rows that just accumulate as noise.
pub async fn delete_revision(
    pool: &Pool,
    note: &Note,
    revision_id: Uuid,
) -> Result<(), AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    let count_row = tx
        .query_one(
            "SELECT COUNT(*) FROM note_revisions WHERE note_id = $1 AND organization_id = $2",
            &[&note.id, &note.organization_id],
        )
        .await?;
    let count: i64 = count_row.get(0);
    if count <= 1 {
        return Err(AppError::BadRequest("Can't delete the only remaining version.".into()));
    }

    let target_row = tx
        .query_opt(
            "SELECT version FROM note_revisions WHERE id = $1 AND note_id = $2 AND organization_id = $3",
            &[&revision_id, &note.id, &note.organization_id],
        )
        .await?;
    let Some(target_row) = target_row else {
        return Err(AppError::NotFound("Version not found.".into()));
    };
    let target_version: i32 = target_row.get(0);

    let reassign_row = tx
        .query_one(
            "SELECT COALESCE(
                (SELECT MAX(version) FROM note_revisions
                 WHERE note_id = $1 AND organization_id = $2 AND version < $3),
                (SELECT MIN(version) FROM note_revisions
                 WHERE note_id = $1 AND organization_id = $2 AND version > $3)
             )",
            &[&note.id, &note.organization_id, &target_version],
        )
        .await?;
    let reassign_to: i32 = reassign_row.get(0);

    tx.execute(
        "UPDATE notes SET in_reply_to_version = $1
         WHERE parent_id = $2 AND organization_id = $3 AND in_reply_to_version = $4",
        &[&reassign_to, &note.id, &note.organization_id, &target_version],
    )
    .await?;

    tx.execute(
        "DELETE FROM note_revisions WHERE id = $1 AND note_id = $2 AND organization_id = $3",
        &[&revision_id, &note.id, &note.organization_id],
    )
    .await?;

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
