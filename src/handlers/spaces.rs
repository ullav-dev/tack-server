use axum::{
    extract::{Path, State},
    Json,
};
use uuid::Uuid;

use crate::auth::TackUser;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::models::page::{CreateSpaceRequest, Space, UpdateSpaceRequest};
use crate::pages_acl::{can_create_in_space, resolve_space, resolve_team_organization};
use crate::AppState;

#[utoipa::path(
    post,
    path = "/spaces",
    request_body = CreateSpaceRequest,
    responses((status = 201, description = "Space created", body = Space)),
    tag = "pages"
)]
pub async fn create_space(
    State(state): State<AppState>,
    user: TackUser,
    Json(body): Json<CreateSpaceRequest>,
) -> AppResult<Json<Space>> {
    if body.name.trim().is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    let organization_id = resolve_team_organization(&user, body.team_id)?;
    let space = db::spaces::create_space(&state.db, organization_id, body.team_id, &body.name).await?;
    Ok(Json(space))
}

/// Spaces visible to the caller across every organization they belong to
/// (their own teams' spaces, plus any org-wide space in those organizations).
#[utoipa::path(
    get,
    path = "/spaces",
    responses((status = 200, description = "Spaces visible to the caller", body = [Space])),
    tag = "pages"
)]
pub async fn list_spaces(State(state): State<AppState>, user: TackUser) -> AppResult<Json<Vec<Space>>> {
    let team_ids: Vec<_> = user.teams.keys().copied().collect();
    let mut spaces = Vec::new();
    for org_id in user.organization_ids() {
        spaces.extend(db::spaces::list_spaces_for_teams(&state.db, org_id, &team_ids).await?);
    }
    Ok(Json(spaces))
}

/// Renames a space. Reuses `pages_acl::can_create_in_space`'s "space
/// default Edit level" check -- the same rule that already gates creating a
/// root page directly under the space, since there's no finer-grained
/// space-level permission model than that.
#[utoipa::path(
    patch,
    path = "/spaces/{id}",
    request_body = UpdateSpaceRequest,
    responses((status = 200, description = "Renamed space", body = Space)),
    tag = "pages"
)]
pub async fn update_space(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateSpaceRequest>,
) -> AppResult<Json<Space>> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    let space = resolve_space(&state.db, &user, id).await?;
    if !can_create_in_space(&space, &user) {
        return Err(AppError::Forbidden("You don't have edit access to this space.".into()));
    }
    let updated = db::spaces::rename_space(&state.db, space.id, space.organization_id, name).await?;
    Ok(Json(updated))
}
