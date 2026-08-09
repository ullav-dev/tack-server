use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::TackUser;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::models::page::{
    CreatePageReferenceRequest, CreatePagePermissionRequest, CreatePageRequest, Page, PageBacklink, PagePermission,
    PagePermissionLevelResponse, PageReference, PageRevision, PagesPage, UpdatePageRequest,
};
use crate::pages_acl::{
    can_create_in_space, can_view, require_edit, resolve_effective_permission, resolve_space, resolve_visible_page,
};
use crate::AppState;

#[utoipa::path(
    post,
    path = "/pages",
    request_body = CreatePageRequest,
    responses((status = 201, description = "Page created", body = Page)),
    tag = "pages"
)]
pub async fn create_page(
    State(state): State<AppState>,
    user: TackUser,
    Json(body): Json<CreatePageRequest>,
) -> AppResult<Json<Page>> {
    if body.title.trim().is_empty() {
        return Err(AppError::BadRequest("title must not be empty".into()));
    }

    let space = resolve_space(&state.db, &user, body.space_id).await?;

    match body.parent_id {
        None => {
            if !can_create_in_space(&space, &user) {
                return Err(AppError::Forbidden("You don't have edit access to this space.".into()));
            }
        }
        Some(parent_id) => {
            let (parent, parent_space) = resolve_visible_page(&state.db, &user, parent_id).await?;
            if parent.space_id != body.space_id {
                return Err(AppError::BadRequest("parent_id must belong to the given space_id".into()));
            }
            require_edit(&state.db, &user, &parent, &parent_space).await?;
        }
    }

    let page = db::pages::create_page(
        &state.db,
        db::pages::NewPage {
            organization_id: space.organization_id,
            space_id: body.space_id,
            parent_id: body.parent_id,
            title: body.title,
            content_markdown: body.content_markdown,
            created_by: user.user_id,
        },
    )
    .await?;
    Ok(Json(page))
}

const DEFAULT_PAGES_LIMIT: i64 = 25;
const MAX_PAGES_LIMIT: i64 = 100;

#[derive(Debug, Deserialize)]
pub struct ListPagesQuery {
    /// Omit to list root pages (pages with no parent) in the space.
    pub parent_id: Option<Uuid>,
    /// Defaults to 25, capped at 100.
    pub limit: Option<i64>,
    /// Defaults to 0.
    pub offset: Option<i64>,
}

/// Direct children of `parent_id` in this space (or root pages if omitted),
/// filtered to only the ones the caller can view, then paginated.
///
/// Each candidate's permission is resolved individually — a child page's
/// own override can make it more (or less) restricted than its siblings —
/// which means the ACL filter can't be pushed into `db::pages::list_children`'s
/// SQL the way Notes' visibility enum can (same limitation
/// `handlers::search::search` already documents for page search hits).
/// A true `LIMIT`/`OFFSET` at the SQL layer would paginate *before* that
/// filter removes rows, undercounting a page. So this fetches every direct
/// child of `parent_id` (bounded by realistic authoring — nobody creates a
/// huge flat sibling list under one page in practice, and Confluence itself
/// doesn't paginate a page's children either, for the same structural
/// reason), resolves each one's live permission, *then* slices the visible
/// set for `limit`/`offset`. The API still gets a real `limit`/`offset`/
/// `total` contract identical to the other three paginated lists; only the
/// internal implementation fetches more than one page's worth up front.
#[utoipa::path(
    get,
    path = "/spaces/{id}/pages",
    params(
        ("parent_id" = Option<Uuid>, Query, description = "List children of this page, or root pages if omitted"),
        ("limit" = Option<i64>, Query, description = "Page size, default 25, max 100"),
        ("offset" = Option<i64>, Query, description = "Offset into the (alphabetical) visible list, default 0"),
    ),
    responses((status = 200, description = "A page of visible pages at this level of the tree", body = PagesPage)),
    tag = "pages"
)]
pub async fn list_pages(
    State(state): State<AppState>,
    user: TackUser,
    Path(space_id): Path<Uuid>,
    Query(query): Query<ListPagesQuery>,
) -> AppResult<Json<PagesPage>> {
    let space = resolve_space(&state.db, &user, space_id).await?;
    let candidates = db::pages::list_children(&state.db, space.organization_id, space_id, query.parent_id).await?;

    let mut visible = Vec::with_capacity(candidates.len());
    for page in candidates {
        let level = resolve_effective_permission(&state.db, &user, &page, &space).await?;
        if level.is_some() {
            visible.push(page);
        }
    }

    let total = visible.len() as i64;
    let limit = query.limit.unwrap_or(DEFAULT_PAGES_LIMIT).clamp(1, MAX_PAGES_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);
    let pages = visible.into_iter().skip(offset as usize).take(limit as usize).collect();
    Ok(Json(PagesPage { pages, total }))
}

