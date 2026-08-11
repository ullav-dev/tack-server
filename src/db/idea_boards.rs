use chrono::{DateTime, Utc};
use deadpool_postgres::Pool;
use tokio_postgres::Row;
use uuid::Uuid;

use crate::error::AppError;
use crate::models::idea_board::{BoardShape, IdeaBoard, NoteLink, Sticky};

use super::note_folders::{row_to_folder, FOLDER_SELECT};
use super::notes::{enqueue_outbox_event, insert_body_and_first_revision, ltree_label};

// ── Boards ───────────────────────────────────────────────────────────────
// A board is just a `note_folders` row with `folder_type = 'ideas_board'`
// -- create/get/rename reuse `db::note_folders`'s own functions directly
// (`handlers::idea_boards` passes `FolderType::IdeasBoard` to `create_folder`
// and re-validates `folder_type` on every resolved folder before treating it
// as a board). Only listing (scoped to boards only, not general folders) and
// deletion (cascading through stickies/shapes/links, not just unfiling notes)
// need board-specific queries, below.

/// Same shape as `db::note_folders::list_folders_for_teams`, scoped to
/// `folder_type = 'ideas_board'` and *not* excluding entity-scoped rows
/// (`owning_service IS NOT NULL`) -- unlike the general Notes folder list, a
/// board legitimately is often entity-scoped (attached to the togra project
/// or cunav ticket it belongs to) and should still show up in `GET
/// /idea-boards`.
pub async fn list_boards_for_teams(
    pool: &Pool,
    organization_id: Uuid,
    team_ids: &[Uuid],
    caller_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<IdeaBoard>, i64), AppError> {
    let client = pool.get().await?;
    let sql = format!(
        "{} WHERE f.organization_id = $1 AND f.team_id = ANY($2) AND f.folder_type = 'ideas_board'
         ORDER BY lower(f.name) ASC LIMIT $4 OFFSET $5",
        FOLDER_SELECT.replace("{caller_id}", "$3")
    );
    let rows = client.query(&sql, &[&organization_id, &team_ids, &caller_id, &limit, &offset]).await?;
    let boards = rows.iter().map(row_to_folder).collect();

    let total: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM note_folders WHERE organization_id = $1 AND team_id = ANY($2) AND folder_type = 'ideas_board'",
            &[&organization_id, &team_ids],
        )
        .await?
        .get(0);

    Ok((boards, total))
}

/// Deletes a board: every sticky's underlying note (hard-deleted, not soft --
/// a board and its stickies are ephemeral canvas state, not durable Notes
/// content; there's no version-history/export UI for a sticky the way there
/// is for a regular note), every shape, every link, then the folder row
/// itself. All in one transaction, mirroring awe-server's own
/// `delete_board`'s "delete the notes under the board's stickies first"
/// shape, extended to also cover shapes/links (which awe-server keeps in
/// tables with no cascade-on-board-delete of their own either).
pub async fn delete_board(pool: &Pool, board: &IdeaBoard) -> Result<(), AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;
    // note_links, board_shapes, and idea_board_stickies all carry
    // `ON DELETE CASCADE` FKs to note_folders (see 011_idea_boards.sql), so
    // deleting the folder row would clean those up on its own -- but a
    // sticky's *note* row has no such FK to note_folders (only to
    // idea_board_stickies, which cascades to the note via ON DELETE CASCADE
    // in the other direction: note deleted -> sticky row deleted, not
    // sticky row deleted -> note deleted). So the notes must be deleted
    // explicitly first, same as awe-server's own handler does.
    tx.execute(
        "DELETE FROM notes WHERE organization_id = $1
         AND id IN (SELECT note_id FROM idea_board_stickies WHERE board_id = $2 AND organization_id = $1)",
        &[&board.organization_id, &board.id],
    )
    .await?;
    // Defense in depth: `handlers::note_folders::check_folder_in_team`
    // rejects filing an ordinary note into a board folder going forward,
    // but `notes_folder_id_fkey` carries no `ON DELETE` action (plain
    // RESTRICT) -- if any note ever ends up with `folder_id = board.id`
    // without a matching `idea_board_stickies` row (a bug elsewhere, or
    // data that predates that check), unfile it rather than let the folder
    // delete below fail with an opaque FK-violation 500. Same "unfile,
    // don't delete" behavior `db::note_folders::delete_folder` already uses
    // for a plain folder's leftover notes.
    tx.execute(
        "UPDATE notes SET folder_id = NULL, updated_at = NOW() WHERE folder_id = $1 AND organization_id = $2",
        &[&board.id, &board.organization_id],
    )
    .await?;
    let deleted = tx
        .execute("DELETE FROM note_folders WHERE id = $1 AND organization_id = $2", &[&board.id, &board.organization_id])
        .await?;
    if deleted == 0 {
        return Err(AppError::NotFound("Board not found.".into()));
    }
    tx.commit().await?;
    Ok(())
}

