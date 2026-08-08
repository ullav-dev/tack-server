use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
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
    /// The folder this note is filed under, or `None` if unfiled. Only ever
    /// set on a top-level note (`parent_id IS NULL`) -- enforced by a DB
    /// CHECK constraint, see `008_note_folders.sql`.
    pub folder_id: Option<Uuid>,
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
    /// Optionally files the new note under a folder at creation time, instead
    /// of leaving it unfiled and moving it in later via `PATCH /notes/{id}`.
    /// Must belong to `team_id` -- checked by the handler.
    #[serde(default)]
    pub folder_id: Option<Uuid>,
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
    /// Backfill-only: lets an admin caller preserve the original author when
    /// migrating notes in from another system (e.g. Cartlann's one-time
    /// `research_notes` import), instead of attributing the note to
    /// whichever admin/service account is running the import. Silently
    /// ignored for non-admins (falls back to the caller's own id) — same
    /// admin-only-override precedent as `created_at`.
    #[serde(default)]
    pub created_by: Option<Uuid>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ReplyRequest {
    pub body_markdown: String,
    /// Same backfill-only, admin-only timestamp override as
    /// `CreateNoteRequest::created_at`.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// Same backfill-only, admin-only author override as
    /// `CreateNoteRequest::created_by`.
    #[serde(default)]
    pub created_by: Option<Uuid>,
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

/// Distinguishes "field omitted" from "field explicitly set to `null`" for a
/// nullable `PATCH` field -- ordinary `Option<T>` can't tell those apart
/// (both deserialize to `None`), but `folder_id` needs all three states:
/// omitted (don't touch), `null` (unfile), or a value (move/assign). Wraps
/// the outcome in a second `Option`, so `None` = omitted, `Some(None)` =
/// explicit null, `Some(Some(id))` = a value.
fn deserialize_some<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(deserializer).map(Some)
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
    /// Tri-state: omit to leave the note's folder unchanged, `null` to
    /// unfile it, or a folder id to file/move it there. Rejected by the
    /// handler on a reply (only top-level notes can be filed) or if the
    /// folder isn't in the note's own team. See `deserialize_some` above.
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<Uuid>, nullable)]
    pub folder_id: Option<Option<Uuid>>,
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

/// A flat (non-nested), per-team grouping for top-level notes — see
/// `008_note_folders.sql`. Any member of `team_id` may create, rename, or
/// delete a folder (this is an organizational tool, not access-controlled
/// content — a folder itself carries no visibility of its own; each note's
/// own `Visibility` still governs who can see it inside the folder).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NoteFolder {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub team_id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Count of top-level notes currently filed here — denormalized at read
    /// time (not stored), same convention as `Note.reply_count` and
    /// `Page.child_count`. Lets the UI show a count, and a delete
    /// confirmation warn how many notes will be unfiled, without a separate
    /// fetch.
    pub note_count: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateNoteFolderRequest {
    pub team_id: Uuid,
    pub name: String,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateNoteFolderRequest {
    pub name: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole "unfile a note" feature depends on this: `Option<Option<T>>`
    /// via `deserialize_some` must actually distinguish omitted from
    /// explicit-null. `cargo build` only proves this compiles, not that it
    /// behaves -- verify all three tri-states directly against real JSON.
    #[test]
    fn folder_id_omitted_deserializes_to_none() {
        let req: UpdateNoteRequest = serde_json::from_str(r#"{"title":"x"}"#).unwrap();
        assert_eq!(req.folder_id, None, "omitting folder_id must mean 'leave it unchanged'");
    }

    #[test]
    fn folder_id_explicit_null_deserializes_to_some_none() {
        let req: UpdateNoteRequest = serde_json::from_str(r#"{"folder_id":null}"#).unwrap();
        assert_eq!(req.folder_id, Some(None), "explicit null must mean 'unfile'");
    }

    #[test]
    fn folder_id_with_value_deserializes_to_some_some() {
        let id = Uuid::new_v4();
        let json = format!(r#"{{"folder_id":"{id}"}}"#);
        let req: UpdateNoteRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req.folder_id, Some(Some(id)), "a folder id must mean 'file/move there'");
    }
}
