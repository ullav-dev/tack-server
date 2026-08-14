use anyhow::{Context, Result};
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
use ullav_mcp_auth::{
    mcp_auth_middleware, protected_resource_metadata, unauthorized_response, McpClaims,
    ProtectedResourceConfig, TokenValidator,
};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

mod handlers;
mod mcp;
mod notes_acl;
mod pages_acl;

use tack_server::{auth, config, db, embeddings, error, models, search, AppState};

use config::Config;
use search::SearchClient;

fn host_from_uri(uri: &str) -> &str {
    let without_scheme = uri.find("://").map(|i| &uri[i + 3..]).unwrap_or(uri);
    without_scheme.split('/').next().unwrap_or(without_scheme)
}

/// Builds the RFC 9728 `resource_metadata` URL for a given MCP resource URI
/// (e.g. `http://localhost:8087/mcp` -> `http://localhost:8087/.well-known/
/// oauth-protected-resource/mcp`) — the same well-known path this service
/// actually registers `protected_resource_metadata` under, so a client that
/// follows the header lands on a real endpoint.
fn resource_metadata_url(resource_uri: &str) -> String {
    let path_start = resource_uri
        .find("://")
        .and_then(|i| resource_uri[i + 3..].find('/').map(|j| i + 3 + j))
        .unwrap_or(resource_uri.len());
    let (origin, path) = resource_uri.split_at(path_start);
    format!("{origin}/.well-known/oauth-protected-resource{path}")
}