// ── Stickies ─────────────────────────────────────────────────────────────

const STICKY_SELECT: &str = "
    SELECT n.id AS note_id, s.board_id, n.organization_id, n.title, b.body_markdown,
           n.created_by, n.created_at, n.updated_at,
           s.x, s.y, s.color, s.width, s.height, s.linked_entity_type, s.linked_entity_id
    FROM idea_board_stickies s
    JOIN notes n ON n.id = s.note_id AND n.organization_id = s.organization_id
    JOIN note_bodies b ON b.note_id = n.id AND b.organization_id = n.organization_id
";

fn row_to_sticky(row: &Row) -> Sticky {
    Sticky {
        note_id: row.get("note_id"),
        board_id: row.get("board_id"),
        organization_id: row.get("organization_id"),
        title: row.get("title"),
        body_markdown: row.get("body_markdown"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        x: row.get("x"),
        y: row.get("y"),
        color: row.get("color"),
        width: row.get("width"),
        height: row.get("height"),
        linked_entity_type: row.get("linked_entity_type"),
        linked_entity_id: row.get("linked_entity_id"),
    }
}

pub struct NewSticky {
    pub title: String,
    pub body_markdown: String,
    pub x: f64,
    pub y: f64,
    pub color: String,
    pub width: f64,
    pub height: f64,
    pub linked_entity_type: Option<String>,
    pub linked_entity_id: Option<String>,
}

/// Creates a sticky's underlying note (title/body/first revision/outbox
/// event, same as `db::notes::create_note`, deliberately not reusing that
/// function since a sticky's note is `folder_id = board.id` but never
/// visibility-gated the way an ordinary note is -- see
/// `handlers::idea_boards`' doc comment on why sticky ACL is
/// board-team-membership, not `notes_acl`) plus its `idea_board_stickies`
/// layout row, in one transaction.
pub async fn create_sticky(
    pool: &Pool,
    board: &IdeaBoard,
    new: NewSticky,
    created_by: Uuid,
    created_at: DateTime<Utc>,
) -> Result<Sticky, AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    let note_id = Uuid::new_v4();
    let thread_path = ltree_label(note_id);

    tx.execute(
        "INSERT INTO notes (id, organization_id, team_id, thread_path, visibility, title, created_by, folder_id, created_at, updated_at)
         VALUES ($1, $2, $3, $4::ltree, 'team', $5, $6, $7, $8, $8)",
        &[&note_id, &board.organization_id, &board.team_id, &thread_path, &new.title, &created_by, &board.id, &created_at],
    )
    .await?;

    insert_body_and_first_revision(&tx, note_id, board.organization_id, &new.body_markdown, created_by, created_at).await?;

    tx.execute(
        "INSERT INTO idea_board_stickies (board_id, note_id, organization_id, x, y, color, width, height, linked_entity_type, linked_entity_id)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        &[
            &board.id,
            &note_id,
            &board.organization_id,
            &new.x,
            &new.y,
            &new.color,
            &new.width,
            &new.height,
            &new.linked_entity_type,
            &new.linked_entity_id,
        ],
    )
    .await?;

    enqueue_outbox_event(&tx, board.organization_id, note_id, "created").await?;

    tx.commit().await?;

    get_sticky(pool, note_id, board.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("sticky {note_id} vanished immediately after insert"))
    })
}

