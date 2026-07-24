use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::TackUser;
use crate::db;
use crate::error::{AppError, AppResult};
use crate::models::note::{CreateNoteRequest, Note, NoteRevision, ReplyRequest, UpdateNoteRequest, Visibility};
use crate::AppState;

/// `true` if `user` may see `note`, given its visibility tier — resolved
/// live from the caller's current team/organization memberships on every
/// call, never a cached/denormalized grant (see the architecture plan's
/// "permissions resolved live" decision).
fn can_view(note: &Note, user: &TackUser) -> bool {
    if user.is_admin || note.created_by == user.user_id {
        return true;
    }
    match note.visibility {
        Visibility::Private => false,
        Visibility::Team => note.team_id.is_some_and(|t| user.teams.contains_key(&t)),
        Visibility::Organization => user.organization_ids().contains(&note.organization_id),
    }
}

/// Only the creator or an admin may edit/delete a note — visibility tier
/// (who can *read* it) is a separate question from who can *write* it.
fn can_edit(note: &Note, user: &TackUser) -> bool {
    user.is_admin || note.created_by == user.user_id
}

/// Resolves a bare note id to a `Note`, scoped to whichever of the caller's
/// organizations actually contains it (organization_id — the partition key —
/// isn't in the URL, so this tries each org the caller belongs to; admins
/// additionally get an unscoped fallback). Then enforces `can_view`.
async fn resolve_visible_note(state: &AppState, user: &TackUser, id: Uuid) -> AppResult<Note> {
    for org_id in user.organization_ids() {
        if let Some(note) = db::notes::get_note(&state.db, id, org_id).await? {
            if can_view(&note, user) {
                return Ok(note);
            }
            return Err(AppError::Forbidden("You don't have access to this note.".into()));
        }
    }
    if user.is_admin {
        if let Some(note) = db::notes::get_note_admin_any_org(&state.db, id).await? {
            return Ok(note);
        }
    }
    Err(AppError::NotFound(format!("Note {id} not found")))
}

fn resolve_team_organization(user: &TackUser, team_id: Uuid) -> AppResult<Uuid> {
    let membership = user
        .teams
        .get(&team_id)
        .ok_or_else(|| AppError::Forbidden("You are not a member of this team.".into()))?;
    membership.organization_id.ok_or_else(|| {
        AppError::BadRequest(
            "This team has no organization assigned yet — ask an admin to assign one before creating Tack content here.".into(),
        )
    })
}

#[utoipa::path(
    post,
    path = "/notes",
    request_body = CreateNoteRequest,
    responses((status = 201, description = "Note created", body = Note)),
    tag = "notes"
)]
pub async fn create_note(
    State(state): State<AppState>,
    user: TackUser,
    Json(body): Json<CreateNoteRequest>,
) -> AppResult<Json<Note>> {
    if body.body_markdown.trim().is_empty() {
        return Err(AppError::BadRequest("body_markdown must not be empty".into()));
    }
    let organization_id = resolve_team_organization(&user, body.team_id)?;
    let note = db::notes::create_note(
        &state.db,
        db::notes::NewNote {
            organization_id,
            team_id: body.team_id,
            visibility: body.visibility,
            created_by: user.user_id,
            body_markdown: body.body_markdown,
        },
    )
    .await?;
    Ok(Json(note))
}

#[derive(Debug, Deserialize)]
pub struct ListNotesQuery {
    pub team_id: Uuid,
}

#[utoipa::path(
    get,
    path = "/notes",
    params(("team_id" = Uuid, Query, description = "List top-level notes filed under this team")),
    responses((status = 200, description = "Top-level notes for this team", body = [Note])),
    tag = "notes"
)]
pub async fn list_notes(
    State(state): State<AppState>,
    user: TackUser,
    Query(query): Query<ListNotesQuery>,
) -> AppResult<Json<Vec<Note>>> {
    let organization_id = resolve_team_organization(&user, query.team_id)?;
    let notes = db::notes::list_team_notes(&state.db, organization_id, query.team_id, user.user_id).await?;
    Ok(Json(notes))
}

#[utoipa::path(
    get,
    path = "/notes/{id}",
    responses((status = 200, description = "A note", body = Note)),
    tag = "notes"
)]
pub async fn get_note(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Note>> {
    let note = resolve_visible_note(&state, &user, id).await?;
    Ok(Json(note))
}

#[utoipa::path(
    patch,
    path = "/notes/{id}",
    request_body = UpdateNoteRequest,
    responses((status = 200, description = "Updated note", body = Note)),
    tag = "notes"
)]
pub async fn update_note(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<UpdateNoteRequest>,
) -> AppResult<Json<Note>> {
    let note = resolve_visible_note(&state, &user, id).await?;
    if !can_edit(&note, &user) {
        return Err(AppError::Forbidden("Only the creator or an admin can edit this note.".into()));
    }
    if let Some(ref body_markdown) = body.body_markdown {
        if body_markdown.trim().is_empty() {
            return Err(AppError::BadRequest("body_markdown must not be empty".into()));
        }
    }
    let updated = db::notes::update_note(
        &state.db,
        &note,
        body.body_markdown.as_deref(),
        body.visibility,
        user.user_id,
    )
    .await?;
    Ok(Json(updated))
}

