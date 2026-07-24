//! Pages access-control logic. Unlike Notes' flat visibility enum, a page's
//! effective permission is resolved live from its position in the
//! space/page tree: an explicit override on the nearest ancestor (or the
//! page itself) if one exists, anywhere in the ancestor chain, or — if no
//! ancestor has any override at all — the caller's space membership.
//!
//! This lives in one shared place (not duplicated per handler) for the same
//! reason `notes_acl.rs` does: whatever transport calls it (REST today, MCP
//! later) must enforce identical rules.

use uuid::Uuid;

use crate::auth::TackUser;
use crate::db::{self, DbPool};
use crate::error::{AppError, AppResult};
use crate::models::page::{Page, PagePermission, PermissionLevel, PrincipalType, Space};

/// Resolves the caller's effective permission on `page`, given its `space`.
/// `None` means no access at all (not even view).
///
/// Resolution order:
/// 1. Admins always get `Edit`.
/// 2. If any ancestor-or-self page (walking up from `page`) has explicit
///    `page_permissions` rows, those rows are an **exhaustive whitelist** —
///    the caller's level is whatever those rows grant them, or `None` if
///    they aren't listed at all. This does not fall through to space
///    membership even if the caller isn't listed — matching how Confluence
///    restrictions actually behave.
/// 3. Otherwise, fall back to space membership (`space_default_level`).
pub async fn resolve_effective_permission(
    db: &DbPool,
    user: &TackUser,
    page: &Page,
    space: &Space,
) -> AppResult<Option<PermissionLevel>> {
    if user.is_admin {
        return Ok(Some(PermissionLevel::Edit));
    }

    let rows = db::pages::nearest_permission_rows(db, page.organization_id, &page.path).await?;
    if !rows.is_empty() {
        return Ok(level_from_rows(&rows, user, space.organization_id));
    }

    Ok(space_default_level(space, user))
}

/// The highest level any matching row in `rows` grants `user` — `None` if no
/// row's principal matches them at all.
fn level_from_rows(rows: &[PagePermission], user: &TackUser, organization_id: Uuid) -> Option<PermissionLevel> {
    rows.iter()
        .filter(|r| principal_matches(r.principal_type, r.principal_id, user, organization_id))
        .map(|r| r.level)
        .max_by_key(|l| matches!(l, PermissionLevel::Edit))
}

fn principal_matches(
    principal_type: PrincipalType,
    principal_id: Option<Uuid>,
    user: &TackUser,
    organization_id: Uuid,
) -> bool {
    match principal_type {
        PrincipalType::User => principal_id == Some(user.user_id),
        PrincipalType::Team => principal_id.is_some_and(|t| user.teams.contains_key(&t)),
        PrincipalType::Organization => user.organization_ids().contains(&organization_id),
    }
}

/// A space with no page-level overrides grants `Edit` to its own team's
/// members (a team wiki, editable by the team — same default posture as
/// Confluence space permissions defaulting to space members being able to
/// edit), or to any member of the space's organization if the space itself
/// is org-wide (`team_id` is `None`).
fn space_default_level(space: &Space, user: &TackUser) -> Option<PermissionLevel> {
    let has_access = match space.team_id {
        Some(team_id) => user.teams.contains_key(&team_id),
        None => user.organization_ids().contains(&space.organization_id),
    };
    has_access.then_some(PermissionLevel::Edit)
}

pub fn can_view(level: Option<PermissionLevel>) -> bool {
    level.is_some()
}

pub fn can_edit(level: Option<PermissionLevel>) -> bool {
    level == Some(PermissionLevel::Edit)
}

/// `true` if `user` may create a root page (or a space-level object like a
/// permission grant on the space itself) directly under `space` — i.e. the
/// space's own default level, since there's no page yet to hold an override.
pub fn can_create_in_space(space: &Space, user: &TackUser) -> bool {
    user.is_admin || space_default_level(space, user) == Some(PermissionLevel::Edit)
}