/// A note is only ever a sticky on the one board it's filed in, so
/// `note_id`/`organization_id` alone resolves it -- no `board_id` needed
/// (see `013_idea_board_sticky_indexes.sql`).
pub async fn get_sticky(pool: &Pool, note_id: Uuid, organization_id: Uuid) -> Result<Option<Sticky>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{STICKY_SELECT} WHERE n.id = $1 AND n.organization_id = $2 AND n.deleted_at IS NULL");
    let row = client.query_opt(&sql, &[&note_id, &organization_id]).await?;
    Ok(row.as_ref().map(row_to_sticky))
}

pub async fn list_stickies_for_board(
    pool: &Pool,
    board_id: Uuid,
    organization_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Sticky>, i64), AppError> {
    let client = pool.get().await?;
    let sql = format!(
        "{STICKY_SELECT} WHERE s.board_id = $1 AND s.organization_id = $2 AND n.deleted_at IS NULL
         ORDER BY n.created_at ASC LIMIT $3 OFFSET $4"
    );
    let rows = client.query(&sql, &[&board_id, &organization_id, &limit, &offset]).await?;
    let stickies = rows.iter().map(row_to_sticky).collect();

    let total: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM idea_board_stickies s JOIN notes n ON n.id = s.note_id AND n.organization_id = s.organization_id
             WHERE s.board_id = $1 AND s.organization_id = $2 AND n.deleted_at IS NULL",
            &[&board_id, &organization_id],
        )
        .await?
        .get(0);

    Ok((stickies, total))
}

/// Scans the caller's organizations (same shape as
/// `db::notes::list_notes_by_attachment`) for a sticky soft-linked to the
/// given external entity -- the tack-server equivalent of awe-server's
/// `get_sticky_by_workflow`, generalized to any `linked_entity_type`.
pub async fn get_sticky_by_entity(
    pool: &Pool,
    organization_id: Uuid,
    linked_entity_type: &str,
    linked_entity_id: &str,
) -> Result<Option<Sticky>, AppError> {
    let client = pool.get().await?;
    let sql = format!(
        "{STICKY_SELECT} WHERE s.organization_id = $1 AND s.linked_entity_type = $2 AND s.linked_entity_id = $3
         AND n.deleted_at IS NULL"
    );
    let row = client.query_opt(&sql, &[&organization_id, &linked_entity_type, &linked_entity_id]).await?;
    Ok(row.as_ref().map(row_to_sticky))
}

pub struct StickyUpdate {
    pub title: Option<String>,
    pub body_markdown: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub color: Option<String>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub linked_entity_type: Option<String>,
    pub linked_entity_id: Option<String>,
}

/// Idea boards are collaborative -- any board-team member may update any
/// sticky (see `handlers::idea_boards`'s doc comment), so unlike
/// `db::notes::update_note` this never creates a new revision or checks
/// `notes_acl::can_edit`; it's a plain field-level update of both the note's
/// content columns and the sticky's layout columns, in one transaction.
pub async fn update_sticky(pool: &Pool, sticky: &Sticky, edited_by: Uuid, update: StickyUpdate) -> Result<Sticky, AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;

    if let Some(title) = &update.title {
        tx.execute(
            "UPDATE notes SET title = $1, updated_at = NOW() WHERE id = $2 AND organization_id = $3",
            &[title, &sticky.note_id, &sticky.organization_id],
        )
        .await?;
    }
    if let Some(body_markdown) = &update.body_markdown {
        tx.execute(
            "UPDATE note_bodies SET body_markdown = $1 WHERE note_id = $2 AND organization_id = $3",
            &[body_markdown, &sticky.note_id, &sticky.organization_id],
        )
        .await?;
        tx.execute(
            "UPDATE notes SET updated_at = NOW() WHERE id = $1 AND organization_id = $2",
            &[&sticky.note_id, &sticky.organization_id],
        )
        .await?;
    }

    tx.execute(
        "UPDATE idea_board_stickies SET
            x = COALESCE($1, x), y = COALESCE($2, y), color = COALESCE($3, color),
            width = COALESCE($4, width), height = COALESCE($5, height),
            linked_entity_type = COALESCE($6, linked_entity_type), linked_entity_id = COALESCE($7, linked_entity_id)
         WHERE board_id = $8 AND note_id = $9 AND organization_id = $10",
        &[
            &update.x,
            &update.y,
            &update.color,
            &update.width,
            &update.height,
            &update.linked_entity_type,
            &update.linked_entity_id,
            &sticky.board_id,
            &sticky.note_id,
            &sticky.organization_id,
        ],
    )
    .await?;

    enqueue_outbox_event(&tx, sticky.organization_id, sticky.note_id, "updated").await?;
    tx.commit().await?;

    let _ = edited_by; // no notes_acl involved -- see doc comment above; kept for a future audit trail
    get_sticky(pool, sticky.note_id, sticky.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("sticky {} vanished immediately after update", sticky.note_id))
    })
}

