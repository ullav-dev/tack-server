//! Idea Boards: a canvas whiteboard (stickies, shapes, a directed link
//! graph) built on top of Notes/note_folders -- see `011_idea_boards.sql`'s
//! header for the schema, and this handler module's own ACL doc comment
//! below for why it deliberately does *not* reuse `notes_acl`.
//!
//! A board is a `note_folders` row with `folder_type = 'ideas_board'`.
//! Every board endpoint here resolves the folder via
//! `handlers::note_folders::resolve_folder_for_caller` (team-membership
//! gate, same as any other folder) and then re-checks `folder_type` so a
//! plain Notes folder id can't be operated on through `/idea-boards/*`, and
//! vice versa -- both cases 404, not 400, to avoid disclosing which kind of
//! folder an id the caller can't otherwise resolve belongs to.
//!
//! ## ACL: board-team-membership, not `notes_acl`
//!
//! Boards themselves follow the exact same "any member of the team may
//! create/rename/delete" rule as an ordinary Notes folder (`note_folders`
//! carries no `created_by`, so there's no creator-only tier to enforce --
//! this is a deliberate divergence from awe-server's own creator-only board
//! ACL, judged consistent with how tack already treats every other folder).
//!
//! Stickies/shapes/links are explicitly collaborative -- "any user with
//! board access may update/delete any sticky," per awe-server's own doc
//! comment on this exact behavior, carried over unchanged here. A sticky's
//! underlying note is therefore never gated by `notes_acl::can_edit`
//! (creator-or-admin-only) the way an ordinary note is: every mutation here
//! instead re-resolves the board via `resolve_board_for_caller` and stops
//! there. `db::idea_boards::update_sticky`/`delete_sticky` intentionally
//! never call into `notes_acl`.

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::{RawBearerToken, TackUser};
use crate::db;
use crate::error::{AppError, AppResult};
use crate::handlers::note_folders::resolve_folder_for_caller;
use crate::models::idea_board::{
    validate_port, validate_shape_type, validate_sticky_color, BoardShape, BoardShapesPage, CreateBoardShapeRequest,
    CreateNoteLinkRequest, CreateStickyRequest, IdeaBoard, IdeaBoardsPage, NoteLink, NoteLinksPage, Sticky, StickiesPage,
    UpdateBoardShapeRequest, UpdateNoteLinkRequest, UpdateStickyRequest,
};
use crate::models::note::{CreateNoteFolderRequest, FolderType, UpdateNoteFolderRequest};
use crate::notes_acl::resolve_team_organization_live;
use crate::AppState;

const DEFAULT_LIMIT: i64 = 25;
const MAX_LIMIT: i64 = 100;

fn clamp_page(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    (limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT), offset.unwrap_or(0).max(0))
}

/// Backfill-only `created_by` resolution, shared by `create_sticky`/
/// `create_shape`/`create_link` -- exact same rule as
/// `handlers::notes::create_note`'s: an admin may attribute to anyone, a
/// non-admin may attribute to an *existing system principal in this same
/// organization* (verified by a real DB lookup, never trusted blindly), and
/// any other claimed `created_by` from a non-admin is silently ignored.
async fn resolve_backfill_created_by(
    state: &AppState,
    user: &TackUser,
    organization_id: Uuid,
    claimed: Option<Uuid>,
) -> AppResult<Uuid> {
    Ok(if user.is_admin {
        claimed.unwrap_or(user.user_id)
    } else if let Some(claimed) = claimed {
        match db::system_principals::get_principal(&state.db, claimed, organization_id).await? {
            Some(_) => claimed,
            None => user.user_id,
        }
    } else {
        user.user_id
    })
}

/// Resolves a bare id to a board, 404ing (not 403/400) if it exists but is
/// an ordinary Notes folder -- see this module's doc comment.
async fn resolve_board_for_caller(state: &AppState, user: &TackUser, id: Uuid) -> AppResult<IdeaBoard> {
    let folder = resolve_folder_for_caller(state, user, id).await?;
    if folder.folder_type != "ideas_board" {
        return Err(AppError::NotFound(format!("Board {id} not found")));
    }
    Ok(folder)
}

