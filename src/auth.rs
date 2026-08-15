use axum::{
    extract::{FromRef, FromRequestParts},
    http::{header, request::Parts},
};
use serde::Deserialize;
use std::collections::HashMap;
use uuid::Uuid;

use crate::{error::AppError, AppState};

// ── JWT claims (matches ullav-user-management's token shape) ──────────────────

#[derive(Debug, Deserialize)]
struct TeamClaim {
    #[serde(default)]
    role: String,
    /// The team's organization, if it has one — see the Organizations migration
    /// in ullav-user-management. Most teams don't have one yet; that's expected.
    #[serde(default)]
    organization_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    teams: HashMap<String, TeamClaim>,
}

// ── TackUser extractor ─────────────────────────────────────────────────────────

/// One team the caller belongs to, with its organization if it has one.
#[derive(Debug, Clone)]
pub struct TackTeamMembership {
    pub role: String,
    pub organization_id: Option<Uuid>,
}

/// Authenticated Tack user extracted from the `Authorization: Bearer` header.
///
/// Requires the caller to be an admin, or a member of at least one team —
/// any team, full stop. **Not** gated on a `tack` product slug being
/// enabled per-team, unlike every other first-party Ullav app's own gate
/// (Togra/Cunav/Cartlann/Lagan still do their own) — deliberately removed
/// here: Tack backs Notes/Pages for all of those apps now, so an opt-in
/// per-team product gate stopped making sense as soon as it became load-
/// bearing infrastructure rather than a standalone product a team chooses
/// to adopt. Every *other* access decision (which notes/pages a caller can
/// actually see) is completely unaffected by this and still fully enforced
/// live, per-request, in `notes_acl.rs`/`pages_acl.rs` — this only ever
/// gated whether the caller gets a `TackUser` at all, never which content
/// within Tack they could see once they had one.
#[derive(Debug, Clone)]
pub struct TackUser {
    pub user_id: Uuid,
    pub is_admin: bool,
    /// Every team the caller belongs to (no product-gate filtering — see
    /// this struct's own doc comment).
    pub teams: HashMap<Uuid, TackTeamMembership>,
}

impl TackUser {
    /// Distinct organization ids across the caller's Tack-enabled teams.
    /// A user can belong to teams in more than one organization.
    pub fn organization_ids(&self) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> =
            self.teams.values().filter_map(|t| t.organization_id).collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

/// Shared "sub/roles/teams -> TackUser" logic, used by both the REST
/// `FromRequestParts` extractor (general API JWT) and `from_mcp_claims`
/// (audience-bound MCP token) — same access-gate rules either way.
///
/// No product-gate filtering on `teams` — see `TackUser`'s own doc comment
/// for why. Every team present in the token's own claims is kept as-is;
/// `ullav-user-management` (the token issuer) is still the one deciding
/// which teams a caller belongs to at all, this just no longer narrows
/// that down further by product enablement.
fn build_tack_user<'a>(
    sub: &str,
    roles: &[String],
    teams: impl IntoIterator<Item = (&'a String, &'a str, Option<&'a str>)>,
) -> Result<TackUser, AppError> {
    let user_id = sub.parse::<Uuid>().map_err(|_| AppError::Unauthorized("Invalid subject in token".into()))?;
    let is_admin = roles.iter().any(|r| r == "admin");

    let team_map: HashMap<Uuid, TackTeamMembership> = teams
        .into_iter()
        .filter_map(|(id, role, organization_id)| {
            let team_id = id.parse::<Uuid>().ok()?;
            Some((
                team_id,
                TackTeamMembership {
                    role: role.to_string(),
                    organization_id: organization_id.and_then(|s| s.parse().ok()),
                },
            ))
        })
        .collect();

    if !is_admin && team_map.is_empty() {
        return Err(AppError::Forbidden("Your account does not belong to any team.".into()));
    }

    Ok(TackUser { user_id, is_admin, teams: team_map })
}

/// Builds a `TackUser` from an audience-bound MCP token's claims (see
/// `ullav_mcp_auth::McpClaims`) — same access-gate rules as the REST
/// `FromRequestParts` extractor, just sourced from the MCP claims shape
/// instead of decoding a fresh general-API JWT.
pub fn from_mcp_claims(claims: &ullav_mcp_auth::McpClaims) -> Result<TackUser, AppError> {
    build_tack_user(
        &claims.sub,
        &claims.roles,
        claims.teams.iter().map(|(id, t)| (id, t.role.as_str(), t.organization_id.as_deref())),
    )
}

#[axum::async_trait]
impl<S> FromRequestParts<S> for TackUser
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);

        let auth_header = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| AppError::Unauthorized("Missing Authorization header".into()))?;

        let token = auth_header
            .strip_prefix("Bearer ")
            .ok_or_else(|| AppError::Unauthorized("Authorization must use Bearer scheme".into()))?;

        let claims = app_state
            .api_validator
            .validate_as::<Claims>(token)
            .await
            .map_err(|e| AppError::Unauthorized(format!("Invalid token: {e}")))?;

        build_tack_user(
            &claims.sub,
            &claims.roles,
            claims.teams.iter().map(|(id, t)| (id, t.role.as_str(), t.organization_id.as_deref())),
        )
    }
}