/// Hard-deletes the sticky's `idea_board_stickies` row and its note (see
/// `delete_board`'s doc comment on why stickies are hard- not soft-deleted).
pub async fn delete_sticky(pool: &Pool, sticky: &Sticky) -> Result<(), AppError> {
    let mut client = pool.get().await?;
    let tx = client.transaction().await?;
    tx.execute(
        "DELETE FROM idea_board_stickies WHERE board_id = $1 AND note_id = $2 AND organization_id = $3",
        &[&sticky.board_id, &sticky.note_id, &sticky.organization_id],
    )
    .await?;
    let deleted = tx
        .execute("DELETE FROM notes WHERE id = $1 AND organization_id = $2", &[&sticky.note_id, &sticky.organization_id])
        .await?;
    if deleted == 0 {
        return Err(AppError::NotFound("Sticky not found.".into()));
    }
    tx.commit().await?;
    Ok(())
}

// ── Shapes ───────────────────────────────────────────────────────────────

const SHAPE_SELECT: &str = "
    SELECT id, board_id, organization_id, shape_type, x, y, width, height, fill_color, stroke_color,
           stroke_width, label, label_color, label_size, image_url, created_by, created_at, updated_at
    FROM board_shapes
";

fn row_to_shape(row: &Row) -> BoardShape {
    BoardShape {
        id: row.get("id"),
        board_id: row.get("board_id"),
        organization_id: row.get("organization_id"),
        shape_type: row.get("shape_type"),
        x: row.get("x"),
        y: row.get("y"),
        width: row.get("width"),
        height: row.get("height"),
        fill_color: row.get("fill_color"),
        stroke_color: row.get("stroke_color"),
        stroke_width: row.get("stroke_width"),
        label: row.get("label"),
        label_color: row.get("label_color"),
        label_size: row.get::<_, i32>("label_size") as f64,
        image_url: row.get("image_url"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

pub struct NewShape {
    pub shape_type: String,
    pub x: f64,
    pub y: f64,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub fill_color: Option<String>,
    pub stroke_color: Option<String>,
    pub stroke_width: Option<f64>,
    pub label: Option<String>,
    pub label_color: Option<String>,
    pub label_size: Option<f64>,
    pub image_url: Option<String>,
}

// Mirrors `board_shapes`' own column DEFAULTs (see `011_idea_boards.sql`) --
// applied in Rust, not via a SQL `DEFAULT` keyword, since `DEFAULT` can't
// appear inside a `COALESCE(...)` expression the way a plain literal can.
const DEFAULT_SHAPE_WIDTH: f64 = 160.0;
const DEFAULT_SHAPE_HEIGHT: f64 = 100.0;
const DEFAULT_SHAPE_FILL_COLOR: &str = "#ffffff";
const DEFAULT_SHAPE_STROKE_COLOR: &str = "#64748b";
const DEFAULT_SHAPE_STROKE_WIDTH: f64 = 2.0;
const DEFAULT_SHAPE_LABEL_COLOR: &str = "#1e293b";
const DEFAULT_SHAPE_LABEL_SIZE: i32 = 13;

pub async fn create_shape(
    pool: &Pool,
    board: &IdeaBoard,
    new: NewShape,
    created_by: Uuid,
    created_at: DateTime<Utc>,
) -> Result<BoardShape, AppError> {
    let client = pool.get().await?;
    let width = new.width.unwrap_or(DEFAULT_SHAPE_WIDTH);
    let height = new.height.unwrap_or(DEFAULT_SHAPE_HEIGHT);
    let fill_color = new.fill_color.unwrap_or_else(|| DEFAULT_SHAPE_FILL_COLOR.to_string());
    let stroke_color = new.stroke_color.unwrap_or_else(|| DEFAULT_SHAPE_STROKE_COLOR.to_string());
    let stroke_width = new.stroke_width.unwrap_or(DEFAULT_SHAPE_STROKE_WIDTH);
    let label_color = new.label_color.unwrap_or_else(|| DEFAULT_SHAPE_LABEL_COLOR.to_string());
    let label_size = new.label_size.map(|v| v as i32).unwrap_or(DEFAULT_SHAPE_LABEL_SIZE);
    let row = client
        .query_one(
            "INSERT INTO board_shapes
                (organization_id, board_id, shape_type, x, y,
                 width, height, fill_color, stroke_color, stroke_width,
                 label, label_color, label_size, image_url, created_by, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $16)
             RETURNING id",
            &[
                &board.organization_id,
                &board.id,
                &new.shape_type,
                &new.x,
                &new.y,
                &width,
                &height,
                &fill_color,
                &stroke_color,
                &stroke_width,
                &new.label,
                &label_color,
                &label_size,
                &new.image_url,
                &created_by,
                &created_at,
            ],
        )
        .await?;
    let id: Uuid = row.get("id");
    get_shape(pool, id, board.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("shape {id} vanished immediately after insert"))
    })
}

pub async fn get_shape(pool: &Pool, id: Uuid, organization_id: Uuid) -> Result<Option<BoardShape>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{SHAPE_SELECT} WHERE id = $1 AND organization_id = $2");
    let row = client.query_opt(&sql, &[&id, &organization_id]).await?;
    Ok(row.as_ref().map(row_to_shape))
}

