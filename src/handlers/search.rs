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

/// Lexical (BM25) search across the caller's visible content — ACL is
/// enforced *in* the search query itself (see `SearchCaller::acl_filter`),
/// resolved live from the caller's current JWT team/org claims on every
/// call, same as direct reads via `GET /notes/:id`. Hybrid (+kNN/semantic)
/// search lands once embeddings exist.
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
    let hits = state.search.search(&query.q, &caller).await?;
    Ok(Json(hits))
}