/// Re-resolves `page`'s effective permission and errors unless it's `Edit`.
/// Used after `resolve_visible_page` (which only guarantees `View`) by any
/// handler that mutates a page.
pub async fn require_edit(db: &DbPool, user: &TackUser, page: &Page, space: &Space) -> AppResult<()> {
    let level = resolve_effective_permission(db, user, page, space).await?;
    if can_edit(level) {
        Ok(())
    } else {
        Err(AppError::Forbidden("You don't have edit access to this page.".into()))
    }
}

/// Resolves a bare space id to a `Space`, scoped to whichever of the
/// caller's organizations actually contains it. Mirrors `resolve_visible_page`.
pub async fn resolve_space(db: &DbPool, user: &TackUser, space_id: Uuid) -> AppResult<Space> {
    for org_id in user.organization_ids() {
        if let Some(space) = db::spaces::get_space(db, space_id, org_id).await? {
            return Ok(space);
        }
    }
    if user.is_admin {
        if let Some(space) = db::spaces::get_space_admin_any_org(db, space_id).await? {
            return Ok(space);
        }
    }
    Err(AppError::NotFound(format!("Space {space_id} not found")))
}

/// Resolves a bare page id to a `Page` + its `Space`, scoped to whichever of
/// the caller's organizations actually contains it, then enforces the
/// caller has at least `View` access. Mirrors `notes_acl::resolve_visible_note`.
pub async fn resolve_visible_page(db: &DbPool, user: &TackUser, id: Uuid) -> AppResult<(Page, Space)> {
    for org_id in user.organization_ids() {
        if let Some(page) = db::pages::get_page(db, id, org_id).await? {
            let space = db::spaces::get_space(db, page.space_id, org_id).await?.ok_or_else(|| {
                AppError::Internal(anyhow::anyhow!("page {id}'s space {} not found", page.space_id))
            })?;
            let level = resolve_effective_permission(db, user, &page, &space).await?;
            if can_view(level) {
                return Ok((page, space));
            }
            return Err(AppError::Forbidden("You don't have access to this page.".into()));
        }
    }
    if user.is_admin {
        if let Some(page) = db::pages::get_page_admin_any_org(db, id).await? {
            let space = db::spaces::get_space(db, page.space_id, page.organization_id).await?.ok_or_else(|| {
                AppError::Internal(anyhow::anyhow!("page {id}'s space {} not found", page.space_id))
            })?;
            return Ok((page, space));
        }
    }
    Err(AppError::NotFound(format!("Page {id} not found")))
}