// ── Boards ───────────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/idea-boards",
    request_body = CreateNoteFolderRequest,
    responses((status = 201, description = "Board created", body = NoteFolder)),
    tag = "idea-boards"
)]
pub async fn create_board(
    State(state): State<AppState>,
    user: TackUser,
    RawBearerToken(raw_token): RawBearerToken,
    Json(body): Json<CreateNoteFolderRequest>,
) -> AppResult<Json<IdeaBoard>> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    let organization_id = resolve_team_organization_live(&state, &user, &raw_token, body.team_id).await?;
    let board = db::note_folders::create_folder(
        &state.db,
        organization_id,
        body.team_id,
        name,
        user.user_id,
        body.attach.as_ref(),
        FolderType::IdeasBoard,
    )
    .await?;
    Ok(Json(board))
}

#[derive(Debug, Deserialize)]
pub struct ListBoardsQuery {
    pub team_id: Option<Uuid>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/idea-boards",
    params(
        ("team_id" = Option<Uuid>, Query, description = "Only this team's boards, if given"),
        ("limit" = Option<i64>, Query, description = "Page size, default 25, max 100"),
        ("offset" = Option<i64>, Query, description = "Offset into the (alphabetical) list, default 0"),
    ),
    responses((status = 200, description = "A page of Idea Boards visible to the caller", body = IdeaBoardsPage)),
    tag = "idea-boards"
)]
pub async fn list_boards(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<ListBoardsQuery>,
) -> AppResult<Json<IdeaBoardsPage>> {
    let team_ids: Vec<Uuid> = match query.team_id {
        Some(id) => {
            if !user.teams.contains_key(&id) {
                return Err(AppError::Forbidden("You are not a member of this team.".into()));
            }
            vec![id]
        }
        None => user.teams.keys().copied().collect(),
    };
    let (limit, offset) = clamp_page(query.limit, query.offset);
    let mut boards = Vec::new();
    let mut total = 0;
    for org_id in user.organization_ids() {
        let (org_boards, org_total) =
            db::idea_boards::list_boards_for_teams(&state.db, org_id, &team_ids, user.user_id, limit, offset).await?;
        boards.extend(org_boards);
        total += org_total;
    }
    Ok(Json(IdeaBoardsPage { boards, total }))
}

#[derive(Debug, Deserialize)]
pub struct BoardsByEntityQuery {
    pub owning_service: String,
    pub entity_type: String,
    pub entity_id: String,
}

/// One entity's own boards (e.g. a togra project's Idea Board) -- the board
/// analogue of `GET /note-folders/by-entity`, kept as a separate endpoint
/// (not a `folder_type` query param on the general one) so a board can
/// never accidentally surface in the ordinary Notes folder picker or vice
/// versa -- see `db::note_folders::list_folders_for_entity`'s doc comment.
#[utoipa::path(
    get,
    path = "/idea-boards/by-entity",
    params(
        ("owning_service" = String, Query, description = "Namespacing service, e.g. \"togra\""),
        ("entity_type" = String, Query, description = "e.g. \"project\""),
        ("entity_id" = String, Query, description = "The external entity's own id"),
    ),
    responses((status = 200, description = "This entity's own boards", body = [NoteFolder])),
    tag = "idea-boards"
)]
pub async fn list_boards_by_entity(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<BoardsByEntityQuery>,
) -> AppResult<Json<Vec<IdeaBoard>>> {
    for organization_id in user.organization_ids() {
        let boards = db::note_folders::list_folders_for_entity(
            &state.db,
            organization_id,
            &query.owning_service,
            &query.entity_type,
            &query.entity_id,
            user.user_id,
            "ideas_board",
        )
        .await?;
        let visible: Vec<_> = boards.into_iter().filter(|b| user.is_admin || user.teams.contains_key(&b.team_id)).collect();
        if !visible.is_empty() {
            return Ok(Json(visible));
        }
    }
    Ok(Json(Vec::new()))
}

