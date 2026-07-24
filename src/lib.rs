pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod models;
pub mod search;

/// Shared state for tack-server's axum handlers (and, via `auth::TackUser`'s
/// extractor bound, anything that authenticates callers the same way).
/// `tack-indexer` doesn't use this — it's not an HTTP server.
#[derive(Clone)]
pub struct AppState {
    pub db: db::DbPool,
    pub api_validator: ullav_mcp_auth::TokenValidator,
    pub search: search::SearchClient,
}