pub async fn list_shapes_for_board(
    pool: &Pool,
    board_id: Uuid,
    organization_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<BoardShape>, i64), AppError> {
    let client = pool.get().await?;
    let sql = format!("{SHAPE_SELECT} WHERE board_id = $1 AND organization_id = $2 ORDER BY created_at ASC LIMIT $3 OFFSET $4");
    let rows = client.query(&sql, &[&board_id, &organization_id, &limit, &offset]).await?;
    let shapes = rows.iter().map(row_to_shape).collect();

    let total: i64 = client
        .query_one(
            "SELECT COUNT(*) FROM board_shapes WHERE board_id = $1 AND organization_id = $2",
            &[&board_id, &organization_id],
        )
        .await?
        .get(0);

    Ok((shapes, total))
}

/// `label`: `Some(None)` (JSON `null`) clears the label, `Some(Some(v))`
/// sets it, `None` (omitted) leaves it unchanged -- same tri-state pattern
/// `UpdateNoteRequest::folder_id` uses, needed here because "no label" is a
/// real, distinct state from "don't touch the label."
pub struct ShapeUpdate {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub fill_color: Option<String>,
    pub stroke_color: Option<String>,
    pub stroke_width: Option<f64>,
    pub label: Option<Option<String>>,
    pub label_color: Option<String>,
    pub label_size: Option<f64>,
    pub image_url: Option<Option<String>>,
}