#[utoipa::path(
    get,
    path = "/idea-boards/{id}",
    responses((status = 200, description = "A board", body = NoteFolder)),
    tag = "idea-boards"
)]
pub async fn get_board(State(state): State<AppState>, user: TackUser, Path(id): Path<Uuid>) -> AppResult<Json<IdeaBoard>> {
    let board = resolve_board_for_caller(&state, &user, id).await?;
    Ok(Json(board))
}

#[utoipa::path(
    patch,
    path = "/idea-boards/{id}",
    request_body = UpdateNoteFolderRequest,
    responses((status = 200, description = "Renamed board", body = NoteFolder)),
    tag = "idea-boards"
)]
pub async fn update_board(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateNoteFolderRequest>,
) -> AppResult<Json<IdeaBoard>> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("name must not be empty".into()));
    }
    let board = resolve_board_for_caller(&state, &user, id).await?;
    let updated = db::note_folders::rename_folder(&state.db, &board, name, user.user_id).await?;
    Ok(Json(updated))
}

#[utoipa::path(
    delete,
    path = "/idea-boards/{id}",
    responses((status = 204, description = "Board deleted, along with its stickies' notes, shapes, and links")),
    tag = "idea-boards"
)]
pub async fn delete_board(State(state): State<AppState>, user: TackUser, Path(id): Path<Uuid>) -> AppResult<axum::http::StatusCode> {
    let board = resolve_board_for_caller(&state, &user, id).await?;
    db::idea_boards::delete_board(&state.db, &board).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ── Stickies ─────────────────────────────────────────────────────────────

const DEFAULT_STICKY_X: f64 = 40.0;
const DEFAULT_STICKY_Y: f64 = 40.0;
const DEFAULT_STICKY_COLOR: &str = "yellow";
const DEFAULT_STICKY_WIDTH: f64 = 220.0;
const DEFAULT_STICKY_HEIGHT: f64 = 160.0;

#[utoipa::path(
    post,
    path = "/idea-boards/{id}/stickies",
    request_body = CreateStickyRequest,
    responses((status = 201, description = "Sticky created", body = Sticky)),
    tag = "idea-boards"
)]
pub async fn create_sticky(
    State(state): State<AppState>,
    user: TackUser,
    Path(board_id): Path<Uuid>,
    Json(body): Json<CreateStickyRequest>,
) -> AppResult<Json<Sticky>> {
    if body.title.trim().is_empty() {
        return Err(AppError::BadRequest("title must not be empty".into()));
    }
    let board = resolve_board_for_caller(&state, &user, board_id).await?;
    let color = body.color.unwrap_or_else(|| DEFAULT_STICKY_COLOR.to_string());
    if !validate_sticky_color(&color) {
        return Err(AppError::BadRequest(format!("Invalid sticky color: {color}")));
    }
    if body.linked_entity_type.is_some() != body.linked_entity_id.is_some() {
        return Err(AppError::BadRequest("linked_entity_type and linked_entity_id must be set together.".into()));
    }
    let created_by = resolve_backfill_created_by(&state, &user, board.organization_id, body.created_by).await?;
    // Backfill-only: only an admin caller's created_at override is honored,
    // same as CreateNoteRequest::created_at.
    let created_at = if user.is_admin { body.created_at.unwrap_or_else(chrono::Utc::now) } else { chrono::Utc::now() };
    let sticky = db::idea_boards::create_sticky(
        &state.db,
        &board,
        db::idea_boards::NewSticky {
            title: body.title,
            body_markdown: body.body_markdown,
            x: body.x.unwrap_or(DEFAULT_STICKY_X),
            y: body.y.unwrap_or(DEFAULT_STICKY_Y),
            color,
            width: body.width.unwrap_or(DEFAULT_STICKY_WIDTH),
            height: body.height.unwrap_or(DEFAULT_STICKY_HEIGHT),
            linked_entity_type: body.linked_entity_type,
            linked_entity_id: body.linked_entity_id,
        },
        created_by,
        created_at,
    )
    .await?;
    Ok(Json(sticky))
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[utoipa::path(
    get,
    path = "/idea-boards/{id}/stickies",
    params(
        ("limit" = Option<i64>, Query, description = "Page size, default 25, max 100"),
        ("offset" = Option<i64>, Query, description = "Offset, default 0"),
    ),
    responses((status = 200, description = "A page of this board's stickies", body = StickiesPage)),
    tag = "idea-boards"
)]
pub async fn list_stickies(
    State(state): State<AppState>,
    user: TackUser,
    Path(board_id): Path<Uuid>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<StickiesPage>> {
    let board = resolve_board_for_caller(&state, &user, board_id).await?;
    let (limit, offset) = clamp_page(query.limit, query.offset);
    let (stickies, total) = db::idea_boards::list_stickies_for_board(&state.db, board.id, board.organization_id, limit, offset).await?;
    Ok(Json(StickiesPage { stickies, total }))
}

#[utoipa::path(
    get,
    path = "/stickies/{note_id}",
    responses((status = 200, description = "A single sticky", body = Sticky)),
    tag = "idea-boards"
)]
pub async fn get_sticky(State(state): State<AppState>, user: TackUser, Path(note_id): Path<Uuid>) -> AppResult<Json<Sticky>> {
    let sticky = resolve_sticky_for_caller(&state, &user, note_id).await?;
    Ok(Json(sticky))
}