/// The caller's raw bearer token, unvalidated by this extractor itself (a
/// handler that wants this alongside `TackUser` gets both -- `TackUser`'s
/// own extraction already validates it). Exists solely to forward the
/// caller's own token to another service call the caller is implicitly
/// authorizing by making this request -- currently just
/// `notes_acl::resolve_team_organization_live`'s admin cross-team lookup
/// against ullav-user-management, same "forward the interactive JWT
/// as-is, no separate service credential" pattern lagan-server used for its
/// (now-retired) tack-server proxy.
pub struct RawBearerToken(pub String);

#[axum::async_trait]
impl<S> FromRequestParts<S> for RawBearerToken
where
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| AppError::Unauthorized("Missing Authorization header".into()))?;
        let token = auth_header
            .strip_prefix("Bearer ")
            .ok_or_else(|| AppError::Unauthorized("Authorization must use Bearer scheme".into()))?;
        Ok(RawBearerToken(token.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_tack_user_grants_access_to_any_team_no_product_gate() {
        // The whole point of this change: a team with no "tack" product
        // slug at all (there's no such concept passed in anymore) still
        // grants a non-admin caller a TackUser, with that team present.
        let team_id = Uuid::new_v4().to_string();
        let user = build_tack_user(
            &Uuid::new_v4().to_string(),
            &[],
            [(&team_id, "member", None)],
        )
        .expect("a caller with any team membership should be granted access");
        assert!(!user.is_admin);
        assert_eq!(user.teams.len(), 1);
        assert_eq!(user.teams[&team_id.parse().unwrap()].role, "member");
    }

    #[test]
    fn build_tack_user_rejects_a_non_admin_with_no_teams_at_all() {
        let result = build_tack_user(&Uuid::new_v4().to_string(), &[], std::iter::empty());
        assert!(matches!(result, Err(AppError::Forbidden(_))));
    }

    #[test]
    fn build_tack_user_admits_an_admin_with_no_teams() {
        let user = build_tack_user(&Uuid::new_v4().to_string(), &["admin".to_string()], std::iter::empty())
            .expect("an admin should be granted access even with zero team memberships");
        assert!(user.is_admin);
        assert!(user.teams.is_empty());
    }

    #[test]
    fn organization_ids_dedupes_across_teams_in_the_same_org() {
        let org = Uuid::new_v4();
        let mut teams = HashMap::new();
        teams.insert(Uuid::new_v4(), TackTeamMembership { role: "member".into(), organization_id: Some(org) });
        teams.insert(Uuid::new_v4(), TackTeamMembership { role: "owner".into(), organization_id: Some(org) });
        let user = TackUser { user_id: Uuid::new_v4(), is_admin: false, teams };
        assert_eq!(user.organization_ids(), vec![org]);
    }

    #[test]
    fn organization_ids_returns_multiple_distinct_orgs() {
        let org_a = Uuid::new_v4();
        let org_b = Uuid::new_v4();
        let mut teams = HashMap::new();
        teams.insert(Uuid::new_v4(), TackTeamMembership { role: "member".into(), organization_id: Some(org_a) });
        teams.insert(Uuid::new_v4(), TackTeamMembership { role: "member".into(), organization_id: Some(org_b) });
        let user = TackUser { user_id: Uuid::new_v4(), is_admin: false, teams };
        let mut ids = user.organization_ids();
        ids.sort();
        let mut expected = vec![org_a, org_b];
        expected.sort();
        assert_eq!(ids, expected);
    }

    #[test]
    fn organization_ids_ignores_org_less_teams() {
        let mut teams = HashMap::new();
        teams.insert(Uuid::new_v4(), TackTeamMembership { role: "member".into(), organization_id: None });
        let user = TackUser { user_id: Uuid::new_v4(), is_admin: false, teams };
        assert!(user.organization_ids().is_empty());
    }
}