#[utoipa::path(
    get,
    path = "/pages/{id}",
    responses((status = 200, description = "A page", body = Page)),
    tag = "pages"
)]
pub async fn get_page(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Page>> {
    let (page, _space) = resolve_visible_page(&state.db, &user, id).await?;
    Ok(Json(page))
}

/// The caller's own effective permission level on this page — `view` or
/// `edit`, resolved by the same live ancestor/space-fallback algorithm as
/// every other page endpoint. Exists specifically so that `tack-hocuspocus`
/// (a separate service, in a different language) can delegate ACL
/// resolution back here rather than reimplementing it in TypeScript; also
/// useful to the frontend directly (e.g. to render the editor read-only).
#[utoipa::path(
    get,
    path = "/pages/{id}/permission",
    responses((status = 200, description = "Caller's effective permission level", body = PagePermissionLevelResponse)),
    tag = "pages"
)]
pub async fn get_page_permission(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<PagePermissionLevelResponse>> {
    let (page, space) = resolve_visible_page(&state.db, &user, id).await?;
    // `resolve_visible_page` already required at least View access, so
    // `level` is guaranteed `Some` here -- the `unwrap_or` is defensive,
    // not a real fallback path.
    let level = resolve_effective_permission(&state.db, &user, &page, &space)
        .await?
        .unwrap_or(crate::models::page::PermissionLevel::View);
    Ok(Json(PagePermissionLevelResponse { level }))
}

#[utoipa::path(
    patch,
    path = "/pages/{id}",
    request_body = UpdatePageRequest,
    responses((status = 200, description = "Updated page", body = Page)),
    tag = "pages"
)]
pub async fn update_page(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdatePageRequest>,
) -> AppResult<Json<Page>> {
    if let Some(ref title) = body.title {
        if title.trim().is_empty() {
            return Err(AppError::BadRequest("title must not be empty".into()));
        }
    }
    let (page, space) = resolve_visible_page(&state.db, &user, id).await?;
    require_edit(&state.db, &user, &page, &space).await?;

    let updated =
        db::pages::update_page(&state.db, &page, body.title.as_deref(), body.content_markdown.as_deref()).await?;
    Ok(Json(updated))
}

#[utoipa::path(
    delete,
    path = "/pages/{id}",
    responses((status = 204, description = "Page soft-deleted")),
    tag = "pages"
)]
pub async fn delete_page(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<axum::http::StatusCode> {
    let (page, space) = resolve_visible_page(&state.db, &user, id).await?;
    require_edit(&state.db, &user, &page, &space).await?;
    db::pages::soft_delete_page(&state.db, &page).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/pages/{id}/permissions",
    responses((status = 200, description = "This page's own explicit permission overrides (not inherited)", body = [PagePermission])),
    tag = "pages"
)]
pub async fn list_page_permissions(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<PagePermission>>> {
    let (page, space) = resolve_visible_page(&state.db, &user, id).await?;
    require_edit(&state.db, &user, &page, &space).await?;
    let permissions = db::pages::list_own_permissions(&state.db, page.id, page.organization_id).await?;
    Ok(Json(permissions))
}

#[utoipa::path(
    post,
    path = "/pages/{id}/permissions",
    request_body = CreatePagePermissionRequest,
    responses((status = 201, description = "Permission override added", body = PagePermission)),
    tag = "pages"
)]
pub async fn create_page_permission(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreatePagePermissionRequest>,
) -> AppResult<Json<PagePermission>> {
    let (page, space) = resolve_visible_page(&state.db, &user, id).await?;
    require_edit(&state.db, &user, &page, &space).await?;

    let principal_id = match body.principal_type {
        crate::models::page::PrincipalType::Organization => None,
        _ => Some(body.principal_id.ok_or_else(|| {
            AppError::BadRequest("principal_id is required for team/user principals".into())
        })?),
    };

    let permission =
        db::pages::add_permission(&state.db, page.organization_id, page.id, body.principal_type, principal_id, body.level)
            .await?;
    Ok(Json(permission))
}

