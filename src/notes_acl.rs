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

/// `resolve_team_organization`, extended with an admin-only fallback for a
/// team the caller isn't a JWT-claimed member of.
///
/// `TackUser.teams` is built entirely from the caller's own JWT `teams`
/// claim (`auth::build_tack_user`), which ullav-user-management scopes to
/// actual membership — never "every team" just because the caller is an
/// admin (the JWT claim is a per-user snapshot, not a role check). That's
/// the right rule for a normal caller creating their own content, but it
/// leaves admins with no way to create/backfill content under a team they
/// don't personally belong to -- e.g. a one-off migration script moving
/// content in from another system team-by-team.
///
/// For an admin caller on a team outside their own JWT claims, this
/// resolves the organization live against ullav-user-management's
/// `GET /admin/teams/{id}` (`users:read`-gated there) instead -- arguably
/// more correct even for teams the admin *is* a member of, since the JWT
/// claim is a snapshot from login time and the live endpoint isn't, but the
/// fast, no-network JWT path stays the default for that case rather than
/// paying a live HTTP round-trip on every note creation.
///
/// Forwards the caller's own token to ullav-user-management (`raw_token`) --
/// same "no separate service credential, just the interactive JWT" pattern
/// used throughout this org (see lagan-server's retired proxy). A 403/404
/// from that call means this admin's own token doesn't actually carry
/// `users:read`, or the team doesn't exist -- surfaced here as the same
/// error shape a normal caller would get, not a 500.
pub async fn resolve_team_organization_live(
    state: &crate::AppState,
    user: &TackUser,
    raw_token: &str,
    team_id: Uuid,
) -> AppResult<Uuid> {
    if let Some(membership) = user.teams.get(&team_id) {
        return membership.organization_id.ok_or_else(|| {
            AppError::BadRequest(
                "This team has no organization assigned yet — ask an admin to assign one before creating Tack content here.".into(),
            )
        });
    }
    if !user.is_admin {
        return Err(AppError::Forbidden("You are not a member of this team.".into()));
    }

    #[derive(serde::Deserialize)]
    struct TeamLookup {
        organization_id: Option<Uuid>,
    }

    let resp = state
        .user_management_http
        .get(format!("{}/admin/teams/{team_id}", state.user_management_base_url))
        .bearer_auth(raw_token)
        .send()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("ullav-user-management call failed: {e}")))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(AppError::BadRequest("team_id does not refer to an existing team.".into()));
    }
    if !resp.status().is_success() {
        return Err(AppError::Forbidden(
            "Could not resolve this team's organization (your account may lack ullav-user-management's users:read permission).".into(),
        ));
    }

    let team: TeamLookup = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("malformed ullav-user-management response: {e}")))?;

    team.organization_id.ok_or_else(|| {
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
            folder_id: None,
            visibility,
            title: "title".into(),
            body_markdown: "body".into(),
            created_by,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            reply_count: 0,
            in_reply_to_version: None,
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

    // ── resolve_team_organization_live ──────────────────────────────────────

    /// A minimal `AppState` for these tests -- `db::create_pool` only builds
    /// a deadpool config (no connection attempt until first use), and
    /// neither `SearchClient::new` nor `TokenValidator::new` do any I/O
    /// either, so this never touches Postgres/OpenSearch/JWKS -- only
    /// `user_management_http`/`user_management_base_url` are exercised by
    /// the tests below.
    fn app_state(user_management_base_url: &str) -> crate::AppState {
        crate::AppState {
            db: db::create_pool("postgresql://test:test@localhost/test").expect("pool config"),
            api_validator: ullav_mcp_auth::TokenValidator::new("http://localhost/jwks", "http://localhost", ""),
            search: crate::search::SearchClient::new("http://localhost:9200"),
            embedder: None,
            user_management_http: reqwest::Client::new(),
            user_management_base_url: user_management_base_url.to_string(),
        }
    }

    /// Spawns a one-shot raw-TCP HTTP server: accepts exactly one
    /// connection, asserts the request carries `Authorization: Bearer
    /// <expected_token>`, and replies with `status`/`body`. No mocking
    /// crate needed -- this exercises the real `reqwest` call end to end
    /// against a real socket, not a stubbed client. Returns the bound
    /// `http://127.0.0.1:<port>` base URL.
    async fn spawn_mock_user_management(status: u16, body: String, expected_token: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.expect("read request");
            let request = String::from_utf8_lossy(&buf[..n]);
            assert!(
                request.to_lowercase().contains(&format!("authorization: bearer {}", expected_token.to_lowercase())),
                "expected the caller's own token to be forwarded, got:\n{request}"
            );
            let reason = match status {
                200 => "OK",
                403 => "Forbidden",
                404 => "Not Found",
                _ => "Error",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.expect("write response");
            socket.shutdown().await.ok();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn jwt_member_path_never_makes_a_network_call() {
        let org = Uuid::new_v4();
        let team_id = Uuid::new_v4();
        let mut teams = HashMap::new();
        teams.insert(team_id, TackTeamMembership { role: "member".into(), organization_id: Some(org) });
        let u = user(false, teams);
        // An unreachable base URL proves this path never dials out -- if it
        // did, this would hang/error instead of returning immediately.
        let state = app_state("http://127.0.0.1:1");
        let result = resolve_team_organization_live(&state, &u, "unused-token", team_id).await;
        assert_eq!(result.unwrap(), org);
    }

    #[tokio::test]
    async fn non_admin_outside_the_team_is_forbidden_without_a_network_call() {
        let u = user(false, HashMap::new());
        let state = app_state("http://127.0.0.1:1");
        let result = resolve_team_organization_live(&state, &u, "unused-token", Uuid::new_v4()).await;
        assert!(matches!(result, Err(AppError::Forbidden(_))));
    }

    #[tokio::test]
    async fn admin_outside_the_team_resolves_live_and_forwards_the_callers_token() {
        let org = Uuid::new_v4();
        let base = spawn_mock_user_management(200, format!("{{\"organization_id\":\"{org}\"}}"), "the-callers-token").await;
        let leaked_base: &'static str = Box::leak(base.into_boxed_str());
        let u = user(true, HashMap::new());
        let state = app_state(leaked_base);
        let result = resolve_team_organization_live(&state, &u, "the-callers-token", Uuid::new_v4()).await;
        assert_eq!(result.unwrap(), org);
    }

    #[tokio::test]
    async fn admin_live_lookup_with_no_organization_assigned_is_a_bad_request() {
        let base = spawn_mock_user_management(200, "{\"organization_id\":null}".to_string(), "tok").await;
        let leaked_base: &'static str = Box::leak(base.into_boxed_str());
        let u = user(true, HashMap::new());
        let state = app_state(leaked_base);
        let result = resolve_team_organization_live(&state, &u, "tok", Uuid::new_v4()).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn admin_live_lookup_404_is_a_bad_request_not_a_500() {
        let base = spawn_mock_user_management(404, "{\"error\":\"not found\"}".to_string(), "tok").await;
        let leaked_base: &'static str = Box::leak(base.into_boxed_str());
        let u = user(true, HashMap::new());
        let state = app_state(leaked_base);
        let result = resolve_team_organization_live(&state, &u, "tok", Uuid::new_v4()).await;
        assert!(matches!(result, Err(AppError::BadRequest(_))));
    }

    #[tokio::test]
    async fn admin_live_lookup_403_surfaces_as_forbidden_not_a_500() {
        // The realistic case: the admin's own token lacks
        // ullav-user-management's `users:read` permission.
        let base = spawn_mock_user_management(403, "{\"error\":\"forbidden\"}".to_string(), "tok").await;
        let leaked_base: &'static str = Box::leak(base.into_boxed_str());
        let u = user(true, HashMap::new());
        let state = app_state(leaked_base);
        let result = resolve_team_organization_live(&state, &u, "tok", Uuid::new_v4()).await;
        assert!(matches!(result, Err(AppError::Forbidden(_))));
    }
}
