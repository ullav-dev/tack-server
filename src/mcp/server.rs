//! Tack MCP server.
//!
//! Exposes a small, outcome-shaped tool surface over Streamable HTTP — not a
//! 1:1 mirror of the REST API — matching the pattern already used well
//! elsewhere in this org (e.g. Obair's `get_task_context` bundling a task,
//! its parent, and its notes in one call). `get_note_thread` here is the
//! same idea: a note plus all its replies, in one round trip.
//!
//! Auth: audience-bound RS256 token validated by `mcp_auth_middleware` from
//! ullav-mcp-auth, plus a `tack:tools` scope guard — same shape as
//! ullav-dam-server's MCP server. `TackUser` (the same access-gate type the
//! REST API uses) is built from the token's claims via
//! `auth::from_mcp_claims`, so MCP tools enforce exactly the same
//! live-resolved visibility rules as direct HTTP reads.
use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ServerCapabilities, ServerInfo},
    service::RequestContext,
    tool, tool_handler, tool_router, RoleServer,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use ullav_mcp_auth::McpClaims;
use uuid::Uuid;

use tack_server::auth::{self, TackUser};
use tack_server::db::{self, DbPool};
use tack_server::embeddings::Embedder;
use tack_server::models::note::Visibility;
use tack_server::search::{SearchCaller, SearchClient};

use crate::notes_acl::{resolve_team_organization, resolve_visible_note};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn caller_from_ctx(ctx: &RequestContext<RoleServer>) -> Result<TackUser, rmcp::ErrorData> {
    let parts = ctx
        .extensions
        .get::<Parts>()
        .ok_or_else(|| rmcp::ErrorData::internal_error("missing request parts", None))?;
    let claims = parts
        .extensions
        .get::<McpClaims>()
        .ok_or_else(|| rmcp::ErrorData::internal_error("missing caller identity", None))?;
    auth::from_mcp_claims(claims).map_err(|e| rmcp::ErrorData::invalid_request(e.to_string(), None))
}

fn app_err(e: impl std::fmt::Display) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(e.to_string(), None)
}

fn parse_uuid(s: &str) -> Result<Uuid, rmcp::ErrorData> {
    s.parse::<Uuid>().map_err(|_| rmcp::ErrorData::invalid_params(format!("'{s}' is not a valid UUID"), None))
}

fn parse_visibility(s: &str) -> Result<Visibility, rmcp::ErrorData> {
    match s {
        "private" => Ok(Visibility::Private),
        "team" => Ok(Visibility::Team),
        "organization" => Ok(Visibility::Organization),
        other => Err(rmcp::ErrorData::invalid_params(
            format!("visibility must be 'private', 'team', or 'organization', got '{other}'"),
            None,
        )),
    }
}

