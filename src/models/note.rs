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

/// Attaches a note to an entity owned by another service (e.g. a lagan pull
/// request, a togra workflow) — backs `content_attachments`. All three
/// fields are required together: a note is either fully attached or not
/// attached at all, never partially.
#[derive(Debug, Deserialize, ToSchema)]
pub struct AttachRequest {
    pub owning_service: String,
    pub entity_type: String,
    pub entity_id: String,
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
    /// Optionally attaches this note to an external entity (`content_attachments`)
    /// in the same transaction as its creation, e.g. a lagan pull request's
    /// discussion thread.
    #[serde(default)]
    pub attach: Option<AttachRequest>,
    /// Backfill-only: lets an admin caller preserve a historical timestamp
    /// when migrating notes in from another system, instead of always using
    /// the moment of the API call. Silently ignored for non-admins (falls
    /// back to `NOW()`) — see `handlers::notes::create_note`.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReplyRequest {
    pub body_markdown: String,
    /// Same backfill-only, admin-only timestamp override as
    /// `CreateNoteRequest::created_at`.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

/// A single `content_attachments` row belonging to a note, exposed so a
/// caller (e.g. Cartlann, linking a note to several of its own objects) can
/// attach/detach/list entities on a note any time after creation -- not just
/// the single fixed `attach` `CreateNoteRequest` supports at creation time.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NoteAttachment {
    pub id: Uuid,
    pub note_id: Uuid,
    pub owning_service: String,
    pub entity_type: String,
    pub entity_id: String,
    pub created_at: DateTime<Utc>,
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
