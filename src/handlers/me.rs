use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::auth::TackUser;
use crate::error::AppResult;

#[derive(Debug, Serialize, ToSchema)]
pub struct MeResponse {
    pub user_id: Uuid,
    pub is_admin: bool,
    /// Distinct organizations across the caller's Tack-enabled teams.
    pub organization_ids: Vec<Uuid>,
    /// Number of Tack-enabled teams the caller belongs to.
    pub team_count: usize,
}

/// Whoami — confirms the caller's Tack access and resolved identity.
/// First real proof this service's auth extractor decodes a live UUM JWT correctly.
#[utoipa::path(
    get,
    path = "/me",
    responses(
        (status = 200, description = "Caller's resolved Tack identity", body = MeResponse),
        (status = 401, description = "Missing or invalid bearer token"),
        (status = 403, description = "No team has Tack access"),
    ),
    tag = "me"
)]
pub async fn me(user: TackUser) -> AppResult<Json<MeResponse>> {
    Ok(Json(MeResponse {
        user_id: user.user_id,
        is_admin: user.is_admin,
        organization_ids: user.organization_ids(),
        team_count: user.teams.len(),
    }))
}
