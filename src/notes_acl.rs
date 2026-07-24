//! Notes access-control logic shared by the REST handlers (`handlers::notes`)
//! and the MCP tool surface (`mcp::server`) — both must enforce exactly the
//! same live-resolved visibility rules, so this lives in one place rather
//! than being duplicated per transport.

use uuid::Uuid;

use crate::auth::TackUser;
use crate::db::{self, DbPool};
use crate::error::{AppError, AppResult};
use crate::models::note::{Note, Visibility};

/// `true` if `user` may see `note`, given its visibility tier — resolved
/// live from the caller's current team/organization memberships on every
/// call, never a cached/denormalized grant (see the architecture plan's
/// "permissions resolved live" decision).
pub fn can_view(note: &Note, user: &TackUser) -> bool {
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
pub fn can_edit(note: &Note, user: &TackUser) -> bool {
    user.is_admin || note.created_by == user.user_id
}

/// Resolves a bare note id to a `Note`, scoped to whichever of the caller's
/// organizations actually contains it (organization_id — the partition key —
/// isn't known up front, so this tries each org the caller belongs to;
/// admins additionally get an unscoped fallback). Then enforces `can_view`.
pub async fn resolve_visible_note(db: &DbPool, user: &TackUser, id: Uuid) -> AppResult<Note> {
    for org_id in user.organization_ids() {
        if let Some(note) = db::notes::get_note(db, id, org_id).await? {
            if can_view(&note, user) {
                return Ok(note);
            }
            return Err(AppError::Forbidden("You don't have access to this note.".into()));
        }
    }
    if user.is_admin {
        if let Some(note) = db::notes::get_note_admin_any_org(db, id).await? {
            return Ok(note);
        }
    }
    Err(AppError::NotFound(format!("Note {id} not found")))
}

/// Resolves the organization to file a new note under, given the team the
/// caller wants to file it in. The team must be one of the caller's
/// Tack-enabled teams, and must already have an organization assigned.
pub fn resolve_team_organization(user: &TackUser, team_id: Uuid) -> AppResult<Uuid> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use crate::auth::TackTeamMembership;
    use chrono::Utc;

    fn note(org: Uuid, team: Option<Uuid>, visibility: Visibility, created_by: Uuid) -> Note {
        Note {
            id: Uuid::new_v4(),
            organization_id: org,
            team_id: team,
            parent_id: None,
            visibility,
            title: "title".into(),
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