/// Gates the MCP endpoint on the `tack:tools` scope — mirrors
/// ullav-dam-server's `dam_scope_guard`. Uses
/// `ullav_mcp_auth::unauthorized_response` to build the RFC 9728
/// `WWW-Authenticate` challenge on rejection, rather than falling through to
/// a bare `StatusCode` (which drops the header — a client relying on
/// discovery via the header, rather than already knowing the metadata URL,
/// can't complete the OAuth flow at all).
async fn tack_scope_guard(
    Extension(prc): Extension<ProtectedResourceConfig>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, Response> {
    let metadata_url = resource_metadata_url(&prc.resource_uri);
    let claims = req
        .extensions()
        .get::<McpClaims>()
        .ok_or_else(|| unauthorized_response(&metadata_url, "tack:tools"))?;
    if !claims.scope.split_whitespace().any(|s| s == "tack:tools") {
        // insufficient_scope is a 403, not a 401, but RFC 6750 still calls
        // for the challenge header so the client can see what scope it's
        // missing — reuse the same header, just override the status.
        let mut resp = unauthorized_response(&metadata_url, "tack:tools");
        *resp.status_mut() = StatusCode::FORBIDDEN;
        return Err(resp);
    }
    Ok(next.run(req).await)
}

#[cfg(test)]
mod scope_guard_tests {
    use super::resource_metadata_url;

    #[test]
    fn builds_well_known_url_alongside_the_resource_path() {
        assert_eq!(
            resource_metadata_url("http://localhost:8087/mcp"),
            "http://localhost:8087/.well-known/oauth-protected-resource/mcp"
        );
    }

    #[test]
    fn handles_a_bare_origin_with_no_path() {
        assert_eq!(
            resource_metadata_url("https://tack.example.com"),
            "https://tack.example.com/.well-known/oauth-protected-resource"
        );
    }

    #[test]
    fn preserves_a_nested_path() {
        assert_eq!(
            resource_metadata_url("https://example.com:8443/svc/mcp"),
            "https://example.com:8443/.well-known/oauth-protected-resource/svc/mcp"
        );
    }
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
        handlers::notes::list_notes_by_attachment,
        handlers::notes::get_note,
        handlers::notes::update_note,
        handlers::notes::delete_note,
        handlers::notes::create_reply,
        handlers::notes::list_replies,
        handlers::notes::create_attachment,
        handlers::notes::list_attachments,
        handlers::notes::delete_attachment,
        handlers::notes::list_revisions,
        handlers::notes::create_revision,
        handlers::notes::delete_revision,
        handlers::notes::mark_note_read,
        handlers::notes::list_unread,
        handlers::note_folders::create_note_folder,
        handlers::note_folders::list_note_folders,
        handlers::note_folders::list_note_folders_by_attachment,
        handlers::note_folders::update_note_folder,
        handlers::note_folders::delete_note_folder,
        handlers::system_principals::create_system_principal,
        handlers::system_principals::list_system_principals,
        handlers::system_principals::delete_system_principal,
        handlers::idea_boards::create_board,
        handlers::idea_boards::list_boards,
        handlers::idea_boards::list_boards_by_entity,
        handlers::idea_boards::get_board,
        handlers::idea_boards::update_board,
        handlers::idea_boards::delete_board,
        handlers::idea_boards::create_sticky,
        handlers::idea_boards::list_stickies,
        handlers::idea_boards::get_sticky,
        handlers::idea_boards::get_sticky_by_entity,
        handlers::idea_boards::update_sticky,
        handlers::idea_boards::delete_sticky,
        handlers::idea_boards::create_shape,
        handlers::idea_boards::list_shapes,
        handlers::idea_boards::update_shape,
        handlers::idea_boards::delete_shape,
        handlers::idea_boards::create_link,
        handlers::idea_boards::list_links,
        handlers::idea_boards::update_link,
        handlers::idea_boards::delete_link,
        handlers::search::search,
        handlers::spaces::create_space,
        handlers::spaces::list_spaces,
        handlers::spaces::update_space,
        handlers::pages::create_page,
        handlers::pages::list_pages,
        handlers::pages::get_page,
        handlers::pages::get_page_permission,
        handlers::pages::update_page,
        handlers::pages::delete_page,
        handlers::pages::list_page_permissions,
        handlers::pages::create_page_permission,
        handlers::pages::delete_page_permission,
        handlers::pages::list_page_revisions,
        handlers::pages::create_page_revision,
        handlers::pages::delete_page_revision,
        handlers::pages::search_pages,
        handlers::pages::create_page_reference,
        handlers::pages::list_page_references,
        handlers::pages::list_page_backlinks,
        handlers::pages::delete_page_reference,
    ),
    components(schemas(
        error::ErrorResponse,
        handlers::me::MeResponse,
        models::note::Note,
        models::note::Visibility,
        models::note::CreateNoteRequest,
        models::note::AttachRequest,
        models::note::ReplyRequest,
        models::note::UpdateNoteRequest,
        models::note::NoteRevision,
        models::note::NoteRead,
        models::note::NoteUnreadStatus,
        models::note::NotesPage,
        models::note::NoteAttachment,
        models::note::NoteFolder,
        models::note::NoteFoldersPage,
        models::note::CreateNoteFolderRequest,
        models::note::UpdateNoteFolderRequest,
        models::system_principal::SystemPrincipal,
        models::system_principal::CreateSystemPrincipalRequest,
        models::system_principal::SystemPrincipalsPage,
        models::idea_board::IdeaBoardsPage,
        models::idea_board::Sticky,
        models::idea_board::StickiesPage,
        models::idea_board::CreateStickyRequest,
        models::idea_board::UpdateStickyRequest,
        models::idea_board::BoardShape,
        models::idea_board::BoardShapesPage,
        models::idea_board::CreateBoardShapeRequest,
        models::idea_board::UpdateBoardShapeRequest,
        models::idea_board::NoteLink,
        models::idea_board::NoteLinksPage,
        models::idea_board::CreateNoteLinkRequest,
        models::idea_board::UpdateNoteLinkRequest,
        search::SearchHit,
        search::SearchTypeResults,
        search::SearchResults,
        models::page::Space,
        models::page::CreateSpaceRequest,
        models::page::UpdateSpaceRequest,
        models::page::SpacesPage,
        models::page::Page,
        models::page::PagesPage,
        models::page::CreatePageRequest,
        models::page::UpdatePageRequest,
        models::page::PagePermission,
        models::page::PagePermissionLevelResponse,
        models::page::CreatePagePermissionRequest,
        models::page::PermissionLevel,
        models::page::PrincipalType,
        models::page::PageRevision,
        models::page::PageReference,
        models::page::PageBacklink,
        models::page::CreatePageReferenceRequest,
    )),
    tags(
        (name = "health", description = "Service health"),
        (name = "me", description = "Caller identity"),
        (name = "notes", description = "Notes: threaded, entity-attached comments"),
        (name = "search", description = "Cross-content search"),
        (name = "pages", description = "Pages: hierarchical documents organized into spaces"),
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

/// Not `#[tokio::main]` -- see `Config::tokio_worker_threads`'s own doc
/// comment. That macro's default multi-thread runtime sizes its worker pool
/// to `std::thread::available_parallelism()`, the *host's* full core count
/// under most container schedulers, not this pod's actual CPU allotment;
/// building the runtime manually here lets that be capped via `Config`
/// (loaded synchronously, before any runtime exists to size).
fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let _ = dotenvy::dotenv();

    let cfg = Config::from_env()?;

    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cfg.tokio_worker_threads)
        .enable_all()
        .build()
        .context("failed to build tokio runtime")?
        .block_on(run(cfg))
}

