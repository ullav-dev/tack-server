use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{RawBearerToken, TackUser};
use crate::db;
use crate::error::{AppError, AppResult};
use crate::models::note::{CreateNoteFolderRequest, FolderType, NoteFolder, NoteFoldersPage, UpdateNoteFolderRequest};
use crate::notes_acl::resolve_team_organization_live;
use crate::AppState;

/// Resolves a bare folder id to a `NoteFolder`, scoped to whichever of the
/// caller's organizations actually contains it (mirrors
/// `notes_acl::resolve_visible_note`'s "try each org" shape, since a
/// folder's organization_id isn't known up front from just its id). Any
/// member of the folder's own team may act on it -- folders carry no
/// visibility of their own (see `NoteFolder`'s doc comment) -- so this is a
/// team-membership check, not a creator/admin one.
pub async fn resolve_folder_for_caller(state: &AppState, user: &TackUser, id: Uuid) -> AppResult<NoteFolder> {
    for org_id in user.organization_ids() {
        if let Some(folder) = db::note_folders::get_folder(&state.db, id, org_id, user.user_id).await? {
            if user.is_admin || user.teams.contains_key(&folder.team_id) {
                return Ok(folder);
            }
            return Err(AppError::Forbidden("You are not a member of this folder's team.".into()));
        }
    }
    if user.is_admin {
        if let Some(folder) = db::note_folders::get_folder_admin_any_org(&state.db, id).await? {
            return Ok(folder);
        }
    }
    Err(AppError::NotFound(format!("Folder {id} not found")))
}

/// Validates that `folder_id`, if given, is a real folder belonging to
/// `team_id` in `organization_id` -- shared by `create_note` and
/// `update_note` in `handlers::notes`, since both let a caller file a note
/// into a folder and neither should silently accept a folder from a
/// different team (or one that doesn't exist). `caller_id` only affects the
/// returned `note_count` (unused here), but `get_folder` requires it.
///
/// `note_attachments` is every entity this note is (or is about to be)
/// attached to, as `(owning_service, entity_type, entity_id)` triples. If
/// the target folder is entity-scoped, at least one of them must match it
/// *exactly*, or a note attached to ticket A could be filed into ticket B's
/// folder despite never appearing in ticket B's own note list. A team-wide
/// folder (the common case) has no such requirement, same as before this
/// check existed. A note can in principle carry more than one attachment
/// (`POST /notes/{id}/attachments`), hence a slice rather than a single
/// optional triple.
pub async fn check_folder_in_team(
    state: &AppState,
    organization_id: Uuid,
    team_id: Uuid,
    folder_id: Uuid,
    caller_id: Uuid,
    note_attachments: &[(&str, &str, &str)],
) -> AppResult<()> {
    let folder = db::note_folders::get_folder(&state.db, folder_id, organization_id, caller_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("folder_id does not refer to an existing folder.".into()))?;
    if folder.team_id != team_id {
        return Err(AppError::BadRequest("folder_id must belong to the note's own team.".into()));
    }
    // An Idea Board is a note_folders row too, but its notes are stickies,
    // managed exclusively through handlers::idea_boards (which never calls
    // this function) -- filing an ordinary note into a board via
    // POST/PATCH /notes would create a note with no idea_board_stickies
    // row, and would also break delete_board's "delete every sticky's note,
    // then the folder" ordering (the leftover note keeps the folder's own
    // notes_folder_id_fkey from being satisfied on delete).
    if folder.folder_type != "general" {
        return Err(AppError::BadRequest("folder_id refers to an Idea Board, not a Notes folder.".into()));
    }
    if let (Some(folder_owning_service), Some(folder_entity_type), Some(folder_entity_id)) =
        (&folder.owning_service, &folder.entity_type, &folder.entity_id)
    {
        let matches = note_attachments
            .iter()
            .any(|(s, t, id)| s == folder_owning_service && t == folder_entity_type && id == folder_entity_id);
        if !matches {
            return Err(AppError::BadRequest(
                "folder_id belongs to an entity-scoped folder for a different entity than this note is attached to.".into(),
            ));
        }
    }
    Ok(())
}

#[utoipa::path(
    post,
    path = "/note-folders",
    request_body = CreateNoteFolderRequest,
    responses((status = 201, description = "Folder created", body = NoteFolder)),
    tag = "notes"
)]
pub async fn create_note_folder(
    State(state): State<AppState>,
    user: TackUser,
    RawBearerToken(raw_token): RawBearerToken,
    Json(body): Json<CreateNoteFolderRequest>,
) -> AppResult<Json<NoteFolder>> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    let organization_id = resolve_team_organization_live(&state, &user, &raw_token, body.team_id).await?;
    let folder = db::note_folders::create_folder(
        &state.db,
        organization_id,
        body.team_id,
        name,
        user.user_id,
        body.attach.as_ref(),
        FolderType::General,
    )
    .await?;
    Ok(Json(folder))
}

