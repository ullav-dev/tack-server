use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{RawBearerToken, TackUser};
use crate::db;
use crate::db::notes::FolderScope;
use crate::error::{AppError, AppResult};
use crate::handlers::note_folders::check_folder_in_team;
use crate::models::note::{
    AttachRequest, CreateNoteRequest, Note, NoteAttachment, NoteRead, NoteRevision, NoteUnreadStatus, NotesPage, ReplyRequest,
    UpdateNoteRequest,
};
use crate::notes_acl::{can_edit, resolve_team_organization, resolve_team_organization_live, resolve_visible_note};
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
    RawBearerToken(raw_token): RawBearerToken,
    Json(body): Json<CreateNoteRequest>,
) -> AppResult<Json<Note>> {
    if body.title.trim().is_empty() {
        return Err(AppError::BadRequest("title must not be empty".into()));
    }
    if body.body_markdown.trim().is_empty() {
        return Err(AppError::BadRequest("body_markdown must not be empty".into()));
    }
    // Live-resolution fallback only actually does anything (a network call)
    // for an admin caller on a team outside their own JWT claims -- see
    // `resolve_team_organization_live`'s doc comment. Every other caller
    // takes the exact same fast JWT-only path `resolve_team_organization`
    // always did.
    let organization_id = resolve_team_organization_live(&state, &user, &raw_token, body.team_id).await?;
    if let Some(folder_id) = body.folder_id {
        let attachments: Vec<(&str, &str, &str)> = body
            .attach
            .as_ref()
            .map(|a| vec![(a.owning_service.as_str(), a.entity_type.as_str(), a.entity_id.as_str())])
            .unwrap_or_default();
        check_folder_in_team(&state, organization_id, body.team_id, folder_id, user.user_id, &attachments).await?;
    }
    // Backfill-only: only an admin caller's created_at override is honored,
    // so an ordinary API consumer can never backdate a note.
    let created_at = if user.is_admin { body.created_at } else { None };
    // created_by: an admin may attribute a note to anyone (backfill). A
    // non-admin caller (e.g. a service account driving AI triage or
    // inbound-email ingestion, neither of which should need full admin just
    // to post as a bot) may attribute a note to an *existing system
    // principal in this same organization* -- verified by a real DB lookup,
    // not trusted from the request body, so this can never be used to
    // impersonate an arbitrary human's UUID. Any other claimed created_by
    // from a non-admin is silently ignored (falls back to the caller's own
    // id), matching this endpoint's behavior before system principals
    // existed at all.
    let created_by = if user.is_admin {
        body.created_by.unwrap_or(user.user_id)
    } else if let Some(claimed) = body.created_by {
        match db::system_principals::get_principal(&state.db, claimed, organization_id).await? {
            Some(_) => claimed,
            None => user.user_id,
        }
    } else {
        user.user_id
    };
    let note = db::notes::create_note(
        &state.db,
        db::notes::NewNote {
            organization_id,
            team_id: body.team_id,
            visibility: body.visibility,
            created_by,
            title: body.title,
            body_markdown: body.body_markdown,
            folder_id: body.folder_id,
            attach: body.attach.map(|a| db::notes::NewAttachment {
                owning_service: a.owning_service,
                entity_type: a.entity_type,
                entity_id: a.entity_id,
            }),
            created_at,
        },
    )
    .await?;
    Ok(Json(note))
}

#[derive(Debug, Deserialize)]
pub struct ListNotesByAttachmentQuery {
    pub owning_service: String,
    pub entity_type: String,
    pub entity_id: String,
}

/// Top-level notes attached to an external entity (e.g. a lagan pull
/// request's discussion thread), oldest-first — see
/// `db::notes::list_notes_by_attachment`. Scoped to the caller's own
/// organizations: since `content_attachments` carries no visibility of its
/// own, this walks every one of the caller's Tack-enabled-team organizations
/// and returns the first org with any *visible* rows — an entity's notes
/// only ever belong to one organization in practice (an attachment is
/// created once, by whichever team's note it is), so this isn't a fan-out
/// concern, just a scan over a typically-small list. The DB query itself is
/// visibility-blind (it only scopes by organization_id, same as `get_note`),
/// so every row is run through `notes_acl::can_view` before being counted —
/// otherwise another team's `private` or `team`-visibility note attached to
/// the same entity would leak to any member of the organization, unlike
/// `GET /notes` which already excludes other people's private notes at the
/// SQL level. Critically, the visibility filter runs *before* the
/// early-return check: an org whose only attached rows are all invisible to
/// this caller must not stop the scan, or a caller in two orgs would get an
/// empty result instead of falling through to the org that actually holds a
/// note they can see.
#[utoipa::path(
    get,
    path = "/notes/by-entity",
    params(
        ("owning_service" = String, Query, description = "Namespacing service, e.g. \"lagan\""),
        ("entity_type" = String, Query, description = "e.g. \"pull_request\""),
        ("entity_id" = String, Query, description = "The external entity's own id"),
    ),
    responses((status = 200, description = "Top-level notes attached to this entity, oldest first", body = [Note])),
    tag = "notes"
)]
pub async fn list_notes_by_attachment(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<ListNotesByAttachmentQuery>,
) -> AppResult<Json<Vec<Note>>> {
    for organization_id in user.organization_ids() {
        let notes = db::notes::list_notes_by_attachment(
            &state.db,
            organization_id,
            &query.owning_service,
            &query.entity_type,
            &query.entity_id,
        )
        .await?;
        let visible: Vec<_> = notes.into_iter().filter(|n| crate::notes_acl::can_view(n, &user)).collect();
        if !visible.is_empty() {
            return Ok(Json(visible));
        }
    }
    Ok(Json(Vec::new()))
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
    /// Only notes filed in this folder. Omit (along with `unfiled`) for the
    /// original, unfiltered behavior -- every note in the team regardless of
    /// folder. Mutually exclusive with `unfiled`.
    pub folder_id: Option<Uuid>,
    /// Only notes with no folder at all. Mutually exclusive with `folder_id`.
    #[serde(default)]
    pub unfiled: bool,
}

