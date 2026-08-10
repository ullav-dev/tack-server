use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use uuid::Uuid;

use crate::error::AppError;
use crate::models::note::{NoteRead, NoteUnreadStatus};

/// Upserts the caller's read marker for `note_id` to `NOW()` -- marking a
/// thread read again just moves the timestamp forward, same as re-reading
/// an already-read email.
pub async fn mark_read(pool: &Pool, note_id: Uuid, organization_id: Uuid, user_id: Uuid) -> Result<NoteRead, AppError> {
    let client = pool.get().await?;
    let row = client
        .query_one(
            "INSERT INTO note_reads (user_id, note_id, organization_id, read_at)
             VALUES ($1, $2, $3, NOW())
             ON CONFLICT (user_id, note_id, organization_id) DO UPDATE SET read_at = NOW()
             RETURNING note_id, read_at",
            &[&user_id, &note_id, &organization_id],
        )
        .await?;
    Ok(NoteRead { note_id: row.get("note_id"), read_at: row.get("read_at") })
}

/// Live unread status for a batch of top-level notes the caller can already
/// see (visibility is the handler's job, before calling this -- this
/// function trusts `note_ids` is pre-filtered). `last_activity_at` is the
/// note's own `updated_at`, or its latest non-deleted reply's, whichever is
/// later; `unread` is `true` when there's no read marker at all, or the
/// marker predates that activity.
pub async fn unread_status(
    pool: &Pool,
    note_ids: &[Uuid],
    organization_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<NoteUnreadStatus>, AppError> {
    if note_ids.is_empty() {
        return Ok(Vec::new());
    }
    let client = pool.get().await?;
    let rows = client
        .query(
            "SELECT n.id AS note_id,
                    GREATEST(
                        n.updated_at,
                        COALESCE((SELECT MAX(r.updated_at) FROM notes r
                                  WHERE r.parent_id = n.id AND r.organization_id = n.organization_id
                                    AND r.deleted_at IS NULL),
                                 n.updated_at)
                    ) AS last_activity_at,
                    nr.read_at
             FROM notes n
             LEFT JOIN note_reads nr
                    ON nr.note_id = n.id AND nr.organization_id = n.organization_id AND nr.user_id = $3
             WHERE n.id = ANY($1) AND n.organization_id = $2 AND n.deleted_at IS NULL",
            &[&note_ids, &organization_id, &user_id],
        )
        .await?;

    Ok(rows
        .iter()
        .map(|row| {
            let last_activity_at: DateTime<Utc> = row.get("last_activity_at");
            let read_at: Option<DateTime<Utc>> = row.get("read_at");
            NoteUnreadStatus {
                note_id: row.get("note_id"),
                unread: read_at.is_none_or(|read_at| read_at < last_activity_at),
                last_activity_at,
            }
        })
        .collect())
}
