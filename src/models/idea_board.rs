use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::note::{deserialize_some, NoteFolder};

/// An Idea Board is a `note_folders` row with `folder_type = 'ideas_board'`
/// (see `011_idea_boards.sql`'s header comment) -- this is just a type alias
/// for readability at Idea Board call sites, not a distinct wire shape.
pub type IdeaBoard = NoteFolder;

/// A page of a team's Idea Boards -- same `total`/pagination convention as
/// `NoteFoldersPage`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct IdeaBoardsPage {
    pub boards: Vec<IdeaBoard>,
    pub total: i64,
}

const VALID_STICKY_COLORS: &[&str] = &["yellow", "pink", "blue", "green", "purple", "orange"];
const VALID_SHAPE_TYPES: &[&str] = &["rect", "circle", "diamond", "database", "cloud", "actor", "image"];
const VALID_PORTS: &[&str] = &["top", "right", "bottom", "left"];

pub fn validate_sticky_color(color: &str) -> bool {
    VALID_STICKY_COLORS.contains(&color)
}

pub fn validate_shape_type(shape_type: &str) -> bool {
    VALID_SHAPE_TYPES.contains(&shape_type)
}

pub fn validate_port(port: &str) -> bool {
    VALID_PORTS.contains(&port)
}

/// A sticky note on an Idea Board -- the join of a `notes` row (its title/
/// body/thread, same as any other note) and its `idea_board_stickies` layout
/// row. Returned as one flattened shape so the canvas UI doesn't need a
/// second fetch per sticky; `note_id`/`board_id` are the only two IDs a
/// caller needs to address it by (`PATCH /stickies/{note_id}`, etc. --
/// stickies are addressed by their note id, not a separate sticky id, since
/// the two rows are 1:1 and created/deleted together).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Sticky {
    pub note_id: Uuid,
    pub board_id: Uuid,
    pub organization_id: Uuid,
    pub title: String,
    pub body_markdown: String,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub x: f64,
    pub y: f64,
    /// One of `VALID_STICKY_COLORS`.
    pub color: String,
    pub width: f64,
    pub height: f64,
    /// Soft link to an entity outside tack-server (e.g. a togra backlog
    /// story) -- both `None` or both `Some`, no FK, may dangle if the linked
    /// entity is later deleted (the app layer's job to tolerate, same as
    /// `NoteFolder`'s own `owning_service`/`entity_type`/`entity_id` triple).
    pub linked_entity_type: Option<String>,
    pub linked_entity_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StickiesPage {
    pub stickies: Vec<Sticky>,
    pub total: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateStickyRequest {
    pub title: String,
    pub body_markdown: String,
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    /// Defaults to `"yellow"` if omitted -- validated against
    /// `VALID_STICKY_COLORS` by the handler either way.
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub width: Option<f64>,
    #[serde(default)]
    pub height: Option<f64>,
    #[serde(default)]
    pub linked_entity_type: Option<String>,
    #[serde(default)]
    pub linked_entity_id: Option<String>,
}

/// All fields optional -- omitted means "leave unchanged." Unlike
/// `UpdateNoteRequest`'s `folder_id`, there's no tri-state need here: a
/// sticky's `linked_entity_type`/`linked_entity_id` are updated together (see
/// the handler) and "unlink" is expressed by passing empty-string-free
/// explicit nulls being out of scope for v1 -- clearing a link isn't a use
/// case the canvas UI needs yet, only setting/changing one.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateStickyRequest {
    pub title: Option<String>,
    pub body_markdown: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub color: Option<String>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub linked_entity_type: Option<String>,
    pub linked_entity_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BoardShape {
    pub id: Uuid,
    pub board_id: Uuid,
    pub organization_id: Uuid,
    /// One of `VALID_SHAPE_TYPES`.
    pub shape_type: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub fill_color: String,
    pub stroke_color: String,
    pub stroke_width: f64,
    pub label: Option<String>,
    pub label_color: String,
    pub label_size: f64,
    /// A DAM asset URL, plain HTTPS -- no authenticated proxy, same posture
    /// as `TeamAvatar` (see tack's own `CLAUDE.md`).
    pub image_url: Option<String>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct BoardShapesPage {
    pub shapes: Vec<BoardShape>,
    pub total: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateBoardShapeRequest {
    pub shape_type: String,
    pub x: f64,
    pub y: f64,
    #[serde(default)]
    pub width: Option<f64>,
    #[serde(default)]
    pub height: Option<f64>,
    #[serde(default)]
    pub fill_color: Option<String>,
    #[serde(default)]
    pub stroke_color: Option<String>,
    #[serde(default)]
    pub stroke_width: Option<f64>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub label_color: Option<String>,
    #[serde(default)]
    pub label_size: Option<f64>,
    #[serde(default)]
    pub image_url: Option<String>,
}

/// `label`/`image_url` are tri-state (omit = leave unchanged, `null` =
/// clear, a value = set) -- same `deserialize_some` pattern as
/// `UpdateNoteRequest::folder_id`, since "no label"/"no image" is a real,
/// distinct state from "don't touch it." Every other field is plain
/// `Option<T>` (omit = leave unchanged, a value = set; these have no
/// meaningful "clear" state -- `x`/`width`/etc. always have a real number).
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateBoardShapeRequest {
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,
    pub fill_color: Option<String>,
    pub stroke_color: Option<String>,
    pub stroke_width: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>, nullable)]
    pub label: Option<Option<String>>,
    pub label_color: Option<String>,
    pub label_size: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>, nullable)]
    pub image_url: Option<Option<String>>,
}

/// One directed edge on the board's link graph, from one note-or-shape to
/// another. Exactly one of `from_note_id`/`from_shape_id` is set (same for
/// `to_`) -- enforced by a DB CHECK, re-validated by the handler at create
/// time so a bad request 400s with a clear message rather than surfacing the
/// DB's own constraint-violation text.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NoteLink {
    pub id: Uuid,
    pub board_id: Uuid,
    pub organization_id: Uuid,
    pub from_note_id: Option<Uuid>,
    pub to_note_id: Option<Uuid>,
    pub from_shape_id: Option<Uuid>,
    pub to_shape_id: Option<Uuid>,
    /// One of `VALID_PORTS`, if set.
    pub from_port: Option<String>,
    pub to_port: Option<String>,
    pub label: Option<String>,
    pub created_by: Uuid,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NoteLinksPage {
    pub links: Vec<NoteLink>,
    pub total: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateNoteLinkRequest {
    #[serde(default)]
    pub from_note_id: Option<Uuid>,
    #[serde(default)]
    pub from_shape_id: Option<Uuid>,
    #[serde(default)]
    pub to_note_id: Option<Uuid>,
    #[serde(default)]
    pub to_shape_id: Option<Uuid>,
    #[serde(default)]
    pub from_port: Option<String>,
    #[serde(default)]
    pub to_port: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

/// All three fields tri-state (omit = leave unchanged, `null` = clear, a
/// value = set) -- a link with no port pinned to a side, or no label, is a
/// real, distinct state from "don't touch it." Same `deserialize_some`
/// pattern as `UpdateBoardShapeRequest::label`.
#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateNoteLinkRequest {
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>, nullable)]
    pub from_port: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>, nullable)]
    pub to_port: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>, nullable)]
    pub label: Option<Option<String>>,
}
