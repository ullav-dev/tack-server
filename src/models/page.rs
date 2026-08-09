use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Space {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub owning_service: String,
    /// `None` means the space is open to any member of the organization,
    /// not just one team — mirrors Notes' `Visibility::Organization` tier,
    /// just expressed as an optional scope rather than an enum since a space
    /// (unlike a single note) has no narrower "private" tier of its own.
    pub team_id: Option<Uuid>,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateSpaceRequest {
    /// The team this space belongs to — required for the same reason
    /// `CreateNoteRequest.team_id` is required: it's how the space's
    /// organization (the Postgres shard key) is resolved. The team must be
    /// one of the caller's Tack-enabled teams, and must already have an
    /// organization assigned.
    pub team_id: Uuid,
    pub name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateSpaceRequest {
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PermissionLevel {
    View,
    Edit,
}

impl PermissionLevel {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            PermissionLevel::View => "view",
            PermissionLevel::Edit => "edit",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "edit" => PermissionLevel::Edit,
            _ => PermissionLevel::View,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalType {
    Team,
    User,
    Organization,
}

impl PrincipalType {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            PrincipalType::Team => "team",
            PrincipalType::User => "user",
            PrincipalType::Organization => "organization",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "user" => PrincipalType::User,
            "organization" => PrincipalType::Organization,
            _ => PrincipalType::Team,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Page {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub space_id: Uuid,
    pub parent_id: Option<Uuid>,
    /// The page's materialized path (e.g. `p1.p2`), exposed as text — used
    /// by clients to render a tree without a separate recursive fetch.
    pub path: String,
    pub title: String,
    pub is_template: bool,
    pub content_markdown: String,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Number of direct children — lets a tree UI show an expand affordance
    /// without a separate fetch, same reasoning as `Note.reply_count`.
    pub child_count: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreatePageRequest {
    pub space_id: Uuid,
    /// `None` creates a root page directly under the space.
    pub parent_id: Option<Uuid>,
    pub title: String,
    #[serde(default)]
    pub content_markdown: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdatePageRequest {
    pub title: Option<String>,
    pub content_markdown: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PagePermission {
    pub id: Uuid,
    pub page_id: Uuid,
    pub principal_type: PrincipalType,
    pub principal_id: Option<Uuid>,
    pub level: PermissionLevel,
    pub created_at: DateTime<Utc>,
}

/// The caller's own effective permission level on a page — used by
/// `GET /pages/{id}/permission`, which exists specifically so that
/// tack-hocuspocus (a separate service, in a different language) can
/// delegate ACL resolution back to this API rather than reimplementing
/// `pages_acl`'s ancestor/space-fallback algorithm in TypeScript. Also
/// useful to the frontend directly (e.g. to render the editor read-only).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PagePermissionLevelResponse {
    pub level: PermissionLevel,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreatePagePermissionRequest {
    pub principal_type: PrincipalType,
    /// Required for `team`/`user`, must be omitted (or `null`) for `organization`.
    pub principal_id: Option<Uuid>,
    pub level: PermissionLevel,
}

/// A named, user-triggered snapshot of a page's content (implementation
/// sequencing step 8c) — same shape and same explicit-only trigger model as
/// `NoteRevision`. Stores `content_markdown` (kept accurate by
/// tack-hocuspocus's `onStoreDocument`, not this table's own concern) rather
/// than a Yjs binary snapshot: simple, human-readable, view-only history —
/// no "restore this version into the live Yjs doc" feature, matching Notes.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PageRevision {
    pub id: Uuid,
    pub page_id: Uuid,
    pub version: i32,
    pub content_markdown: String,
    pub edited_by: Uuid,
    pub edited_at: DateTime<Utc>,
}

/// A page-to-page cross-reference (implementation sequencing step 8d,
/// scoped to page-to-page only for this pass -- cross-service references,
/// e.g. to a Togra workflow or Cunav ticket, are deferred: they'd need a
/// new public resolve endpoint in the owning service, which doesn't exist
/// yet). Backed by the generic `content_references` table (already present
/// since migration 002, unused until now) with `owning_service = 'tack'`,
/// `entity_type = 'page'`, `entity_id = <target page id>`.
///
/// Always resolved live at read time (`target_title`/`target_space_id` are
/// `None` if the target page no longer exists or the caller can no longer
/// see it -- a "broken link" state the UI can show, not an error) -- never
/// a denormalized snapshot. This is the direct structural fix for the
/// Confluence/Jira stale-reference problem the plan calls out.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PageReference {
    pub id: Uuid,
    pub source_page_id: Uuid,
    pub target_page_id: Uuid,
    pub target_title: Option<String>,
    pub target_space_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// The reverse of `PageReference` -- pages that reference *this* one. Same
/// underlying `content_references` rows, read from the other direction;
/// doubles as the backlinks UI, per the plan's original design for this
/// table ("populated at save time... doubles as the backlinks graph").
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PageBacklink {
    pub id: Uuid,
    pub source_page_id: Uuid,
    pub source_title: Option<String>,
    pub source_space_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreatePageReferenceRequest {
    pub target_page_id: Uuid,
}
