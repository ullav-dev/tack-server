use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::TackUser;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::models::note::{CreateNoteFolderRequest, NoteFolder, UpdateNoteFolderRequest};
use crate::notes_acl::resolve_team_organization;
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
pub async fn check_folder_in_team(
    state: &AppState,
    organization_id: Uuid,
    team_id: Uuid,
    folder_id: Uuid,
    caller_id: Uuid,
) -> AppResult<()> {
    let folder = db::note_folders::get_folder(&state.db, folder_id, organization_id, caller_id)
        .await?
        .ok_or_else(|| AppError::BadRequest("folder_id does not refer to an existing folder.".into()))?;
    if folder.team_id != team_id {
        return Err(AppError::BadRequest("folder_id must belong to the note's own team.".into()));
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
    Json(body): Json<CreateNoteFolderRequest>,
) -> AppResult<Json<NoteFolder>> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    let organization_id = resolve_team_organization(&user, body.team_id)?;
    let folder = db::note_folders::create_folder(&state.db, organization_id, body.team_id, name, user.user_id).await?;
    Ok(Json(folder))
}

#[derive(Debug, Deserialize)]
pub struct ListNoteFoldersQuery {
    /// Optional: only this team's folders. Omitted returns every folder
    /// across every Tack-enabled team the caller belongs to, same
    /// "list across all my orgs" shape as `GET /spaces`.
    pub team_id: Option<Uuid>,
}

#[utoipa::path(
    get,
    path = "/note-folders",
    params(("team_id" = Option<Uuid>, Query, description = "Only this team's folders, if given")),
    responses((status = 200, description = "Folders visible to the caller", body = [NoteFolder])),
    tag = "notes"
)]
pub async fn list_note_folders(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<ListNoteFoldersQuery>,
) -> AppResult<Json<Vec<NoteFolder>>> {
    let team_ids: Vec<Uuid> = match query.team_id {
        Some(id) => {
            if !user.teams.contains_key(&id) {
                return Err(AppError::Forbidden("You are not a member of this team.".into()));
            }
            vec![id]
        }
        None => user.teams.keys().copied().collect(),
    };
    let mut folders = Vec::new();
    for org_id in user.organization_ids() {
        folders.extend(db::note_folders::list_folders_for_teams(&state.db, org_id, &team_ids, user.user_id).await?);
    }
    Ok(Json(folders))
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