/// Resolves the organization to file a new space under, given the team the
/// caller wants to create it in. Mirrors `notes_acl::resolve_team_organization`.
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
    use crate::auth::TackTeamMembership;
    use chrono::Utc;
    use std::collections::HashMap;

    fn user(is_admin: bool, teams: HashMap<Uuid, TackTeamMembership>) -> TackUser {
        TackUser { user_id: Uuid::new_v4(), is_admin, teams }
    }

    fn space(organization_id: Uuid, team_id: Option<Uuid>) -> Space {
        Space {
            id: Uuid::new_v4(),
            organization_id,
            owning_service: "tack".into(),
            team_id,
            name: "Test Space".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn page(organization_id: Uuid, space_id: Uuid, path: &str) -> Page {
        Page {
            id: Uuid::new_v4(),
            organization_id,
            space_id,
            parent_id: None,
            path: path.into(),
            title: "Page".into(),
            is_template: false,
            content_markdown: "content".into(),
            created_by: Uuid::new_v4(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            child_count: 0,
        }
    }

    #[test]
    fn space_default_grants_edit_to_team_members() {
        let team_id = Uuid::new_v4();
        let s = space(Uuid::new_v4(), Some(team_id));
        let mut teams = HashMap::new();
        teams.insert(team_id, TackTeamMembership { role: "member".into(), organization_id: None });
        let u = user(false, teams);
        assert_eq!(space_default_level(&s, &u), Some(PermissionLevel::Edit));
    }

    #[test]
    fn space_default_denies_non_members() {
        let s = space(Uuid::new_v4(), Some(Uuid::new_v4()));
        let u = user(false, HashMap::new());
        assert_eq!(space_default_level(&s, &u), None);
    }

    #[test]
    fn org_wide_space_grants_edit_to_any_org_member() {
        let org = Uuid::new_v4();
        let s = space(org, None);
        let mut teams = HashMap::new();
        teams.insert(Uuid::new_v4(), TackTeamMembership { role: "member".into(), organization_id: Some(org) });
        let u = user(false, teams);
        assert_eq!(space_default_level(&s, &u), Some(PermissionLevel::Edit));
    }

    #[test]
    fn org_wide_space_denies_outsiders() {
        let s = space(Uuid::new_v4(), None);
        let u = user(false, HashMap::new());
        assert_eq!(space_default_level(&s, &u), None);
    }

    #[test]
    fn override_rows_are_an_exhaustive_whitelist_not_merged_with_space_default() {
        // Caller IS a member of the space's team (would get Edit by default),
        // but the page has explicit rows that don't list them -- must be denied,
        // not fall back to the space default.
        let org = Uuid::new_v4();
        let team_id = Uuid::new_v4();
        let mut teams = HashMap::new();
        teams.insert(team_id, TackTeamMembership { role: "member".into(), organization_id: Some(org) });
        let u = user(false, teams);

        let rows = vec![PagePermission {
            id: Uuid::new_v4(),
            page_id: Uuid::new_v4(),
            principal_type: PrincipalType::User,
            principal_id: Some(Uuid::new_v4()), // some other user, not `u`
            level: PermissionLevel::View,
            created_at: Utc::now(),
        }];
        assert_eq!(level_from_rows(&rows, &u, org), None);
    }

    #[test]
    fn override_rows_grant_the_highest_matching_level() {
        let org = Uuid::new_v4();
        let team_id = Uuid::new_v4();
        let mut teams = HashMap::new();
        teams.insert(team_id, TackTeamMembership { role: "member".into(), organization_id: Some(org) });
        let u = user(false, teams);

        let rows = vec![
            PagePermission {
                id: Uuid::new_v4(),
                page_id: Uuid::new_v4(),
                principal_type: PrincipalType::Team,
                principal_id: Some(team_id),
                level: PermissionLevel::View,
                created_at: Utc::now(),
            },
            PagePermission {
                id: Uuid::new_v4(),
                page_id: Uuid::new_v4(),
                principal_type: PrincipalType::User,
                principal_id: Some(u.user_id),
                level: PermissionLevel::Edit,
                created_at: Utc::now(),
            },
        ];
        assert_eq!(level_from_rows(&rows, &u, org), Some(PermissionLevel::Edit));
    }

    #[test]
    fn organization_principal_matches_any_org_member() {
        let org = Uuid::new_v4();
        let mut teams = HashMap::new();
        teams.insert(Uuid::new_v4(), TackTeamMembership { role: "member".into(), organization_id: Some(org) });
        let u = user(false, teams);

        let rows = vec![PagePermission {
            id: Uuid::new_v4(),
            page_id: Uuid::new_v4(),
            principal_type: PrincipalType::Organization,
            principal_id: None,
            level: PermissionLevel::View,
            created_at: Utc::now(),
        }];
        assert_eq!(level_from_rows(&rows, &u, org), Some(PermissionLevel::View));
    }

    #[test]
    fn can_view_and_can_edit_reflect_the_resolved_level() {
        assert!(can_view(Some(PermissionLevel::View)));
        assert!(can_view(Some(PermissionLevel::Edit)));
        assert!(!can_view(None));
        assert!(!can_edit(Some(PermissionLevel::View)));
        assert!(can_edit(Some(PermissionLevel::Edit)));
    }

    #[test]
    fn page_and_space_fixtures_are_self_consistent() {
        // Sanity check that the fixtures used above actually compile/typecheck
        // against the real Page/Space shapes (guards against fixture drift).
        let org = Uuid::new_v4();
        let s = space(org, None);
        let p = page(org, s.id, "abc123");
        assert_eq!(p.organization_id, s.organization_id);
    }
}
