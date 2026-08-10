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
    /// Product slugs this team has enabled (from `team_product_access`).
    #[serde(default)]
    products: Vec<String>,
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

/// One team the caller belongs to that has Tack access (the `tack` product
/// enabled), with its organization if it has one.
#[derive(Debug, Clone)]
pub struct TackTeamMembership {
    pub role: String,
    pub organization_id: Option<Uuid>,
}

/// Authenticated Tack user extracted from the `Authorization: Bearer` header.
///
/// Requires the caller to be an admin, or a member of at least one team with
/// the `tack` product enabled — same team-granted access-gate pattern used by
/// every other first-party Ullav app (Togra/Cunav/Cartlann/Lagan).
#[derive(Debug, Clone)]
pub struct TackUser {
    pub user_id: Uuid,
    pub is_admin: bool,
    /// Only teams with `tack` enabled — teams without it are invisible to Tack,
    /// same as togra's frontend-side `getTograTeamIds` filtering, just enforced
    /// server-side here.
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
fn build_tack_user<'a>(
    sub: &str,
    roles: &[String],
    teams: impl IntoIterator<Item = (&'a String, &'a str, &'a [String], Option<&'a str>)>,
) -> Result<TackUser, AppError> {
    let user_id = sub.parse::<Uuid>().map_err(|_| AppError::Unauthorized("Invalid subject in token".into()))?;
    let is_admin = roles.iter().any(|r| r == "admin");

    let team_map: HashMap<Uuid, TackTeamMembership> = teams
        .into_iter()
        .filter(|(_, _, products, _)| products.iter().any(|p| p == "tack"))
        .filter_map(|(id, role, _, organization_id)| {
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
        return Err(AppError::Forbidden(
            "Your account does not have access to Tack. Ask your team owner to enable Tack for your team.".into(),
        ));
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
        claims.teams.iter().map(|(id, t)| (id, t.role.as_str(), t.products.as_slice(), t.organization_id.as_deref())),
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
            claims.teams.iter().map(|(id, t)| (id, t.role.as_str(), t.products.as_slice(), t.organization_id.as_deref())),
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