#[utoipa::path(
    delete,
    path = "/pages/{id}/permissions/{permission_id}",
    responses((status = 204, description = "Permission override removed")),
    tag = "pages"
)]
pub async fn delete_page_permission(
    State(state): State<AppState>,
    user: TackUser,
    Path((id, permission_id)): Path<(Uuid, Uuid)>,
) -> AppResult<axum::http::StatusCode> {
    let (page, space) = resolve_visible_page(&state.db, &user, id).await?;
    require_edit(&state.db, &user, &page, &space).await?;
    db::pages::delete_permission(&state.db, permission_id, page.organization_id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    get,
    path = "/pages/{id}/revisions",
    responses((status = 200, description = "Revision history, newest first", body = [PageRevision])),
    tag = "pages"
)]
pub async fn list_page_revisions(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<PageRevision>>> {
    let (page, _space) = resolve_visible_page(&state.db, &user, id).await?;
    let revisions = db::pages::list_page_revisions(&state.db, page.id, page.organization_id).await?;
    Ok(Json(revisions))
}

/// Snapshots the page's current `content_markdown` as a new named version —
/// a deliberate action, not an automatic side effect of every autosave-style
/// collaborative edit (see `db::pages::create_page_revision`).
#[utoipa::path(
    post,
    path = "/pages/{id}/revisions",
    responses((status = 201, description = "New version created", body = PageRevision)),
    tag = "pages"
)]
pub async fn create_page_revision(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<PageRevision>> {
    let (page, space) = resolve_visible_page(&state.db, &user, id).await?;
    require_edit(&state.db, &user, &page, &space).await?;
    let revision = db::pages::create_page_revision(&state.db, &page, user.user_id).await?;
    Ok(Json(revision))
}

#[utoipa::path(
    delete,
    path = "/pages/{id}/revisions/{revision_id}",
    responses((status = 204, description = "Version deleted")),
    tag = "pages"
)]
pub async fn delete_page_revision(
    State(state): State<AppState>,
    user: TackUser,
    Path((id, revision_id)): Path<(Uuid, Uuid)>,
) -> AppResult<axum::http::StatusCode> {
    let (page, space) = resolve_visible_page(&state.db, &user, id).await?;
    require_edit(&state.db, &user, &page, &space).await?;
    db::pages::delete_page_revision(&state.db, &page, revision_id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct SearchPagesQuery {
    /// Used only to resolve which organization to search within (and to
    /// confirm the caller has access to at least this space) -- results
    /// are not narrowed to this one space. See `db::pages::search_pages`.
    pub space_id: Uuid,
    #[serde(default)]
    pub q: String,
}

/// Plain title search across an organization's pages, ACL-filtered per
/// candidate exactly like `list_pages` -- the backing store for the Page
/// reference picker (F7 8d). Not routed through OpenSearch: Page content
/// indexing there is a separate, still-pending gap (see CLAUDE.md), and a
/// title search doesn't need it.
#[utoipa::path(
    get,
    path = "/pages/search",
    params(
        ("space_id" = Uuid, Query, description = "Resolves which organization to search; results aren't narrowed to this space"),
        ("q" = Option<String>, Query, description = "Title substring to search for; omit/empty to list recent pages"),
    ),
    responses((status = 200, description = "Visible pages matching the query", body = [Page])),
    tag = "pages"
)]
pub async fn search_pages(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<SearchPagesQuery>,
) -> AppResult<Json<Vec<Page>>> {
    let scoping_space = resolve_space(&state.db, &user, query.space_id).await?;
    let candidates = db::pages::search_pages(&state.db, scoping_space.organization_id, query.q.trim(), 20).await?;

    let mut visible = Vec::with_capacity(candidates.len());
    for page in candidates {
        // Candidates can be in a different space than `scoping_space`
        // (search is org-wide) -- each one's own space governs its
        // permission resolution, not the one used to scope the search.
        let Some(page_space) = db::spaces::get_space(&state.db, page.space_id, scoping_space.organization_id).await? else {
            continue;
        };
        let level = resolve_effective_permission(&state.db, &user, &page, &page_space).await?;
        if can_view(level) {
            visible.push(page);
        }
    }
    Ok(Json(visible))
}