/// Resolves a sticky by its note id, verifying its board is visible to the
/// caller (team-membership) -- shared by every mutating sticky endpoint
/// below.
async fn resolve_sticky_for_caller(state: &AppState, user: &TackUser, note_id: Uuid) -> AppResult<Sticky> {
    for org_id in user.organization_ids() {
        if let Some(sticky) = db::idea_boards::get_sticky(&state.db, note_id, org_id).await? {
            resolve_board_for_caller(state, user, sticky.board_id).await?;
            return Ok(sticky);
        }
    }
    Err(AppError::NotFound(format!("Sticky {note_id} not found")))
}

#[derive(Debug, Deserialize)]
pub struct StickyByEntityQuery {
    pub entity_type: String,
    pub entity_id: String,
}

/// The tack-server equivalent of awe-server's `get_sticky_by_workflow`,
/// generalized to any linked entity type -- see `Sticky::linked_entity_type`.
#[utoipa::path(
    get,
    path = "/stickies/by-entity",
    params(
        ("entity_type" = String, Query, description = "e.g. \"workflow\""),
        ("entity_id" = String, Query, description = "The external entity's own id"),
    ),
    responses((status = 200, description = "The sticky linked to this entity, if any", body = Option<Sticky>)),
    tag = "idea-boards"
)]
pub async fn get_sticky_by_entity(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<StickyByEntityQuery>,
) -> AppResult<Json<Option<Sticky>>> {
    for org_id in user.organization_ids() {
        if let Some(sticky) = db::idea_boards::get_sticky_by_entity(&state.db, org_id, &query.entity_type, &query.entity_id).await? {
            // Same team-membership check as every other sticky read --
            // `resolve_board_for_caller` 404s if the caller isn't on the
            // board's team, which we translate to "no sticky" here rather
            // than leaking existence across teams.
            if resolve_board_for_caller(&state, &user, sticky.board_id).await.is_ok() {
                return Ok(Json(Some(sticky)));
            }
        }
    }
    Ok(Json(None))
}