async fn run(cfg: Config) -> Result<()> {
    let pool = db::create_pool(&cfg.database_url, cfg.db_pool_max_size)?;
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

    // Loading the embedding model is CPU/blocking work and, on a fresh cache
    // dir, a one-time download -- same non-fatal-degrade posture as
    // OpenSearch above: semantic search is a bonus on top of lexical search,
    // never a startup requirement.
    let embedder = {
        let cache_dir = cfg.embedding_model_cache_dir.clone();
        let intra_threads = cfg.embedding_intra_threads;
        match tokio::task::spawn_blocking(move || embeddings::Embedder::new(cache_dir, intra_threads)).await {
            Ok(Ok(embedder)) => Some(embedder),
            Ok(Err(e)) => {
                tracing::warn!("embedding model failed to load, continuing lexical-only: {e:#}");
                None
            }
            Err(e) => {
                tracing::warn!("embedding model load task panicked, continuing lexical-only: {e:#}");
                None
            }
        }
    };

    let addr: std::net::SocketAddr = cfg.bind_addr().parse()?;

    let mcp_token_validator =
        TokenValidator::new(cfg.oauth2_jwks_url.clone(), cfg.oauth2_issuer.clone(), cfg.tack_mcp_canonical_uri.clone());
    let mcp_prc = ProtectedResourceConfig {
        resource_uri: cfg.tack_mcp_canonical_uri.clone(),
        authorization_server: cfg.oauth2_issuer.clone(),
        scopes_supported: vec!["tack:tools".to_owned()],
        jwks_uri: cfg.oauth2_jwks_url.clone(),
    };
    let mcp_service = mcp::make_tack_mcp_service(
        pool.clone(),
        search_client.clone(),
        embedder.clone(),
        host_from_uri(&cfg.tack_mcp_canonical_uri),
    );

    let user_management_base_url = cfg.oauth2_issuer.clone();
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
        embedder,
        user_management_http: reqwest::Client::new(),
        user_management_base_url,
    };

    let app = Router::new()
        .merge(SwaggerUi::new("/docs").url("/api-doc/openapi.json", ApiDoc::openapi()))
        .route("/health", get(health))
        .route("/me", get(handlers::me::me))
        .route("/notes", post(handlers::notes::create_note).get(handlers::notes::list_notes))
        .route("/notes/by-entity", get(handlers::notes::list_notes_by_attachment))
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
        .route(
            "/notes/:id/attachments",
            post(handlers::notes::create_attachment).get(handlers::notes::list_attachments),
        )
        .route("/notes/:id/attachments/:attachment_id", axum::routing::delete(handlers::notes::delete_attachment))
        .route(
            "/notes/:id/revisions",
            get(handlers::notes::list_revisions).post(handlers::notes::create_revision),
        )
        .route("/notes/:id/revisions/:revision_id", axum::routing::delete(handlers::notes::delete_revision))
        .route("/notes/:id/read", post(handlers::notes::mark_note_read))
        .route("/notes/unread", get(handlers::notes::list_unread))
        .route(
            "/note-folders",
            post(handlers::note_folders::create_note_folder).get(handlers::note_folders::list_note_folders),
        )
        .route("/note-folders/by-entity", get(handlers::note_folders::list_note_folders_by_attachment))
        .route(
            "/note-folders/:id",
            axum::routing::patch(handlers::note_folders::update_note_folder)
                .delete(handlers::note_folders::delete_note_folder),
        )
        .route(
            "/system-principals",
            post(handlers::system_principals::create_system_principal).get(handlers::system_principals::list_system_principals),
        )
        .route("/system-principals/:id", axum::routing::delete(handlers::system_principals::delete_system_principal))
        .route(
            "/idea-boards",
            post(handlers::idea_boards::create_board).get(handlers::idea_boards::list_boards),
        )
        .route("/idea-boards/by-entity", get(handlers::idea_boards::list_boards_by_entity))
        .route(
            "/idea-boards/:id",
            get(handlers::idea_boards::get_board)
                .patch(handlers::idea_boards::update_board)
                .delete(handlers::idea_boards::delete_board),
        )
        .route(
            "/idea-boards/:id/stickies",
            post(handlers::idea_boards::create_sticky).get(handlers::idea_boards::list_stickies),
        )
        .route(
            "/idea-boards/:id/shapes",
            post(handlers::idea_boards::create_shape).get(handlers::idea_boards::list_shapes),
        )
        .route(
            "/idea-boards/:id/links",
            post(handlers::idea_boards::create_link).get(handlers::idea_boards::list_links),
        )
        .route("/stickies/by-entity", get(handlers::idea_boards::get_sticky_by_entity))
        .route(
            "/stickies/:note_id",
            get(handlers::idea_boards::get_sticky)
                .patch(handlers::idea_boards::update_sticky)
                .delete(handlers::idea_boards::delete_sticky),
        )
        .route(
            "/shapes/:id",
            axum::routing::patch(handlers::idea_boards::update_shape).delete(handlers::idea_boards::delete_shape),
        )
        .route(
            "/links/:id",
            axum::routing::patch(handlers::idea_boards::update_link).delete(handlers::idea_boards::delete_link),
        )
        .route("/search", get(handlers::search::search))
        .route("/spaces", post(handlers::spaces::create_space).get(handlers::spaces::list_spaces))
        .route("/spaces/:id", axum::routing::patch(handlers::spaces::update_space))
        .route("/spaces/:id/pages", get(handlers::pages::list_pages))
        .route("/pages", post(handlers::pages::create_page))
        .route("/pages/search", get(handlers::pages::search_pages))
        .route(
            "/pages/:id",
            get(handlers::pages::get_page).patch(handlers::pages::update_page).delete(handlers::pages::delete_page),
        )
        .route("/pages/:id/permission", get(handlers::pages::get_page_permission))
        .route(
            "/pages/:id/permissions",
            post(handlers::pages::create_page_permission).get(handlers::pages::list_page_permissions),
        )
        .route("/pages/:id/permissions/:permission_id", axum::routing::delete(handlers::pages::delete_page_permission))
        .route(
            "/pages/:id/revisions",
            get(handlers::pages::list_page_revisions).post(handlers::pages::create_page_revision),
        )
        .route("/pages/:id/revisions/:revision_id", axum::routing::delete(handlers::pages::delete_page_revision))
        .route(
            "/pages/:id/references",
            get(handlers::pages::list_page_references).post(handlers::pages::create_page_reference),
        )
        .route("/pages/:id/references/:reference_id", axum::routing::delete(handlers::pages::delete_page_reference))
        .route("/pages/:id/backlinks", get(handlers::pages::list_page_backlinks))
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
