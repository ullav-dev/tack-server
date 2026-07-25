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
    /// Empty for replies -- only top-level notes collect a title (enforced
    /// in the handler, not the schema; see `handlers::notes::create_note`).
    pub title: String,
    pub body_markdown: String,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Number of direct replies — derived, returned by list/get endpoints so
    /// the UI can show a badge without a separate fetch.
    pub reply_count: i64,
    /// For a reply, the parent note's latest saved version number at the
    /// moment this reply was created -- lets the UI show a reply only while
    /// browsing that version (or the current state, if it's still the
    /// latest). `None` for top-level notes, and for replies created before
    /// this field existed.
    pub in_reply_to_version: Option<i32>,
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
    pub title: String,
    pub body_markdown: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReplyRequest {
    pub body_markdown: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateNoteRequest {
    /// Does NOT create a new revision -- see `POST /notes/{id}/revisions`
    /// for that, which is a separate, explicit action.
    pub body_markdown: Option<String>,
    pub visibility: Option<Visibility>,
    /// Not versioned in `note_revisions` (only `body_markdown` is) -- title
    /// history isn't tracked, matching Pages, which doesn't version its
    /// title either.
    pub title: Option<String>,
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
