use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

use crate::auth::TackUser;
use crate::error::AppResult;
use crate::pages_acl::resolve_visible_page;
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

    // Notes' visibility is fully expressed in the OpenSearch query itself
    // (SearchCaller::acl_filter), so hits of that type are already correctly
    // scoped. Pages' ACL is resolved live from the page/space tree and can't
    // be baked into a static query filter the same way -- `acl_filter` only
    // applies a coarse organization-membership pre-filter for them, so each
    // page hit is re-checked here against the real, live permission
    // resolution before being returned. Same live-recheck idiom already used
    // by `handlers::pages::search_pages`. Skipped entirely for admins, who
    // bypass ACL everywhere else in this service too.
    let hits = if user.is_admin {
        hits
    } else {
        let mut visible = Vec::with_capacity(hits.len());
        for hit in hits {
            if hit.content_type != "page" || resolve_visible_page(&state.db, &user, hit.content_id).await.is_ok() {
                visible.push(hit);
            }
        }
        visible
    };

    Ok(Json(hits))
}
