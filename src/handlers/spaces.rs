use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::TackUser;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::models::page::{CreateSpaceRequest, Space, SpacesPage, UpdateSpaceRequest};
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

const DEFAULT_SPACES_LIMIT: i64 = 25;
const MAX_SPACES_LIMIT: i64 = 100;

#[derive(Debug, Deserialize)]
pub struct ListSpacesQuery {
    /// Defaults to 25, capped at 100.
    pub limit: Option<i64>,
    /// Defaults to 0.
    pub offset: Option<i64>,
}

/// Spaces visible to the caller across every organization they belong to
/// (their own teams' spaces, plus any org-wide space in those organizations).
#[utoipa::path(
    get,
    path = "/spaces",
    params(
        ("limit" = Option<i64>, Query, description = "Page size, default 25, max 100"),
        ("offset" = Option<i64>, Query, description = "Offset into the (alphabetical) list, default 0"),
    ),
    responses((status = 200, description = "A page of spaces visible to the caller", body = SpacesPage)),
    tag = "pages"
)]
pub async fn list_spaces(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<ListSpacesQuery>,
) -> AppResult<Json<SpacesPage>> {
    let team_ids: Vec<_> = user.teams.keys().copied().collect();
    let limit = query.limit.unwrap_or(DEFAULT_SPACES_LIMIT).clamp(1, MAX_SPACES_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);
    let mut spaces = Vec::new();
    let mut total = 0;
    for org_id in user.organization_ids() {
        let (org_spaces, org_total) = db::spaces::list_spaces_for_teams(&state.db, org_id, &team_ids, limit, offset).await?;
        spaces.extend(org_spaces);
        total += org_total;
    }
    Ok(Json(SpacesPage { spaces, total }))
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
