use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::TackUser;
use crate::error::AppResult;
use crate::notes_acl::resolve_team_organization;
use crate::pages_acl::resolve_visible_page;
use crate::search::{SearchCaller, SearchHit, SearchResults, SearchTypeResults};
use crate::AppState;

const DEFAULT_SEARCH_LIMIT: i64 = 10;
const MAX_SEARCH_LIMIT: i64 = 50;

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    /// Search is scoped to one team at a time, same as `GET /notes` --
    /// required, not optional, so a hit can never surface from a team the
    /// caller isn't currently looking at (see `SearchCaller`'s doc comment
    /// for why this replaced the old "every team/org the caller belongs
    /// to" behavior).
    pub team_id: Uuid,
    /// Defaults to 10, capped at 50.
    pub notes_limit: Option<i64>,
    /// Defaults to 0.
    pub notes_offset: Option<i64>,
    /// Defaults to 10, capped at 50.
    pub pages_limit: Option<i64>,
    /// Defaults to 0.
    pub pages_offset: Option<i64>,
}

fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_SEARCH_LIMIT).clamp(1, MAX_SEARCH_LIMIT)
}

fn paginate(hits: Vec<SearchHit>, limit: i64, offset: i64) -> SearchTypeResults {
    let total = hits.len() as i64;
    let page = hits.into_iter().skip(offset.max(0) as usize).take(limit as usize).collect();
    SearchTypeResults { hits: page, total }
}

/// Hybrid (BM25 + kNN semantic) search, scoped to one team at a time (same
/// as `GET /notes?team_id=`) — a hit can never surface from a team the
/// caller isn't currently looking at. `resolve_team_organization` enforces
/// the same membership check every other team-scoped write already uses,
/// including for an admin caller (an admin still has to belong to the team
/// being searched, exactly like `GET /notes` never bypasses its own
/// `team_id` filter for admins). ACL is otherwise enforced *in* both
/// queries themselves (see `SearchCaller::filters`).
/// Degrades to lexical-only if the embedding model isn't loaded.
///
/// Results are grouped by content type and paginated independently
/// (confirmed design: search stays global across both types regardless of
/// which Navigator tab is active, not scoped to one). Pagination happens
/// *here*, not inside `SearchClient::search`, because it has to happen
/// after this handler's own page-ACL post-filter below -- paginating the
/// fused list first (inside `search.rs`) could hand back a page that's
/// short real results once invisible pages are filtered out of it.
#[utoipa::path(
    get,
    path = "/search",
    params(
        ("q" = String, Query, description = "Free-text query"),
        ("team_id" = Uuid, Query, description = "Scope results to this team -- required, the caller must be a member"),
        ("notes_limit" = Option<i64>, Query, description = "Note results page size, default 10, max 50"),
        ("notes_offset" = Option<i64>, Query, description = "Note results offset, default 0"),
        ("pages_limit" = Option<i64>, Query, description = "Page results page size, default 10, max 50"),
        ("pages_offset" = Option<i64>, Query, description = "Page results offset, default 0"),
    ),
    responses((status = 200, description = "Matching content, ACL-filtered, grouped by type", body = SearchResults)),
    tag = "search"
)]
pub async fn search(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<SearchQuery>,
) -> AppResult<Json<SearchResults>> {
    let organization_id = resolve_team_organization(&user, query.team_id)?;
    let caller = SearchCaller {
        user_id: user.user_id,
        is_admin: user.is_admin,
        team_id: query.team_id,
        organization_id,
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

    let (note_hits, page_hits): (Vec<SearchHit>, Vec<SearchHit>) =
        hits.into_iter().partition(|h| h.content_type == "note");

    let notes = paginate(note_hits, clamp_limit(query.notes_limit), query.notes_offset.unwrap_or(0));
    let pages = paginate(page_hits, clamp_limit(query.pages_limit), query.pages_offset.unwrap_or(0));

    Ok(Json(SearchResults { notes, pages }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn hit() -> SearchHit {
        SearchHit {
            content_id: Uuid::new_v4(),
            content_type: "note".into(),
            score: 1.0,
            title: "t".into(),
            text: "x".into(),
            highlight: vec![],
            folder_id: None,
            space_id: None,
            parent_id: None,
        }
    }

    #[test]
    fn paginate_reports_total_ignoring_the_slice() {
        let hits: Vec<_> = (0..5).map(|_| hit()).collect();
        let result = paginate(hits, 2, 0);
        assert_eq!(result.hits.len(), 2);
        assert_eq!(result.total, 5);
    }

    #[test]
    fn paginate_offset_moves_the_window_without_changing_total() {
        let hits: Vec<_> = (0..5).map(|_| hit()).collect();
        let result = paginate(hits, 2, 4);
        assert_eq!(result.hits.len(), 1, "only one hit left past offset 4 of 5");
        assert_eq!(result.total, 5);
    }

    #[test]
    fn clamp_limit_defaults_and_caps() {
        assert_eq!(clamp_limit(None), DEFAULT_SEARCH_LIMIT);
        assert_eq!(clamp_limit(Some(9999)), MAX_SEARCH_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1, "a zero/negative limit must not mean 'unlimited'");
    }
}