#[utoipa::path(
    delete,
    path = "/notes/{id}",
    responses((status = 204, description = "Note soft-deleted")),
    tag = "notes"
)]
pub async fn delete_note(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<axum::http::StatusCode> {
    let note = resolve_visible_note(&state, &user, id).await?;
    if !can_edit(&note, &user) {
        return Err(AppError::Forbidden("Only the creator or an admin can delete this note.".into()));
    }
    db::notes::soft_delete_note(&state.db, &note).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[utoipa::path(
    post,
    path = "/notes/{id}/replies",
    request_body = ReplyRequest,
    responses((status = 201, description = "Reply created", body = Note)),
    tag = "notes"
)]
pub async fn create_reply(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
    Json(body): Json<ReplyRequest>,
) -> AppResult<Json<Note>> {
    if body.body_markdown.trim().is_empty() {
        return Err(AppError::BadRequest("body_markdown must not be empty".into()));
    }
    let parent = resolve_visible_note(&state, &user, id).await?;
    let reply = db::notes::create_reply(&state.db, &parent, user.user_id, &body.body_markdown).await?;
    Ok(Json(reply))
}

#[utoipa::path(
    get,
    path = "/notes/{id}/replies",
    responses((status = 200, description = "Replies to a note, oldest first", body = [Note])),
    tag = "notes"
)]
pub async fn list_replies(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<Note>>> {
    let parent = resolve_visible_note(&state, &user, id).await?;
    let replies = db::notes::list_replies(&state.db, parent.id, parent.organization_id).await?;
    Ok(Json(replies))
}

#[utoipa::path(
    get,
    path = "/notes/{id}/revisions",
    responses((status = 200, description = "Revision history, newest first", body = [NoteRevision])),
    tag = "notes"
)]
pub async fn list_revisions(
    State(state): State<AppState>,
    user: TackUser,
    Path(id): Path<Uuid>,
) -> AppResult<Json<Vec<NoteRevision>>> {
    let note = resolve_visible_note(&state, &user, id).await?;
    let revisions = db::notes::list_revisions(&state.db, note.id, note.organization_id).await?;
    Ok(Json(revisions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::TackTeamMembership;
    use chrono::Utc;
    use std::collections::HashMap;

    fn note(org: Uuid, team: Option<Uuid>, visibility: Visibility, created_by: Uuid) -> Note {
        Note {
            id: Uuid::new_v4(),
            organization_id: org,
            team_id: team,
            parent_id: None,
            visibility,
            body_markdown: "body".into(),
            created_by,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            reply_count: 0,
        }
    }

    fn user(is_admin: bool, teams: HashMap<Uuid, TackTeamMembership>) -> TackUser {
        TackUser { user_id: Uuid::new_v4(), is_admin, teams }
    }

    #[test]
    fn creator_can_always_view_their_own_private_note() {
        let creator_id = Uuid::new_v4();
        let n = note(Uuid::new_v4(), None, Visibility::Private, creator_id);
        let mut u = user(false, HashMap::new());
        u.user_id = creator_id;
        assert!(can_view(&n, &u));
    }

    #[test]
    fn stranger_cannot_view_a_private_note() {
        let n = note(Uuid::new_v4(), None, Visibility::Private, Uuid::new_v4());
        let u = user(false, HashMap::new());
        assert!(!can_view(&n, &u));
    }

    #[test]
    fn admin_can_view_any_note_regardless_of_visibility() {
        let n = note(Uuid::new_v4(), None, Visibility::Private, Uuid::new_v4());
        let u = user(true, HashMap::new());
        assert!(can_view(&n, &u));
    }

    #[test]
    fn team_member_can_view_a_team_visibility_note() {
        let team_id = Uuid::new_v4();
        let n = note(Uuid::new_v4(), Some(team_id), Visibility::Team, Uuid::new_v4());
        let mut teams = HashMap::new();
        teams.insert(team_id, TackTeamMembership { role: "member".into(), organization_id: None });
        let u = user(false, teams);
        assert!(can_view(&n, &u));
    }

    #[test]
    fn non_member_cannot_view_a_team_visibility_note() {
        let n = note(Uuid::new_v4(), Some(Uuid::new_v4()), Visibility::Team, Uuid::new_v4());
        let u = user(false, HashMap::new());
        assert!(!can_view(&n, &u));
    }

    #[test]
    fn org_member_can_view_an_organization_visibility_note_via_a_different_team() {
        let org = Uuid::new_v4();
        // Note filed under some team in `org`; viewer belongs to a *different*
        // team, also in `org` — organization visibility must not require the
        // same team, only the same organization.
        let n = note(org, Some(Uuid::new_v4()), Visibility::Organization, Uuid::new_v4());
        let mut teams = HashMap::new();
        teams.insert(Uuid::new_v4(), TackTeamMembership { role: "member".into(), organization_id: Some(org) });
        let u = user(false, teams);
        assert!(can_view(&n, &u));
    }

    #[test]
    fn outsider_org_cannot_view_an_organization_visibility_note() {
        let n = note(Uuid::new_v4(), Some(Uuid::new_v4()), Visibility::Organization, Uuid::new_v4());
        let mut teams = HashMap::new();
        teams.insert(
            Uuid::new_v4(),
            TackTeamMembership { role: "member".into(), organization_id: Some(Uuid::new_v4()) },
        );
        let u = user(false, teams);
        assert!(!can_view(&n, &u));
    }

    #[test]
    fn only_creator_or_admin_can_edit() {
        let creator_id = Uuid::new_v4();
        let n = note(Uuid::new_v4(), None, Visibility::Organization, creator_id);

        let mut creator = user(false, HashMap::new());
        creator.user_id = creator_id;
        assert!(can_edit(&n, &creator));

        let admin = user(true, HashMap::new());
        assert!(can_edit(&n, &admin));

        let team_id = n.team_id.unwrap_or_else(Uuid::new_v4);
        let mut teams = HashMap::new();
        teams.insert(team_id, TackTeamMembership { role: "member".into(), organization_id: Some(n.organization_id) });
        let other_member = user(false, teams);
        assert!(!can_edit(&n, &other_member), "a team member who isn't the creator must not be able to edit");
    }
}
