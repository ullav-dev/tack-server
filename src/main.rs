use anyhow::Result;
use axum::{extract::State, http::StatusCode, routing::get, Router};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

mod auth;
mod config;
mod db;
mod error;
mod handlers;

use config::Config;
use db::DbPool;

#[derive(Clone)]
pub struct AppState {
    pub db: DbPool,
    pub api_validator: ullav_mcp_auth::TokenValidator,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Tack Server API",
        version = "0.1.0",
        description = "Notes & Pages content platform"
    ),
    paths(health, handlers::me::me),
    components(schemas(error::ErrorResponse, handlers::me::MeResponse)),
    tags(
        (name = "health", description = "Service health"),
        (name = "me", description = "Caller identity"),
    ),
)]
struct ApiDoc;

/// Check service health (liveness + DB connectivity)
#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Service is healthy"),
        (status = 503, description = "Database unavailable"),
    ),
    tag = "health"
)]
async fn health(State(state): State<AppState>) -> StatusCode {
    match state.db.get().await {
        Ok(client) => match client.query_one("SELECT 1", &[]).await {
            Ok(_) => StatusCode::OK,
            Err(_) => StatusCode::SERVICE_UNAVAILABLE,
        },
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let _ = dotenvy::dotenv();

    let cfg = Config::from_env()?;

    let pool = db::create_pool(&cfg.database_url)?;
    db::run_migrations(&pool).await?;

    let addr: std::net::SocketAddr = cfg.bind_addr().parse()?;

    let state = AppState {
        db: pool,
        // Empty audience — this validates general API bearer tokens (any UUM-issued
        // JWT), not an MCP resource-server-audience-bound token. Matches
        // ullav-dam-server's `api_validator` construction.
        api_validator: ullav_mcp_auth::TokenValidator::new(
            cfg.oauth2_jwks_url,
            cfg.oauth2_issuer,
            String::new(),
        ),
    };

    let app = Router::new()
        .merge(SwaggerUi::new("/docs").url("/api-doc/openapi.json", ApiDoc::openapi()))
        .route("/health", get(health))
        .route("/me", get(handlers::me::me))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    tracing::info!("Listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