#[utoipa::path(
    patch,
    path = "/stickies/{note_id}",
    request_body = UpdateStickyRequest,
    responses((status = 200, description = "Updated sticky", body = Sticky)),
    tag = "idea-boards"
)]
pub async fn update_sticky(
    State(state): State<AppState>,
    user: TackUser,
    Path(note_id): Path<Uuid>,
    Json(body): Json<UpdateStickyRequest>,
) -> AppResult<Json<Sticky>> {
    let sticky = resolve_sticky_for_caller(&state, &user, note_id).await?;
    if let Some(color) = &body.color {
        if !validate_sticky_color(color) {
            return Err(AppError::BadRequest(format!("Invalid sticky color: {color}")));
        }
    }
    let updated = db::idea_boards::update_sticky(
        &state.db,
        &sticky,
        user.user_id,
        db::idea_boards::StickyUpdate {
            title: body.title,
            body_markdown: body.body_markdown,
            x: body.x,
            y: body.y,
            color: body.color,
            width: body.width,
            height: body.height,
            linked_entity_type: body.linked_entity_type,
            linked_entity_id: body.linked_entity_id,
        },
    )
    .await?;
    Ok(Json(updated))
}

#[utoipa::path(
    delete,
    path = "/stickies/{note_id}",
    responses((status = 204, description = "Sticky (and its note) deleted")),
    tag = "idea-boards"
)]
pub async fn delete_sticky(State(state): State<AppState>, user: TackUser, Path(note_id): Path<Uuid>) -> AppResult<axum::http::StatusCode> {
    let sticky = resolve_sticky_for_caller(&state, &user, note_id).await?;
    db::idea_boards::delete_sticky(&state.db, &sticky).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ── Shapes ───────────────────────────────────────────────────────────────

#[utoipa::path(
    post,
    path = "/idea-boards/{id}/shapes",
    request_body = CreateBoardShapeRequest,
    responses((status = 201, description = "Shape created", body = BoardShape)),
    tag = "idea-boards"
)]
pub async fn create_shape(
    State(state): State<AppState>,
    user: TackUser,
    Path(board_id): Path<Uuid>,
    Json(body): Json<CreateBoardShapeRequest>,
) -> AppResult<Json<BoardShape>> {
    if !validate_shape_type(&body.shape_type) {
        return Err(AppError::BadRequest(format!("Invalid shape_type: {}", body.shape_type)));
    }
    let board = resolve_board_for_caller(&state, &user, board_id).await?;
    let created_by = resolve_backfill_created_by(&state, &user, board.organization_id, body.created_by).await?;
    let created_at = if user.is_admin { body.created_at.unwrap_or_else(chrono::Utc::now) } else { chrono::Utc::now() };
    let shape = db::idea_boards::create_shape(
        &state.db,
        &board,
        db::idea_boards::NewShape {
            shape_type: body.shape_type,
            x: body.x,
            y: body.y,
            width: body.width,
            height: body.height,
            fill_color: body.fill_color,
            stroke_color: body.stroke_color,
            stroke_width: body.stroke_width,
            label: body.label,
            label_color: body.label_color,
            label_size: body.label_size,
            image_url: body.image_url,
        },
        created_by,
        created_at,
    )
    .await?;
    Ok(Json(shape))
}

