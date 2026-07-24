use axum::{extract::State, Json};

use crate::auth::TackUser;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::models::page::{CreateSpaceRequest, Space};
use crate::pages_acl::resolve_team_organization;
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