#[utoipa::path(
    get,
    path = "/notes",
    params(
        ("team_id" = Uuid, Query, description = "List top-level notes filed under this team"),
        ("limit" = Option<i64>, Query, description = "Page size, default 20, max 100"),
        ("offset" = Option<i64>, Query, description = "Offset into the (newest-first) list, default 0"),
        ("folder_id" = Option<Uuid>, Query, description = "Only notes filed in this folder"),
        ("unfiled" = Option<bool>, Query, description = "Only notes with no folder"),
    ),
    responses((status = 200, description = "A page of top-level notes for this team", body = NotesPage)),
    tag = "notes"
)]
pub async fn list_notes(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<ListNotesQuery>,
) -> AppResult<Json<NotesPage>> {
    if query.folder_id.is_some() && query.unfiled {
        return Err(AppError::BadRequest("folder_id and unfiled are mutually exclusive.".into()));
    }
    let organization_id = resolve_team_organization(&user, query.team_id)?;
    let limit = query.limit.unwrap_or(DEFAULT_NOTES_LIMIT).clamp(1, MAX_NOTES_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);
    let folder = match (query.folder_id, query.unfiled) {
        (Some(id), _) => Some(FolderScope::Folder(id)),
        (None, true) => Some(FolderScope::Unfiled),
        (None, false) => None,
    };
    let notes =
        db::notes::list_team_notes(&state.db, organization_id, query.team_id, user.user_id, folder, limit, offset)
            .await?;
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
    if let Some(folder_id) = body.folder_id {
        if note.parent_id.is_some() {
            return Err(AppError::BadRequest("Replies can't be filed into a folder -- only top-level notes can.".into()));
        }
        if let Some(folder_id) = folder_id {
            let team_id = note
                .team_id
                .ok_or_else(|| AppError::BadRequest("This note has no team, so it can't be filed into a folder.".into()))?;
            let existing = db::notes::list_note_attachments(&state.db, note.organization_id, note.id).await?;
            let attachments: Vec<(&str, &str, &str)> =
                existing.iter().map(|a| (a.owning_service.as_str(), a.entity_type.as_str(), a.entity_id.as_str())).collect();
            check_folder_in_team(&state, note.organization_id, team_id, folder_id, user.user_id, &attachments).await?;
        }
    }
    let updated = db::notes::update_note(
        &state.db,
        &note,
        body.title.as_deref(),
        body.body_markdown.as_deref(),
        body.visibility,
        body.folder_id,
    )
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
    let created_at = if user.is_admin { body.created_at } else { None };
    let created_by = if user.is_admin { body.created_by.unwrap_or(user.user_id) } else { user.user_id };
    let reply = db::notes::create_reply(&state.db, &parent, created_by, &body.body_markdown, created_at).await?;
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

/// Attaches an already-created note to another entity, e.g. linking one
/// Cartlann research note to a second collection object. Additive to
/// `CreateNoteRequest.attach` (which only ever creates one attachment, at
/// creation time) -- this is how a note ends up linked to *several* entities,
/// or linked to one after the fact.
#[utoipa::path(
    post,
    path = "/notes/{id}/attachments",
    request_body = AttachRequest,
    responses((status = 201, description = "Attachment created", body = NoteAttachment)),
    tag = "notes"
)]
pub async fn create_attachment(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<AttachRequest>,
) -> AppResult<Json<NoteAttachment>> {
    let note = resolve_visible_note(&state.db, &user, id).await?;
    if !can_edit(&note, &user) {
        return Err(AppError::Forbidden("Only the creator or an admin can attach entities to this note.".into()));
    }
    let attachment = db::notes::attach_note(
        &state.db,
        note.organization_id,
        note.id,
        &db::notes::NewAttachment {
            owning_service: body.owning_service,
            entity_type: body.entity_type,
            entity_id: body.entity_id,
        },
    )
    .await?;
    Ok(Json(attachment))
}