#[utoipa::path(
    get,
    path = "/idea-boards/{id}/shapes",
    params(
        ("limit" = Option<i64>, Query, description = "Page size, default 25, max 100"),
        ("offset" = Option<i64>, Query, description = "Offset, default 0"),
    ),
    responses((status = 200, description = "A page of this board's shapes", body = BoardShapesPage)),
    tag = "idea-boards"
)]
pub async fn list_shapes(
    State(state): State<AppState>,
    user: TackUser,
    Path(board_id): Path<Uuid>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<BoardShapesPage>> {
    let board = resolve_board_for_caller(&state, &user, board_id).await?;
    let (limit, offset) = clamp_page(query.limit, query.offset);
    let (shapes, total) = db::idea_boards::list_shapes_for_board(&state.db, board.id, board.organization_id, limit, offset).await?;
    Ok(Json(BoardShapesPage { shapes, total }))
}

/// Same "scan the caller's orgs, then check board team membership" shape as
/// `resolve_sticky_for_caller`/`resolve_link_for_caller` -- no separate
/// admin-any-org fallback is needed here, unlike `resolve_folder_for_caller`,
/// since `organization_ids()` already includes every org an admin
/// administers.
async fn resolve_shape_for_caller(state: &AppState, user: &TackUser, id: Uuid) -> AppResult<BoardShape> {
    for org_id in user.organization_ids() {
        if let Some(shape) = db::idea_boards::get_shape(&state.db, id, org_id).await? {
            resolve_board_for_caller(state, user, shape.board_id).await?;
            return Ok(shape);
        }
    }
    Err(AppError::NotFound(format!("Shape {id} not found")))
}

#[utoipa::path(
    patch,
    path = "/shapes/{id}",
    request_body = UpdateBoardShapeRequest,
    responses((status = 200, description = "Updated shape", body = BoardShape)),
    tag = "idea-boards"
)]
pub async fn update_shape(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateBoardShapeRequest>,
) -> AppResult<Json<BoardShape>> {
    let shape = resolve_shape_for_caller(&state, &user, id).await?;
    let updated = db::idea_boards::update_shape(
        &state.db,
        &shape,
        db::idea_boards::ShapeUpdate {
            x: body.x,
            y: body.y,
            width: body.width,
            height: body.height,
            fill_color: body.fill_color,
            stroke_color: body.stroke_color,
            stroke_width: body.stroke_width,
            label: body.label,
            label_color: body.label_color,
            label_size: body.label_size,
            image_url: body.image_url,
        },
    )
    .await?;
    Ok(Json(updated))
}

#[utoipa::path(
    delete,
    path = "/shapes/{id}",
    responses((status = 204, description = "Shape deleted")),
    tag = "idea-boards"
)]
pub async fn delete_shape(State(state): State<AppState>, user: TackUser, Path(id): Path<Uuid>) -> AppResult<axum::http::StatusCode> {
    let shape = resolve_shape_for_caller(&state, &user, id).await?;
    db::idea_boards::delete_shape(&state.db, &shape).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ── Links ────────────────────────────────────────────────────────────────

fn validate_endpoints(body: &CreateNoteLinkRequest) -> AppResult<()> {
    if body.from_note_id.is_some() == body.from_shape_id.is_some() {
        return Err(AppError::BadRequest("Exactly one of from_note_id/from_shape_id must be set.".into()));
    }
    if body.to_note_id.is_some() == body.to_shape_id.is_some() {
        return Err(AppError::BadRequest("Exactly one of to_note_id/to_shape_id must be set.".into()));
    }
    if let Some(port) = &body.from_port {
        if !validate_port(port) {
            return Err(AppError::BadRequest(format!("Invalid from_port: {port}")));
        }
    }
    if let Some(port) = &body.to_port {
        if !validate_port(port) {
            return Err(AppError::BadRequest(format!("Invalid to_port: {port}")));
        }
    }
    Ok(())
}

#[utoipa::path(
    post,
    path = "/idea-boards/{id}/links",
    request_body = CreateNoteLinkRequest,
    responses((status = 201, description = "Link created", body = NoteLink)),
    tag = "idea-boards"
)]
pub async fn create_link(
    State(state): State<AppState>,
    user: TackUser,
    Path(board_id): Path<Uuid>,
    Json(body): Json<CreateNoteLinkRequest>,
) -> AppResult<Json<NoteLink>> {
    validate_endpoints(&body)?;
    let board = resolve_board_for_caller(&state, &user, board_id).await?;

    if let Some(note_id) = body.from_note_id {
        if !db::idea_boards::note_belongs_to_board(&state.db, note_id, board.organization_id, board.id).await? {
            return Err(AppError::BadRequest("from_note_id does not belong to this board.".into()));
        }
    }
    if let Some(shape_id) = body.from_shape_id {
        if !db::idea_boards::shape_belongs_to_board(&state.db, shape_id, board.organization_id, board.id).await? {
            return Err(AppError::BadRequest("from_shape_id does not belong to this board.".into()));
        }
    }
    if let Some(note_id) = body.to_note_id {
        if !db::idea_boards::note_belongs_to_board(&state.db, note_id, board.organization_id, board.id).await? {
            return Err(AppError::BadRequest("to_note_id does not belong to this board.".into()));
        }
    }
    if let Some(shape_id) = body.to_shape_id {
        if !db::idea_boards::shape_belongs_to_board(&state.db, shape_id, board.organization_id, board.id).await? {
            return Err(AppError::BadRequest("to_shape_id does not belong to this board.".into()));
        }
    }

    let created_by = resolve_backfill_created_by(&state, &user, board.organization_id, body.created_by).await?;
    let created_at = if user.is_admin { body.created_at.unwrap_or_else(chrono::Utc::now) } else { chrono::Utc::now() };
    let link = db::idea_boards::create_link(
        &state.db,
        &board,
        db::idea_boards::NewLink {
            from_note_id: body.from_note_id,
            from_shape_id: body.from_shape_id,
            to_note_id: body.to_note_id,
            to_shape_id: body.to_shape_id,
            from_port: body.from_port,
            to_port: body.to_port,
            label: body.label,
        },
        created_by,
        created_at,
    )
    .await?;
    Ok(Json(link))
}

