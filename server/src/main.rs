mod db;
mod error;
mod models;
mod routes;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use sqlx::SqlitePool;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub data_dir: Arc<PathBuf>,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "ESPuino Label Printer API",
        version = "0.1.0",
        description = "Manage prefixes and printable RFID card labels (front image + optional B&W back text)."
    ),
    paths(
        routes::create_prefix,
        routes::list_prefixes,
        routes::get_prefix,
        routes::delete_prefix,
        routes::delete_cards,
        routes::submit_prefix,
        routes::patch_status,
        routes::list_cards,
        routes::put_card,
        routes::post_card,
        routes::get_card,
        routes::get_card_image,
        routes::patch_card,
        routes::delete_card,
    ),
    components(schemas(
        models::Prefix,
        models::PrefixInput,
        models::PrefixStatus,
        models::StatusInput,
        models::Card,
        models::CardIndex,
        models::CardInput,
        models::CardPatch,
        error::ErrorBody,
    ))
)]
struct ApiDoc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=info".into()),
        )
        .init();

    let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "data".to_string()));
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| format!("sqlite://{}/cards.db", data_dir.display()));
    let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    std::fs::create_dir_all(&data_dir)?;

    let pool = db::init_pool(&database_url).await?;
    let state = AppState {
        pool,
        data_dir: Arc::new(data_dir),
    };

    // Static SPA (Svelte build). Served from disk so the frontend can be
    // rebuilt without recompiling the server. Unknown paths fall back to
    // index.html for client-side routing.
    let static_dir =
        std::env::var("STATIC_DIR").unwrap_or_else(|_| "server/static".to_string());
    let index_html = PathBuf::from(&static_dir).join("index.html");
    if !index_html.exists() {
        tracing::warn!(
            "no frontend build at {static_dir} (build it with `npm --prefix web run build`); \
             the API and Swagger UI are still served"
        );
    }

    let app = Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .merge(routes::router(state))
        .fallback_service(ServeDir::new(&static_dir).fallback(ServeFile::new(index_html)))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive());

    let addr: SocketAddr = bind.parse()?;
    tracing::info!("listening on http://{addr}  (app at /, swagger UI at /swagger-ui)");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}