#[utoipa::path(
    get,
    path = "/notes/{id}/attachments",
    responses((status = 200, description = "This note's own attachments", body = [NoteAttachment])),
    tag = "notes"
)]
pub async fn list_attachments(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<NoteAttachment>>> {
    let note = resolve_visible_note(&state.db, &user, id).await?;
    let attachments = db::notes::list_note_attachments(&state.db, note.organization_id, note.id).await?;
    Ok(Json(attachments))
}

#[utoipa::path(
    delete,
    path = "/notes/{id}/attachments/{attachment_id}",
    responses((status = 204, description = "Attachment removed")),
    tag = "notes"
)]
pub async fn delete_attachment(
    State(state): State<AppState>,
    user: TackUser,
    Path((id, attachment_id)): Path<(Uuid, Uuid)>,
) -> AppResult<axum::http::StatusCode> {
    let note = resolve_visible_note(&state.db, &user, id).await?;
    if !can_edit(&note, &user) {
        return Err(AppError::Forbidden("Only the creator or an admin can detach entities from this note.".into()));
    }
    db::notes::delete_note_attachment(&state.db, note.organization_id, note.id, attachment_id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
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

/// Deletes one saved version. Refuses to delete the last remaining one (see
/// `db::notes::delete_revision`) — a note must always have at least a
/// baseline snapshot.
#[utoipa::path(
    delete,
    path = "/notes/{id}/revisions/{revision_id}",
    responses((status = 204, description = "Version deleted")),
    tag = "notes"
)]
pub async fn delete_revision(
    State(state): State<AppState>,
    user: TackUser,
    Path((id, revision_id)): Path<(Uuid, Uuid)>,
) -> AppResult<axum::http::StatusCode> {
    let note = resolve_visible_note(&state.db, &user, id).await?;
    if !can_edit(&note, &user) {
        return Err(AppError::Forbidden("Only the creator or an admin can delete a version of this note.".into()));
    }
    db::notes::delete_revision(&state.db, &note, revision_id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Marks a top-level note read by the caller, as of now — see
/// `010_note_reads.sql`. Any caller who can view the note may mark it read;
/// this isn't a `can_edit`-gated action, it's purely the caller's own
/// read state.
#[utoipa::path(
    post,
    path = "/notes/{id}/read",
    responses((status = 200, description = "Read marker updated", body = NoteRead)),
    tag = "notes"
)]
pub async fn mark_note_read(State(state): State<AppState>, user: TackUser, Path(id): Path<Uuid>) -> AppResult<Json<NoteRead>> {
    let note = resolve_visible_note(&state.db, &user, id).await?;
    let read = db::note_reads::mark_read(&state.db, note.id, note.organization_id, user.user_id).await?;
    Ok(Json(read))
}

const MAX_UNREAD_IDS: usize = 200;

#[derive(Debug, Deserialize)]
pub struct UnreadQuery {
    /// Comma-separated note ids to check, capped at 200 per call.
    pub ids: String,
}

/// Live unread status for a batch of top-level notes — see
/// `db::note_reads::unread_status`'s doc comment for exactly what "unread"
/// means. Notes the caller can't view (or that don't exist) are silently
/// omitted from the response rather than erroring the whole batch — same
/// posture as `note_id` being invalid on a route the caller has no access
/// to elsewhere in this API: no information disclosure either way, and one
/// bad id in a UI-driven badge-refresh batch shouldn't fail the rest.
#[utoipa::path(
    get,
    path = "/notes/unread",
    params(("ids" = String, Query, description = "Comma-separated note ids, max 200")),
    responses((status = 200, description = "Unread status for each requested note the caller can view", body = Vec<NoteUnreadStatus>)),
    tag = "notes"
)]
pub async fn list_unread(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<UnreadQuery>,
) -> AppResult<Json<Vec<NoteUnreadStatus>>> {
    let requested: Vec<Uuid> = query
        .ids
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<Uuid>().map_err(|_| AppError::BadRequest(format!("Invalid note id: {s}"))))
        .collect::<AppResult<Vec<_>>>()?;
    if requested.len() > MAX_UNREAD_IDS {
        return Err(AppError::BadRequest(format!("Too many ids: max {MAX_UNREAD_IDS} per call.")));
    }

    // Visible notes are resolved (and grouped by organization) first, then
    // batched one query per organization — a caller's requested ids are
    // almost always all in one org in practice, but nothing here assumes it.
    let mut by_org: std::collections::HashMap<Uuid, Vec<Uuid>> = std::collections::HashMap::new();
    for id in requested {
        if let Ok(note) = resolve_visible_note(&state.db, &user, id).await {
            by_org.entry(note.organization_id).or_default().push(note.id);
        }
    }

    let mut statuses = Vec::new();
    for (org_id, ids) in by_org {
        statuses.extend(db::note_reads::unread_status(&state.db, &ids, org_id, user.user_id).await?);
    }
    Ok(Json(statuses))
}

