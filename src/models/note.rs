use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    /// Only the creator (and admins) can see it.
    Private,
    /// Visible to any member of `team_id`.
    Team,
    /// Visible to any member of any team in the note's organization.
    Organization,
}

impl Visibility {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Team => "team",
            Visibility::Organization => "organization",
        }
    }

    pub fn from_db_str(s: &str) -> Self {
        match s {
            "team" => Visibility::Team,
            "organization" => Visibility::Organization,
            _ => Visibility::Private,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Note {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub team_id: Option<Uuid>,
    pub parent_id: Option<Uuid>,
    pub visibility: Visibility,
    pub body_markdown: String,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Number of direct replies — derived, returned by list/get endpoints so
    /// the UI can show a badge without a separate fetch.
    pub reply_count: i64,
}

/// A page of top-level notes for a team, oldest-first pagination via
/// `limit`/`offset` — simple offset pagination, not cursor-based, since a
/// team's note volume doesn't yet warrant the extra complexity (see
/// `handlers::notes::list_notes`).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NotesPage {
    pub notes: Vec<Note>,
    /// `true` if there are more notes beyond this page (requesting the next
    /// `offset` will return further results).
    pub has_more: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateNoteRequest {
    /// The team to file this note under. Required even for a `private` note —
    /// it's how the note's organization (the Postgres shard key) is resolved.
    /// The team must be one of the caller's Tack-enabled teams, and must
    /// already have an organization assigned (see the Organizations
    /// migration in ullav-user-management).
    pub team_id: Uuid,
    pub visibility: Visibility,
    pub body_markdown: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReplyRequest {
    pub body_markdown: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateNoteRequest {
    /// Presence of this field creates a new revision.
    pub body_markdown: Option<String>,
    pub visibility: Option<Visibility>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NoteRevision {
    pub id: Uuid,
    pub note_id: Uuid,
    pub version: i32,
    pub body_markdown: String,
    pub edited_by: Uuid,
    pub edited_at: DateTime<Utc>,
}
