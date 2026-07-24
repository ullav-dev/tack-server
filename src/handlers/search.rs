use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

use crate::auth::TackUser;
use crate::error::AppResult;
use crate::search::{SearchCaller, SearchHit};
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
}

/// Hybrid (BM25 + kNN semantic) search across the caller's visible content —
/// ACL is enforced *in* both queries themselves (see
/// `SearchCaller::acl_filter`), resolved live from the caller's current JWT
/// team/org claims on every call, same as direct reads via `GET /notes/:id`.
/// Degrades to lexical-only if the embedding model isn't loaded.
#[utoipa::path(
    get,
    path = "/search",
    params(("q" = String, Query, description = "Free-text query")),
    responses((status = 200, description = "Matching content, ACL-filtered", body = [SearchHit])),
    tag = "search"
)]
pub async fn search(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<SearchQuery>,
) -> AppResult<Json<Vec<SearchHit>>> {
    let caller = SearchCaller {
        user_id: user.user_id,
        is_admin: user.is_admin,
        team_ids: user.teams.keys().copied().collect(),
        organization_ids: user.organization_ids(),
    };
    let hits = state.search.search(&query.q, &caller, state.embedder.as_ref()).await?;
    Ok(Json(hits))
}
