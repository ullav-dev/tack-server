use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::TackUser;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::models::note::{CreateNoteRequest, Note, NoteRevision, NotesPage, ReplyRequest, UpdateNoteRequest};
use crate::notes_acl::{can_edit, resolve_team_organization, resolve_visible_note};
use crate::AppState;

#[utoipa::path(
    post,
    path = "/notes",
    request_body = CreateNoteRequest,
    responses((status = 201, description = "Note created", body = Note)),
    tag = "notes"
)]
pub async fn create_note(
    State(state): State<AppState>,
    user: TackUser,
    Json(body): Json<CreateNoteRequest>,
) -> AppResult<Json<Note>> {
    if body.title.trim().is_empty() {
        return Err(AppError::BadRequest("title must not be empty".into()));
    }
    if body.body_markdown.trim().is_empty() {
        return Err(AppError::BadRequest("body_markdown must not be empty".into()));
    }
    let organization_id = resolve_team_organization(&user, body.team_id)?;
    let note = db::notes::create_note(
        &state.db,
        db::notes::NewNote {
            organization_id,
            team_id: body.team_id,
            visibility: body.visibility,
            created_by: user.user_id,
            title: body.title,
            body_markdown: body.body_markdown,
        },
    )
    .await?;
    Ok(Json(note))
}

const DEFAULT_NOTES_LIMIT: i64 = 20;
const MAX_NOTES_LIMIT: i64 = 100;

#[derive(Debug, Deserialize)]
pub struct ListNotesQuery {
    pub team_id: Uuid,
    /// Defaults to 20, capped at 100.
    pub limit: Option<i64>,
    /// Defaults to 0.
    pub offset: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/notes",
    params(
        ("team_id" = Uuid, Query, description = "List top-level notes filed under this team"),
        ("limit" = Option<i64>, Query, description = "Page size, default 20, max 100"),
        ("offset" = Option<i64>, Query, description = "Offset into the (newest-first) list, default 0"),
    ),
    responses((status = 200, description = "A page of top-level notes for this team", body = NotesPage)),
    tag = "notes"
)]
pub async fn list_notes(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<ListNotesQuery>,
) -> AppResult<Json<NotesPage>> {
    let organization_id = resolve_team_organization(&user, query.team_id)?;
    let limit = query.limit.unwrap_or(DEFAULT_NOTES_LIMIT).clamp(1, MAX_NOTES_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);
    let notes =
        db::notes::list_team_notes(&state.db, organization_id, query.team_id, user.user_id, limit, offset).await?;
    Ok(Json(notes))
}

#[utoipa::path(
    get,
    path = "/notes/{id}",
    responses((status = 200, description = "A note", body = Note)),
    tag = "notes"
)]
pub async fn get_note(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Note>> {
    let note = resolve_visible_note(&state.db, &user, id).await?;
    Ok(Json(note))
}

#[utoipa::path(
    patch,
    path = "/notes/{id}",
    request_body = UpdateNoteRequest,
    responses((status = 200, description = "Updated note", body = Note)),
    tag = "notes"
)]
pub async fn update_note(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateNoteRequest>,
) -> AppResult<Json<Note>> {
    let note = resolve_visible_note(&state.db, &user, id).await?;
    if !can_edit(&note, &user) {
        return Err(AppError::Forbidden("Only the creator or an admin can edit this note.".into()));
    }
    if let Some(ref title) = body.title {
        if title.trim().is_empty() {
            return Err(AppError::BadRequest("title must not be empty".into()));
        }
    }
    if let Some(ref body_markdown) = body.body_markdown {
        if body_markdown.trim().is_empty() {
            return Err(AppError::BadRequest("body_markdown must not be empty".into()));
        }
    }
    let updated =
        db::notes::update_note(&state.db, &note, body.title.as_deref(), body.body_markdown.as_deref(), body.visibility)
            .await?;
    Ok(Json(updated))
}

#[utoipa::path(
    delete,
    path = "/notes/{id}",
    responses((status = 204, description = "Note soft-deleted")),
    tag = "notes"
)]
pub async fn delete_note(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<axum::http::StatusCode> {
    let note = resolve_visible_note(&state.db, &user, id).await?;
    if !can_edit(&note, &user) {
        return Err(AppError::Forbidden("Only the creator or an admin can delete this note.".into()));
    }
    db::notes::soft_delete_note(&state.db, &note).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/notes/{id}/replies",
    request_body = ReplyRequest,
    responses((status = 201, description = "Reply created", body = Note)),
    tag = "notes"
)]
pub async fn create_reply(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<ReplyRequest>,
) -> AppResult<Json<Note>> {
    if body.body_markdown.trim().is_empty() {
        return Err(AppError::BadRequest("body_markdown must not be empty".into()));
    }
    let parent = resolve_visible_note(&state.db, &user, id).await?;
    let reply = db::notes::create_reply(&state.db, &parent, user.user_id, &body.body_markdown).await?;
    Ok(Json(reply))
}

#[utoipa::path(
    get,
    path = "/notes/{id}/replies",
    responses((status = 200, description = "Replies to a note, oldest first", body = [Note])),
    tag = "notes"
)]
pub async fn list_replies(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<Note>>> {
    let parent = resolve_visible_note(&state.db, &user, id).await?;
    let replies = db::notes::list_replies(&state.db, parent.id, parent.organization_id).await?;
    Ok(Json(replies))
}

#[utoipa::path(
    get,
    path = "/notes/{id}/revisions",
    responses((status = 200, description = "Revision history, newest first", body = [NoteRevision])),
    tag = "notes"
)]
pub async fn list_revisions(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<NoteRevision>>> {
    let note = resolve_visible_note(&state.db, &user, id).await?;
    let revisions = db::notes::list_revisions(&state.db, note.id, note.organization_id).await?;
    Ok(Json(revisions))
}

/// Snapshots the note's current body as a new named version — a deliberate
/// action, not an automatic side effect of every `PATCH` (see
/// `db::notes::create_revision`).
#[utoipa::path(
    post,
    path = "/notes/{id}/revisions",
    responses((status = 201, description = "New version created", body = NoteRevision)),
    tag = "notes"
)]
pub async fn create_revision(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<NoteRevision>> {
    let note = resolve_visible_note(&state.db, &user, id).await?;
    if !can_edit(&note, &user) {
        return Err(AppError::Forbidden("Only the creator or an admin can create a version of this note.".into()));
    }
    let revision = db::notes::create_revision(&state.db, &note, user.user_id).await?;
    Ok(Json(revision))
}

