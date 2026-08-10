use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// A real, resolvable non-human note author -- see `009_system_principals.sql`
/// for why this exists instead of a nullable `notes.created_by`. A note
/// authored by a system principal is created exactly like any other note
/// (`CreateNoteRequest::created_by` set to this id, admin-only, same
/// backfill-override path used for migrating notes in from another system),
/// and is resolved by clients the same way: check the org's system
/// principals for a matching id before falling back to the team roster.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SystemPrincipal {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub label: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateSystemPrincipalRequest {
    pub organization_id: Uuid,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SystemPrincipalsPage {
    pub principals: Vec<SystemPrincipal>,
    pub total: i64,
}