pub async fn update_shape(pool: &Pool, shape: &BoardShape, update: ShapeUpdate) -> Result<BoardShape, AppError> {
    let client = pool.get().await?;
    let label_size = update.label_size.map(|v| v as i32);
    client
        .execute(
            "UPDATE board_shapes SET
                x = COALESCE($1, x), y = COALESCE($2, y), width = COALESCE($3, width), height = COALESCE($4, height),
                fill_color = COALESCE($5, fill_color), stroke_color = COALESCE($6, stroke_color),
                stroke_width = COALESCE($7, stroke_width),
                label = CASE WHEN $8 THEN $9 ELSE label END,
                label_color = COALESCE($10, label_color), label_size = COALESCE($11, label_size),
                image_url = CASE WHEN $12 THEN $13 ELSE image_url END,
                updated_at = NOW()
             WHERE id = $14 AND organization_id = $15",
            &[
                &update.x,
                &update.y,
                &update.width,
                &update.height,
                &update.fill_color,
                &update.stroke_color,
                &update.stroke_width,
                &update.label.is_some(),
                &update.label.flatten(),
                &update.label_color,
                &label_size,
                &update.image_url.is_some(),
                &update.image_url.flatten(),
                &shape.id,
                &shape.organization_id,
            ],
        )
        .await?;
    get_shape(pool, shape.id, shape.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("shape {} vanished immediately after update", shape.id))
    })
}

pub async fn delete_shape(pool: &Pool, shape: &BoardShape) -> Result<(), AppError> {
    let client = pool.get().await?;
    let deleted = client
        .execute("DELETE FROM board_shapes WHERE id = $1 AND organization_id = $2", &[&shape.id, &shape.organization_id])
        .await?;
    if deleted == 0 {
        return Err(AppError::NotFound("Shape not found.".into()));
    }
    Ok(())
}

// ── Links ────────────────────────────────────────────────────────────────

const LINK_SELECT: &str = "
    SELECT id, board_id, organization_id, from_note_id, to_note_id, from_shape_id, to_shape_id,
           from_port, to_port, label, created_by, created_at
    FROM note_links
";

fn row_to_link(row: &Row) -> NoteLink {
    NoteLink {
        id: row.get("id"),
        board_id: row.get("board_id"),
        organization_id: row.get("organization_id"),
        from_note_id: row.get("from_note_id"),
        to_note_id: row.get("to_note_id"),
        from_shape_id: row.get("from_shape_id"),
        to_shape_id: row.get("to_shape_id"),
        from_port: row.get("from_port"),
        to_port: row.get("to_port"),
        label: row.get("label"),
        created_by: row.get("created_by"),
        created_at: row.get("created_at"),
    }
}

pub struct NewLink {
    pub from_note_id: Option<Uuid>,
    pub from_shape_id: Option<Uuid>,
    pub to_note_id: Option<Uuid>,
    pub to_shape_id: Option<Uuid>,
    pub from_port: Option<String>,
    pub to_port: Option<String>,
    pub label: Option<String>,
}

/// Catches the `note_links_from_note_id_to_note_id_organization_id_key`
/// unique-constraint violation (Postgres SQLSTATE 23505) to return a clear
/// 400 instead of a raw DB error -- mirrors awe-server's own `create_link`.
pub async fn create_link(
    pool: &Pool,
    board: &IdeaBoard,
    new: NewLink,
    created_by: Uuid,
    created_at: DateTime<Utc>,
) -> Result<NoteLink, AppError> {
    let client = pool.get().await?;
    let result = client
        .query_one(
            "INSERT INTO note_links
                (organization_id, board_id, from_note_id, to_note_id, from_shape_id, to_shape_id, from_port, to_port, label, created_by, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             RETURNING id",
            &[
                &board.organization_id,
                &board.id,
                &new.from_note_id,
                &new.to_note_id,
                &new.from_shape_id,
                &new.to_shape_id,
                &new.from_port,
                &new.to_port,
                &new.label,
                &created_by,
                &created_at,
            ],
        )
        .await;
    let row = match result {
        Ok(row) => row,
        Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) => {
            return Err(AppError::BadRequest("A link between these endpoints already exists.".into()));
        }
        Err(e) => return Err(e.into()),
    };
    let id: Uuid = row.get("id");
    get_link(pool, id, board.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("link {id} vanished immediately after insert"))
    })
}