/// Resolves a raw `content_references` row's `target_page_id` into a live
/// title/space, or `None`/`None` if the target no longer exists or the
/// caller can no longer see it -- a "broken link" the UI can show, not an
/// error. Shared by both `list_page_references` (resolving the target) and
/// `list_page_backlinks` (resolving the source, from the caller's
/// perspective the "other" page either way).
async fn resolve_other_page(state: &AppState, user: &TackUser, other_page_id: Uuid) -> (Option<String>, Option<Uuid>) {
    match resolve_visible_page(&state.db, user, other_page_id).await {
        Ok((page, _space)) => (Some(page.title), Some(page.space_id)),
        Err(_) => (None, None),
    }
}

#[utoipa::path(
    post,
    path = "/pages/{id}/references",
    request_body = CreatePageReferenceRequest,
    responses((status = 201, description = "Reference created", body = PageReference)),
    tag = "pages"
)]
pub async fn create_page_reference(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<CreatePageReferenceRequest>,
) -> AppResult<Json<PageReference>> {
    let (page, space) = resolve_visible_page(&state.db, &user, id).await?;
    require_edit(&state.db, &user, &page, &space).await?;

    // Confirms the target actually exists and is visible to the caller --
    // content_references has no DB-level FK (it's polymorphic, see
    // migrations/002_content_links.sql), so this is the only thing
    // stopping a reference to a bogus or inaccessible page id from being
    // recorded in the first place.
    let (target, _target_space) = resolve_visible_page(&state.db, &user, body.target_page_id).await?;

    let raw = db::pages::create_page_reference(&state.db, page.organization_id, page.id, target.id).await?;
    Ok(Json(PageReference {
        id: raw.id,
        source_page_id: raw.source_page_id,
        target_page_id: raw.target_page_id,
        target_title: Some(target.title),
        target_space_id: Some(target.space_id),
        created_at: raw.created_at,
    }))
}

#[utoipa::path(
    get,
    path = "/pages/{id}/references",
    responses((status = 200, description = "This page's own outgoing references", body = [PageReference])),
    tag = "pages"
)]
pub async fn list_page_references(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<PageReference>>> {
    let (page, _space) = resolve_visible_page(&state.db, &user, id).await?;
    let raw = db::pages::list_page_references(&state.db, page.organization_id, page.id).await?;

    let mut resolved = Vec::with_capacity(raw.len());
    for r in raw {
        let (target_title, target_space_id) = resolve_other_page(&state, &user, r.target_page_id).await;
        resolved.push(PageReference {
            id: r.id,
            source_page_id: r.source_page_id,
            target_page_id: r.target_page_id,
            target_title,
            target_space_id,
            created_at: r.created_at,
        });
    }
    Ok(Json(resolved))
}

#[utoipa::path(
    get,
    path = "/pages/{id}/backlinks",
    responses((status = 200, description = "Pages that reference this one", body = [PageBacklink])),
    tag = "pages"
)]
pub async fn list_page_backlinks(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<PageBacklink>>> {
    let (page, _space) = resolve_visible_page(&state.db, &user, id).await?;
    let raw = db::pages::list_page_backlinks(&state.db, page.organization_id, page.id).await?;

    let mut resolved = Vec::with_capacity(raw.len());
    for r in raw {
        let (source_title, source_space_id) = resolve_other_page(&state, &user, r.source_page_id).await;
        resolved.push(PageBacklink {
            id: r.id,
            source_page_id: r.source_page_id,
            source_title,
            source_space_id,
            created_at: r.created_at,
        });
    }
    Ok(Json(resolved))
}

#[utoipa::path(
    delete,
    path = "/pages/{id}/references/{reference_id}",
    responses((status = 204, description = "Reference removed")),
    tag = "pages"
)]
pub async fn delete_page_reference(
    State(state): State<AppState>,
    user: TackUser,
    Path((id, reference_id)): Path<(Uuid, Uuid)>,
) -> AppResult<axum::http::StatusCode> {
    let (page, space) = resolve_visible_page(&state.db, &user, id).await?;
    require_edit(&state.db, &user, &page, &space).await?;
    db::pages::delete_page_reference(&state.db, page.organization_id, page.id, reference_id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
