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
    CreatePagePermissionRequest, CreatePageRequest, Page, PagePermission, PagePermissionLevelResponse, PageRevision,
    UpdatePageRequest,
};
use crate::pages_acl::{
    can_create_in_space, require_edit, resolve_effective_permission, resolve_space, resolve_visible_page,
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

#[derive(Debug, Deserialize)]
pub struct ListPagesQuery {
    /// Omit to list root pages (pages with no parent) in the space.
    pub parent_id: Option<Uuid>,
}

/// Direct children of `parent_id` in this space (or root pages if omitted),
/// filtered to only the ones the caller can view. Each candidate's
/// permission is resolved individually — a child page's own override can
/// make it more (or less) restricted than its siblings.
#[utoipa::path(
    get,
    path = "/spaces/{id}/pages",
    params(("parent_id" = Option<Uuid>, Query, description = "List children of this page, or root pages if omitted")),
    responses((status = 200, description = "Visible pages at this level of the tree", body = [Page])),
    tag = "pages"
)]
pub async fn list_pages(
    State(state): State<AppState>,
    user: TackUser,
    Path(space_id): Path<Uuid>,
    Query(query): Query<ListPagesQuery>,
) -> AppResult<Json<Vec<Page>>> {
    let space = resolve_space(&state.db, &user, space_id).await?;
    let candidates = db::pages::list_children(&state.db, space.organization_id, space_id, query.parent_id).await?;

    let mut visible = Vec::with_capacity(candidates.len());
    for page in candidates {
        let level = resolve_effective_permission(&state.db, &user, &page, &space).await?;
        if level.is_some() {
            visible.push(page);
        }
    }
    Ok(Json(visible))
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