#[derive(Debug, Deserialize)]
pub struct ListNoteFoldersByAttachmentQuery {
    pub owning_service: String,
    pub entity_type: String,
    pub entity_id: String,
}

/// One entity's own folders (e.g. a cunav ticket's sub-folders) -- the
/// folder analogue of `handlers::notes::list_notes_by_attachment`, same
/// "scan the caller's orgs, first org with any *visible* rows wins" shape
/// and same reasoning for why that's fine (an entity's folders only ever
/// belong to one organization in practice). Folders carry no visibility
/// tier of their own (see `NoteFolder`'s doc comment), but they do still
/// belong to a team, and the DB query is organization-scoped only -- so
/// every row is filtered to the caller's own team memberships before being
/// counted, the same shape `list_note_folders` below already applies via
/// its explicit `team_ids` list (this endpoint just can't build that list
/// up front, since it isn't given a team_id, only an entity).
#[utoipa::path(
    get,
    path = "/note-folders/by-entity",
    params(
        ("owning_service" = String, Query, description = "Namespacing service, e.g. \"cunav\""),
        ("entity_type" = String, Query, description = "e.g. \"workflow\""),
        ("entity_id" = String, Query, description = "The external entity's own id"),
    ),
    responses((status = 200, description = "This entity's own folders", body = [NoteFolder])),
    tag = "notes"
)]
pub async fn list_note_folders_by_attachment(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<ListNoteFoldersByAttachmentQuery>,
) -> AppResult<Json<Vec<NoteFolder>>> {
    for organization_id in user.organization_ids() {
        let folders = db::note_folders::list_folders_for_entity(
            &state.db,
            organization_id,
            &query.owning_service,
            &query.entity_type,
            &query.entity_id,
            user.user_id,
            "general",
        )
        .await?;
        let visible: Vec<_> = folders.into_iter().filter(|f| user.is_admin || user.teams.contains_key(&f.team_id)).collect();
        if !visible.is_empty() {
            return Ok(Json(visible));
        }
    }
    Ok(Json(Vec::new()))
}

const DEFAULT_FOLDERS_LIMIT: i64 = 25;
const MAX_FOLDERS_LIMIT: i64 = 100;

#[derive(Debug, Deserialize)]
pub struct ListNoteFoldersQuery {
    /// Optional: only this team's folders. Omitted returns every folder
    /// across every Tack-enabled team the caller belongs to, same
    /// "list across all my orgs" shape as `GET /spaces`.
    pub team_id: Option<Uuid>,
    /// Defaults to 25, capped at 100.
    pub limit: Option<i64>,
    /// Defaults to 0.
    pub offset: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/note-folders",
    params(
        ("team_id" = Option<Uuid>, Query, description = "Only this team's folders, if given"),
        ("limit" = Option<i64>, Query, description = "Page size, default 25, max 100"),
        ("offset" = Option<i64>, Query, description = "Offset into the (alphabetical) list, default 0"),
    ),
    responses((status = 200, description = "A page of folders visible to the caller", body = NoteFoldersPage)),
    tag = "notes"
)]
pub async fn list_note_folders(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<ListNoteFoldersQuery>,
) -> AppResult<Json<NoteFoldersPage>> {
    let team_ids: Vec<Uuid> = match query.team_id {
        Some(id) => {
            if !user.teams.contains_key(&id) {
                return Err(AppError::Forbidden("You are not a member of this team.".into()));
            }
            vec![id]
        }
        None => user.teams.keys().copied().collect(),
    };
    let limit = query.limit.unwrap_or(DEFAULT_FOLDERS_LIMIT).clamp(1, MAX_FOLDERS_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);
    let mut folders = Vec::new();
    let mut total = 0;
    for org_id in user.organization_ids() {
        let (org_folders, org_total) =
            db::note_folders::list_folders_for_teams(&state.db, org_id, &team_ids, user.user_id, limit, offset, "general")
                .await?;
        folders.extend(org_folders);
        total += org_total;
    }
    Ok(Json(NoteFoldersPage { folders, total }))
}

#[utoipa::path(
    patch,
    path = "/note-folders/{id}",
    request_body = UpdateNoteFolderRequest,
    responses((status = 200, description = "Renamed folder", body = NoteFolder)),
    tag = "notes"
)]
pub async fn update_note_folder(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateNoteFolderRequest>,
) -> AppResult<Json<NoteFolder>> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    let folder = resolve_folder_for_caller(&state, &user, id).await?;
    let updated = db::note_folders::rename_folder(&state.db, &folder, name, user.user_id).await?;
    Ok(Json(updated))
}

#[utoipa::path(
    delete,
    path = "/note-folders/{id}",
    responses((status = 204, description = "Folder deleted; its notes are unfiled, not deleted")),
    tag = "notes"
)]
pub async fn delete_note_folder(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<axum::http::StatusCode> {
    let folder = resolve_folder_for_caller(&state, &user, id).await?;
    db::note_folders::delete_folder(&state.db, &folder).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