#[utoipa::path(
    get,
    path = "/idea-boards/{id}/links",
    params(
        ("limit" = Option<i64>, Query, description = "Page size, default 25, max 100"),
        ("offset" = Option<i64>, Query, description = "Offset, default 0"),
    ),
    responses((status = 200, description = "A page of this board's links", body = NoteLinksPage)),
    tag = "idea-boards"
)]
pub async fn list_links(
    State(state): State<AppState>,
    user: TackUser,
    Path(board_id): Path<Uuid>,
    Query(query): Query<PageQuery>,
) -> AppResult<Json<NoteLinksPage>> {
    let board = resolve_board_for_caller(&state, &user, board_id).await?;
    let (limit, offset) = clamp_page(query.limit, query.offset);
    let (links, total) = db::idea_boards::list_links_for_board(&state.db, board.id, board.organization_id, limit, offset).await?;
    Ok(Json(NoteLinksPage { links, total }))
}

async fn resolve_link_for_caller(state: &AppState, user: &TackUser, id: Uuid) -> AppResult<NoteLink> {
    for org_id in user.organization_ids() {
        if let Some(link) = db::idea_boards::get_link(&state.db, id, org_id).await? {
            resolve_board_for_caller(state, user, link.board_id).await?;
            return Ok(link);
        }
    }
    Err(AppError::NotFound(format!("Link {id} not found")))
}

#[utoipa::path(
    patch,
    path = "/links/{id}",
    request_body = UpdateNoteLinkRequest,
    responses((status = 200, description = "Updated link", body = NoteLink)),
    tag = "idea-boards"
)]
pub async fn update_link(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateNoteLinkRequest>,
) -> AppResult<Json<NoteLink>> {
    if let Some(Some(port)) = &body.from_port {
        if !validate_port(port) {
            return Err(AppError::BadRequest(format!("Invalid from_port: {port}")));
        }
    }
    if let Some(Some(port)) = &body.to_port {
        if !validate_port(port) {
            return Err(AppError::BadRequest(format!("Invalid to_port: {port}")));
        }
    }
    let link = resolve_link_for_caller(&state, &user, id).await?;
    let updated = db::idea_boards::update_link(
        &state.db,
        &link,
        db::idea_boards::LinkUpdate {
            from_port: body.from_port,
            to_port: body.to_port,
            label: body.label,
        },
    )
    .await?;
    Ok(Json(updated))
}

#[utoipa::path(
    delete,
    path = "/links/{id}",
    responses((status = 204, description = "Link deleted")),
    tag = "idea-boards"
)]
pub async fn delete_link(State(state): State<AppState>, user: TackUser, Path(id): Path<Uuid>) -> AppResult<axum::http::StatusCode> {
    let link = resolve_link_for_caller(&state, &user, id).await?;
    db::idea_boards::delete_link(&state.db, &link).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}