pub async fn get_link(pool: &Pool, id: Uuid, organization_id: Uuid) -> Result<Option<NoteLink>, AppError> {
    let client = pool.get().await?;
    let sql = format!("{LINK_SELECT} WHERE id = $1 AND organization_id = $2");
    let row = client.query_opt(&sql, &[&id, &organization_id]).await?;
    Ok(row.as_ref().map(row_to_link))
}

/// O(1) via the denormalized `board_id` column -- see `011_idea_boards.sql`'s
/// header comment on why that redundancy is worth it.
pub async fn list_links_for_board(
    pool: &Pool,
    board_id: Uuid,
    organization_id: Uuid,
    limit: i64,
    offset: i64,
) -> Result<(Vec<NoteLink>, i64), AppError> {
    let client = pool.get().await?;
    let sql = format!("{LINK_SELECT} WHERE board_id = $1 AND organization_id = $2 ORDER BY created_at ASC LIMIT $3 OFFSET $4");
    let rows = client.query(&sql, &[&board_id, &organization_id, &limit, &offset]).await?;
    let links = rows.iter().map(row_to_link).collect();

    let total: i64 = client
        .query_one("SELECT COUNT(*) FROM note_links WHERE board_id = $1 AND organization_id = $2", &[&board_id, &organization_id])
        .await?
        .get(0);

    Ok((links, total))
}

pub struct LinkUpdate {
    pub from_port: Option<Option<String>>,
    pub to_port: Option<Option<String>>,
    pub label: Option<Option<String>>,
}

pub async fn update_link(pool: &Pool, link: &NoteLink, update: LinkUpdate) -> Result<NoteLink, AppError> {
    let client = pool.get().await?;
    client
        .execute(
            "UPDATE note_links SET
                from_port = CASE WHEN $1 THEN $2 ELSE from_port END,
                to_port = CASE WHEN $3 THEN $4 ELSE to_port END,
                label = CASE WHEN $5 THEN $6 ELSE label END
             WHERE id = $7 AND organization_id = $8",
            &[
                &update.from_port.is_some(),
                &update.from_port.clone().flatten(),
                &update.to_port.is_some(),
                &update.to_port.clone().flatten(),
                &update.label.is_some(),
                &update.label.flatten(),
                &link.id,
                &link.organization_id,
            ],
        )
        .await?;
    get_link(pool, link.id, link.organization_id).await?.ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("link {} vanished immediately after update", link.id))
    })
}

pub async fn delete_link(pool: &Pool, link: &NoteLink) -> Result<(), AppError> {
    let client = pool.get().await?;
    let deleted = client
        .execute("DELETE FROM note_links WHERE id = $1 AND organization_id = $2", &[&link.id, &link.organization_id])
        .await?;
    if deleted == 0 {
        return Err(AppError::NotFound("Link not found.".into()));
    }
    Ok(())
}

/// Resolves which board a link endpoint (a note-or-shape id) belongs to, so
/// `handlers::idea_boards::create_link` can reject an endpoint from a
/// different board with a clear message instead of a confusing FK error --
/// mirrors awe-server's own per-endpoint "belongs to board" lookup.
pub async fn note_belongs_to_board(pool: &Pool, note_id: Uuid, organization_id: Uuid, board_id: Uuid) -> Result<bool, AppError> {
    let client = pool.get().await?;
    let row = client
        .query_opt(
            "SELECT 1 FROM idea_board_stickies WHERE note_id = $1 AND organization_id = $2 AND board_id = $3",
            &[&note_id, &organization_id, &board_id],
        )
        .await?;
    Ok(row.is_some())
}

pub async fn shape_belongs_to_board(pool: &Pool, shape_id: Uuid, organization_id: Uuid, board_id: Uuid) -> Result<bool, AppError> {
    let client = pool.get().await?;
    let row = client
        .query_opt(
            "SELECT 1 FROM board_shapes WHERE id = $1 AND organization_id = $2 AND board_id = $3",
            &[&shape_id, &organization_id, &board_id],
        )
        .await?;
    Ok(row.is_some())
}