// ── Request parameter types ───────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchContentParams {
    /// Free-text query.
    pub query: String,
    /// Maximum number of results (default 20).
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetNoteThreadParams {
    /// UUID of the top-level note.
    pub note_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateNoteParams {
    /// UUID of the team to file this note under (must be one of the caller's
    /// Tack-enabled teams, with an organization already assigned).
    pub team_id: String,
    /// One of "private", "team", "organization".
    pub visibility: String,
    pub title: String,
    pub body_markdown: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReplyToNoteParams {
    /// UUID of the note to reply to.
    pub note_id: String,
    pub body_markdown: String,
}

// ── Server implementation ─────────────────────────────────────────────────────

pub struct TackMcpServer {
    db: DbPool,
    search: SearchClient,
    embedder: Option<Embedder>,
}

impl TackMcpServer {
    fn new(db: DbPool, search: SearchClient, embedder: Option<Embedder>) -> Self {
        Self { db, search, embedder }
    }
}

#[tool_router]
impl TackMcpServer {
    /// Search across the caller's visible Notes (hybrid BM25 + semantic when
    /// the embedding model is loaded).
    #[tool(description = "Search Tack content the caller has access to")]
    async fn search_content(
        &self,
        Parameters(p): Parameters<SearchContentParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let user = caller_from_ctx(&context)?;
        let caller = SearchCaller {
            user_id: user.user_id,
            is_admin: user.is_admin,
            team_ids: user.teams.keys().copied().collect(),
            organization_ids: user.organization_ids(),
        };
        let hits = self.search.search(&p.query, &caller, self.embedder.as_ref()).await.map_err(app_err)?;
        let limit = p.limit.unwrap_or(20).max(0) as usize;
        let hits = &hits[..hits.len().min(limit)];
        let text = serde_json::to_string_pretty(hits).unwrap();
        let structured = serde_json::to_value(hits).unwrap();
        Ok(super::text_result(text, structured))
    }

    /// Get a note and all of its replies in one call.
    #[tool(description = "Get a note and its full reply thread in one call")]
    async fn get_note_thread(
        &self,
        Parameters(p): Parameters<GetNoteThreadParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let user = caller_from_ctx(&context)?;
        let note_id = parse_uuid(&p.note_id)?;
        let note = resolve_visible_note(&self.db, &user, note_id).await.map_err(app_err)?;
        let replies = db::notes::list_replies(&self.db, note.id, note.organization_id).await.map_err(app_err)?;
        let structured = json!({ "note": note, "replies": replies });
        Ok(super::text_result(serde_json::to_string_pretty(&structured).unwrap(), structured))
    }

    /// Create a new top-level note.
    #[tool(description = "Create a new top-level note under one of the caller's teams")]
    async fn create_note(
        &self,
        Parameters(p): Parameters<CreateNoteParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let user = caller_from_ctx(&context)?;
        if p.title.trim().is_empty() {
            return Err(rmcp::ErrorData::invalid_params("title must not be empty", None));
        }
        if p.body_markdown.trim().is_empty() {
            return Err(rmcp::ErrorData::invalid_params("body_markdown must not be empty", None));
        }
        let team_id = parse_uuid(&p.team_id)?;
        let visibility = parse_visibility(&p.visibility)?;
        let organization_id = resolve_team_organization(&user, team_id).map_err(app_err)?;
        let note = db::notes::create_note(
            &self.db,
            db::notes::NewNote {
                organization_id,
                team_id,
                visibility,
                created_by: user.user_id,
                title: p.title,
                body_markdown: p.body_markdown,
                attach: None,
                created_at: None,
            },
        )
        .await
        .map_err(app_err)?;
        let text = serde_json::to_string_pretty(&note).unwrap();
        let structured = serde_json::to_value(&note).unwrap();
        Ok(super::text_result(text, structured))
    }

    /// Reply to an existing note.
    #[tool(description = "Reply to an existing note")]
    async fn reply_to_note(
        &self,
        Parameters(p): Parameters<ReplyToNoteParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let user = caller_from_ctx(&context)?;
        if p.body_markdown.trim().is_empty() {
            return Err(rmcp::ErrorData::invalid_params("body_markdown must not be empty", None));
        }
        let note_id = parse_uuid(&p.note_id)?;
        let parent = resolve_visible_note(&self.db, &user, note_id).await.map_err(app_err)?;
        let reply =
            db::notes::create_reply(&self.db, &parent, user.user_id, &p.body_markdown, None).await.map_err(app_err)?;
        let text = serde_json::to_string_pretty(&reply).unwrap();
        let structured = serde_json::to_value(&reply).unwrap();
        Ok(super::text_result(text, structured))
    }
}

#[tool_handler]
impl rmcp::ServerHandler for TackMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Tack — Notes & Pages content platform. \
             Use search_content to find notes the caller has access to. \
             Use get_note_thread to fetch a note and its full reply thread in one call. \
             Use create_note to file a new top-level note under one of the caller's teams \
             (visibility: private, team, or organization). \
             Use reply_to_note to reply to an existing note.",
        )
    }
}

// ── Service factory ───────────────────────────────────────────────────────────

pub fn make_tack_mcp_service(
    db: DbPool,
    search: SearchClient,
    embedder: Option<Embedder>,
    external_host: &str,
) -> StreamableHttpService<TackMcpServer, LocalSessionManager> {
    let session_manager = Arc::new(LocalSessionManager::default());
    let config = StreamableHttpServerConfig::default().with_allowed_hosts([
        "localhost",
        "127.0.0.1",
        "::1",
        external_host,
    ]);
    StreamableHttpService::new(
        move || Ok(TackMcpServer::new(db.clone(), search.clone(), embedder.clone())),
        session_manager,
        config,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uuid_rejects_invalid() {
        assert!(parse_uuid("not-a-uuid").is_err());
    }

    #[test]
    fn parse_uuid_accepts_valid() {
        assert!(parse_uuid("550e8400-e29b-41d4-a716-446655440000").is_ok());
    }

    #[test]
    fn parse_visibility_accepts_known_values() {
        assert!(matches!(parse_visibility("private"), Ok(Visibility::Private)));
        assert!(matches!(parse_visibility("team"), Ok(Visibility::Team)));
        assert!(matches!(parse_visibility("organization"), Ok(Visibility::Organization)));
    }

    #[test]
    fn parse_visibility_rejects_unknown_values() {
        assert!(parse_visibility("public").is_err());
        assert!(parse_visibility("").is_err());
    }
}
