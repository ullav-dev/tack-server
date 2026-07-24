use anyhow::Result;
use axum::{
    extract::State,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Extension, Router,
};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use ullav_mcp_auth::{mcp_auth_middleware, protected_resource_metadata, McpClaims, ProtectedResourceConfig, TokenValidator};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

mod handlers;
mod mcp;
mod notes_acl;

use tack_server::{auth, config, db, error, models, search, AppState};

use config::Config;
use search::SearchClient;

fn host_from_uri(uri: &str) -> &str {
    let without_scheme = uri.find("://").map(|i| &uri[i + 3..]).unwrap_or(uri);
    without_scheme.split('/').next().unwrap_or(without_scheme)
}

/// Gates the MCP endpoint on the `tack:tools` scope — mirrors
/// ullav-dam-server's `dam_scope_guard`.
async fn tack_scope_guard(req: Request<axum::body::Body>, next: Next) -> Result<Response, StatusCode> {
    let claims = req.extensions().get::<McpClaims>().ok_or(StatusCode::UNAUTHORIZED)?;
    if !claims.scope.split_whitespace().any(|s| s == "tack:tools") {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(req).await)
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Tack Server API",
        version = "0.1.0",
        description = "Notes & Pages content platform"
    ),
    paths(
        health,
        handlers::me::me,
        handlers::notes::create_note,
        handlers::notes::list_notes,
        handlers::notes::get_note,
        handlers::notes::update_note,
        handlers::notes::delete_note,
        handlers::notes::create_reply,
        handlers::notes::list_replies,
        handlers::notes::list_revisions,
        handlers::search::search,
    ),
    components(schemas(
        error::ErrorResponse,
        handlers::me::MeResponse,
        models::note::Note,
        models::note::Visibility,
        models::note::CreateNoteRequest,
        models::note::ReplyRequest,
        models::note::UpdateNoteRequest,
        models::note::NoteRevision,
        search::SearchHit,
    )),
    tags(
        (name = "health", description = "Service health"),
        (name = "me", description = "Caller identity"),
        (name = "notes", description = "Notes: threaded, entity-attached comments"),
        (name = "search", description = "Cross-content search"),
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

    // OpenSearch is a secondary, rebuildable index -- Postgres (already
    // connected above) is the source of truth. A Notes CRUD-only outage in
    // OpenSearch must not take down the whole API server, so this is a
    // logged warning, not a startup failure (`GET /search` will simply error
    // per-request until OpenSearch is reachable).
    let search_client = SearchClient::new(cfg.opensearch_url.clone());
    if let Err(e) = search_client.ensure_index().await {
        tracing::warn!("OpenSearch not reachable at startup, continuing without it: {e:#}");
    }

    let addr: std::net::SocketAddr = cfg.bind_addr().parse()?;

    let mcp_token_validator =
        TokenValidator::new(cfg.oauth2_jwks_url.clone(), cfg.oauth2_issuer.clone(), cfg.tack_mcp_canonical_uri.clone());
    let mcp_prc = ProtectedResourceConfig {
        resource_uri: cfg.tack_mcp_canonical_uri.clone(),
        authorization_server: cfg.oauth2_issuer.clone(),
        scopes_supported: vec!["tack:tools".to_owned()],
        jwks_uri: cfg.oauth2_jwks_url.clone(),
    };
    let mcp_service =
        mcp::make_tack_mcp_service(pool.clone(), search_client.clone(), host_from_uri(&cfg.tack_mcp_canonical_uri));

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
        search: search_client,
    };

    let app = Router::new()
        .merge(SwaggerUi::new("/docs").url("/api-doc/openapi.json", ApiDoc::openapi()))
        .route("/health", get(health))
        .route("/me", get(handlers::me::me))
        .route("/notes", post(handlers::notes::create_note).get(handlers::notes::list_notes))
        .route(
            "/notes/:id",
            get(handlers::notes::get_note)
                .patch(handlers::notes::update_note)
                .delete(handlers::notes::delete_note),
        )
        .route(
            "/notes/:id/replies",
            post(handlers::notes::create_reply).get(handlers::notes::list_replies),
        )
        .route("/notes/:id/revisions", get(handlers::notes::list_revisions))
        .route("/search", get(handlers::search::search))
        // Tack MCP — audience-bound RS256 + tack:tools scope guard.
        .merge(
            Router::new()
                .route_service("/mcp", mcp_service)
                .layer(middleware::from_fn(tack_scope_guard))
                .layer(middleware::from_fn(mcp_auth_middleware))
                .layer(Extension(mcp_token_validator))
                .layer(Extension(mcp_prc.clone())),
        )
        // RFC 9728 protected resource metadata.
        .merge(
            Router::new()
                .route("/.well-known/oauth-protected-resource/mcp", get(protected_resource_metadata))
                .layer(Extension(mcp_prc)),
        )
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    tracing::info!("Listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
