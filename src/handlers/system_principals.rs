//! System principals: admin-managed non-human note authors (see
//! `009_system_principals.sql`). Create/delete are admin-only -- minting a
//! new bot identity is an ops action, not something any Tack user should be
//! able to do. List is open to any caller who belongs to the organization
//! (or an admin), since resolving `created_by` to a display name is a
//! read-path every note viewer needs, not just admins.

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::TackUser;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::models::system_principal::{CreateSystemPrincipalRequest, SystemPrincipal, SystemPrincipalsPage};
use crate::AppState;

#[utoipa::path(
    post,
    path = "/system-principals",
    request_body = CreateSystemPrincipalRequest,
    responses((status = 201, description = "System principal created", body = SystemPrincipal)),
    tag = "notes"
)]
pub async fn create_system_principal(
    State(state): State<AppState>,
    user: TackUser,
    Json(body): Json<CreateSystemPrincipalRequest>,
) -> AppResult<Json<SystemPrincipal>> {
    if !user.is_admin {
        return Err(AppError::Forbidden("Only admins may create system principals.".into()));
    }
    let label = body.label.trim();
    if label.is_empty() {
        return Err(AppError::BadRequest("label must not be empty".into()));
    }
    let principal = db::system_principals::create_principal(&state.db, body.organization_id, label).await?;
    Ok(Json(principal))
}

const DEFAULT_PRINCIPALS_LIMIT: i64 = 25;
const MAX_PRINCIPALS_LIMIT: i64 = 100;

#[derive(Debug, Deserialize)]
pub struct ListSystemPrincipalsQuery {
    pub organization_id: Uuid,
    /// Defaults to 25, capped at 100.
    pub limit: Option<i64>,
    /// Defaults to 0.
    pub offset: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/system-principals",
    params(
        ("organization_id" = Uuid, Query, description = "The organization to list system principals for"),
        ("limit" = Option<i64>, Query, description = "Page size, default 25, max 100"),
        ("offset" = Option<i64>, Query, description = "Offset into the (alphabetical) list, default 0"),
    ),
    responses((status = 200, description = "A page of system principals in the organization", body = SystemPrincipalsPage)),
    tag = "notes"
)]
pub async fn list_system_principals(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<ListSystemPrincipalsQuery>,
) -> AppResult<Json<SystemPrincipalsPage>> {
    if !user.is_admin && !user.organization_ids().contains(&query.organization_id) {
        return Err(AppError::Forbidden("You are not a member of this organization.".into()));
    }
    let limit = query.limit.unwrap_or(DEFAULT_PRINCIPALS_LIMIT).clamp(1, MAX_PRINCIPALS_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);
    let (principals, total) = db::system_principals::list_principals(&state.db, query.organization_id, limit, offset).await?;
    Ok(Json(SystemPrincipalsPage { principals, total }))
}

#[utoipa::path(
    delete,
    path = "/system-principals/{id}",
    responses((status = 204, description = "System principal deleted")),
    tag = "notes"
)]
pub async fn delete_system_principal(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<axum::http::StatusCode> {
    if !user.is_admin {
        return Err(AppError::Forbidden("Only admins may delete system principals.".into()));
    }
    // Admin-only, and admins aren't necessarily a member of any team in the
    // principal's organization (unlike a regular caller, whose
    // organization_ids() would normally scope this) -- delete by id alone.
    db::system_principals::delete_principal(&state.db, id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
