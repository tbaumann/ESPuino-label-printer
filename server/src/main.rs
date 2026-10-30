mod auth;
mod db;
mod error;
mod models;
mod notify;
mod rate_limit;
mod render;
mod routes;
mod stripe;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::Router;
use sqlx::SqlitePool;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

/// How many checkout sessions one prefix may open per window. A customer
/// re-opening after a card decline or an abandoned page is a handful, never
/// dozens — anything above this in a minute is a loop.
const CHECKOUT_LIMIT_PER_PREFIX: u32 = 5;
const CHECKOUT_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub data_dir: Arc<PathBuf>,
    /// The printer's internal bitmap, read from `PRINT_RASTER_WIDTH`/`HEIGHT` at
    /// startup. Config, resolved once — not a per-render environment lookup, so a
    /// test (or a future per-printer setting) can hand the renderer a size
    /// without the process environment standing in for a parameter.
    pub raster: render::RasterSize,
    /// The markdown backend and its scratch directory, resolved the same way and
    /// for the same reason as `raster`. A missing backend is not fatal: every
    /// other back source renders, and a markdown card reports itself
    /// unprintable with the reason.
    pub markdown: render::MarkdownRenderer,
    /// Bearer token granting operator rights. `None` disables the admin API.
    pub admin_token: Option<Arc<str>>,
    /// Live admin UI logins (the session cookie), persisted in the database.
    pub sessions: auth::Sessions,
    /// Throttle on failed admin-credential attempts (wrong bearer token or
    /// wrong login token), keyed by client address.
    pub login_limiter: auth::LoginLimiter,
    /// Force `Secure` on the session cookie. `None` = infer per request from
    /// `X-Forwarded-Proto`, which is what you want behind a TLS proxy.
    pub cookie_secure: Option<bool>,
    /// Payment integration. `None` disables checkout and the webhook (503).
    pub stripe: Option<Arc<stripe::Stripe>>,
    /// Transactional email (Resend) for the paid-order messages. `None`
    /// disables sending entirely — the shop works and nobody is mailed.
    pub notifier: Option<Arc<notify::Notifier>>,
    /// Throttles checkout-session creation, keyed by prefix. Persisted per
    /// deployment, not part of the admin/session machinery.
    pub checkout_limiter: rate_limit::RateLimiter,
}

/// Declares the bearer scheme so Swagger UI offers an Authorize button and
/// generated clients know how to authenticate the admin operations.
struct SecurityAddon;

impl utoipa::Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        use utoipa::openapi::security::{
            ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme,
        };
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearerAuth",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("opaque")
                        .description(Some(
                            "Operator token: `Authorization: Bearer <ADMIN_TOKEN>`. \
                             Used by the CLI, which is not a browser and has no cookie.",
                        ))
                        .build(),
                ),
            );
            components.add_security_scheme(
                "sessionCookie",
                SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                    auth::SESSION_COOKIE,
                    "Admin UI session, set by POST /api/admin/session. HttpOnly, so script \
                     cannot read it. Mutating requests must additionally carry the \
                     x-esplp-csrf header.",
                ))),
            );
        }
    }
}

/// The surface the docs describe: everything a caller can reach without the
/// operator's credential.
///
/// Two declarations disqualify an operation, both of them in its own
/// `#[utoipa::path(...)]`: naming a credential (`bearerAuth`, `sessionCookie`),
/// and carrying the [`ADMIN_TAG`]. The tag is not redundant for
/// `POST /api/admin/session`, whose token travels in the BODY where no security
/// scheme can express it — there the tag is the only declaration that this is
/// the way *into* the admin surface, not part of the open one.
///
/// The filter reads those declarations instead of keeping a second `paths(...)`
/// list beside the derive's: an endpoint says what it needs in the one place
/// that already names it. Which also means the declaration has to be true — in
/// this server the operations calling `require_admin()` are exactly the ones
/// declaring a credential, and a new one must keep that up.
const ADMIN_TAG: &str = "admin";

/// Where a `$ref` to a component schema points, as it appears in the JSON.
const SCHEMA_REF: &str = "#/components/schemas/";

/// Drop every operation that needs the operator's credential, and with it the
/// components nothing kept still uses.
fn restrict_to_public_surface(openapi: &mut utoipa::openapi::OpenApi) {
    openapi.paths.paths.retain(|_, item| {
        item.operations.retain(|_, operation| {
            operation.security.is_none()
                && !operation.tags.iter().flatten().any(|tag| tag == ADMIN_TAG)
        });
        !item.operations.is_empty()
    });

    // What the kept operations still use: their schemas (or the docs keep a
    // Schemas section describing the admin session, i.e. the models of what was
    // just hidden) and their credential schemes (or the public docs offer an
    // Authorize button for a token nothing in them accepts). Both are read out of
    // the spec's own JSON, because `utoipa` has no accessor for "what does this
    // operation reference". A spec that cannot be read is left whole: a filter
    // that fails must never empty the document it filters.
    let paths = match serde_json::to_value(&openapi.paths) {
        Ok(paths) => paths,
        Err(err) => {
            tracing::warn!(
                "could not read the spec's paths ({err}): every component stays in the served docs"
            );
            return;
        }
    };
    let schemas = match reachable_schemas(openapi, &paths) {
        Some(schemas) => schemas,
        None => {
            tracing::warn!(
                "could not read the spec's schemas: every component stays in the served docs"
            );
            return;
        }
    };
    let schemes = named_security_schemes(&paths);

    if let Some(components) = openapi.components.as_mut() {
        components.schemas.retain(|name, _| schemas.contains(name));
        components
            .security_schemes
            .retain(|name, _| schemes.contains(name));
    }

    // Say which half this is: the info text describes both kinds of caller, and
    // the operator's own operations are exactly what was dropped from here.
    let described = openapi.info.description.take().unwrap_or_default();
    openapi.info.description = Some(format!(
        "{} This document is the public surface: the operations callable without an operator \
         credential. The operator's own are documented at /swagger-ui-full.",
        described.trim_end()
    ));
}

/// The schema names the given paths reach, following `$ref`s through the schemas
/// themselves (`Card` carries a `BackSource`). `None` if any of it cannot be
/// serialized, which the caller reads as "prune nothing".
fn reachable_schemas(
    openapi: &utoipa::openapi::OpenApi,
    paths: &serde_json::Value,
) -> Option<HashSet<String>> {
    fn collect(value: &serde_json::Value, into: &mut HashSet<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, item) in map {
                    match (key.as_str(), item.as_str()) {
                        ("$ref", Some(reference)) => {
                            if let Some(name) = reference.strip_prefix(SCHEMA_REF) {
                                into.insert(name.to_string());
                            }
                        }
                        _ => collect(item, into),
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|item| collect(item, into)),
            _ => {}
        }
    }

    let mut schemas: Vec<(String, serde_json::Value)> = Vec::new();
    if let Some(components) = openapi.components.as_ref() {
        for (name, schema) in &components.schemas {
            schemas.push((name.clone(), serde_json::to_value(schema).ok()?));
        }
    }

    let mut reached: HashSet<String> = HashSet::new();
    collect(paths, &mut reached);

    // Schemas reference one another, so the set grows until it settles. There
    // are a few dozen of them, so this takes two or three passes.
    loop {
        let before = reached.len();
        for (name, schema) in &schemas {
            if reached.contains(name) {
                collect(schema, &mut reached);
            }
        }
        if reached.len() == before {
            return Some(reached);
        }
    }
}

/// The security scheme names the given paths name, i.e. the keys of every
/// `security` requirement in them.
fn named_security_schemes(paths: &serde_json::Value) -> HashSet<String> {
    let mut named: HashSet<String> = HashSet::new();
    match paths {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key == "security" {
                    if let serde_json::Value::Array(requirements) = value {
                        for requirement in requirements {
                            if let serde_json::Value::Object(names) = requirement {
                                named.extend(names.keys().cloned());
                            }
                        }
                    }
                }
                named.extend(named_security_schemes(value));
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                named.extend(named_security_schemes(item));
            }
        }
        _ => {}
    }
    named
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "ESPuino Label Printer API",
        version = "0.1.0",
        description = "Manage prefixes and printable RFID card labels (front image + optional B&W back text). \
            Two kinds of caller: a customer, whose credential is the prefix uuid they know, and the operator, \
            who presents the admin bearer token for listing, status changes and overriding the print freeze."
    ),
    modifiers(&SecurityAddon),
    paths(
        routes::create_prefix,
        routes::put_prefix,
        routes::list_prefixes,
        routes::get_prefix,
        routes::patch_prefix_ribbon,
        routes::delete_prefix,
        routes::delete_cards,
        routes::patch_status,
        routes::list_cards,
        routes::put_card,
        routes::post_card,
        routes::get_card,
        routes::get_front_image,
        routes::get_back_image,
        routes::get_front_display,
        routes::get_back_display,
        routes::get_front_raster,
        routes::get_back_raster,
        routes::put_back_image,
        routes::patch_card,
        routes::delete_card,
                routes::get_print_queue,
                routes::mark_card_printed,
                routes::admin_login,
        routes::admin_session,
        routes::admin_logout,
        routes::needs_attention,
        routes::shop_config,
        routes::put_ribbon_stock,
        routes::put_ribbon_price,
        routes::create_checkout,
        routes::stripe_webhook,
        routes::get_prefix_shipping,
    ),
    // The names here are what a `$ref` must spell, and a path attribute's
    // `body = <tokens>` is named by the tokens it is given: `body = Prefix`
    // points at `Prefix`, while `body = crate::error::ErrorBody` points at
    // `crate.error.ErrorBody`, which nothing registers. So the attribute spells
    // the bare type name and does NOT need it imported — the utoipa macro reads
    // the name out of the tokens without resolving them.
    components(schemas(
        models::Prefix,
        models::PrefixInput,
        models::PrefixStatus,
        models::StatusInput,
        models::Card,
        models::CardIndex,
        models::CardInput,
        models::CardPatch,
        models::PrefixRibbonPatch,
        models::PrintedInput,
        models::BackSource,
        models::PrintQueue,
        models::PrintQueueCard,
        models::AdminLogin,
        models::AdminSession,
        models::NeedsAttentionItem,
        models::NeedsAttentionKind,
        models::ShopConfig,
        models::RibbonPrice,
        models::Ribbon,
        models::RibbonStock,
        models::RibbonStockPatch,
        models::RibbonPricePatch,
        models::ShippingAddress,
        stripe::CheckoutSession,
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

    // Operator credential. Unset or blank means the admin API is switched off
    // entirely, rather than left open.
    let admin_token: Option<Arc<str>> = std::env::var("ADMIN_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .map(Arc::<str>::from);
    if admin_token.is_none() {
        tracing::warn!(
            "ADMIN_TOKEN is not set: the admin API is disabled \
             (listing prefixes and status changes will answer 503)"
        );
    }

    // Force the session cookie's Secure flag, or leave it to be inferred from
    // X-Forwarded-Proto per request. Unrecognised values are ignored.
    let cookie_secure = std::env::var("SESSION_COOKIE_SECURE").ok().and_then(|v| {
        match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" => Some(true),
            "0" | "false" | "no" => Some(false),
            _ => None,
        }
    });

    // Payment. Unset or blank STRIPE_SECRET_KEY switches checkout and the
    // webhook off — they answer 503 — rather than leaving a half-configured
    // payment path that looks like it works.
    let stripe = stripe::Stripe::from_env().map(Arc::new);
    if stripe.is_none() {
        tracing::info!("STRIPE_SECRET_KEY is not set: the shop endpoints will answer 503");
    }

    // Email. Unset RESEND_API_KEY (or no EMAIL_FROM) switches sending off, with
    // a warning from `from_env` saying which. An email that cannot be sent must
    // never fail the payment webhook, so this is deliberately fire-and-forget
    // from the caller's point of view.
    let notifier = notify::Notifier::from_env().map(Arc::new);

    // Markdown backs are rendered by an external backend (`MARKIE_BIN`, else
    // `markie` on `PATH`). A missing one costs only the markdown backs, so it is
    // a warning naming what will not render — like the SPA warning below, and
    // unlike the payment path, which must never look like it works.
    let markdown = render::MarkdownRenderer::from_env(&data_dir);
    if let Err(why) = markdown.probe() {
        tracing::warn!("{why}: markdown backs will report themselves unprintable");
    }

    let state = AppState {
        pool: pool.clone(),
        data_dir: Arc::new(data_dir),
        raster: render::RasterSize::from_env(),
        markdown,
        admin_token,
        sessions: auth::Sessions::new(pool),
        login_limiter: auth::LoginLimiter::default(),
        cookie_secure,
        stripe,
        notifier,
        checkout_limiter: rate_limit::RateLimiter::new(CHECKOUT_LIMIT_PER_PREFIX, CHECKOUT_WINDOW),
    };

    // Static SPA (Svelte build). Served from disk so the frontend can be
    // rebuilt without recompiling the server. Unknown paths fall back to
    // index.html for client-side routing, and `static_cache_policy` decides what
    // a browser may keep of it — the defaults are wrong for a store path.
    let static_dir = std::env::var("STATIC_DIR").unwrap_or_else(|_| "server/static".to_string());
    let index_html = PathBuf::from(&static_dir).join("index.html");
    if !index_html.exists() {
        tracing::warn!(
            "no frontend build at {static_dir} (build it with `npm --prefix web run build`); \
             the API and Swagger UI are still served"
        );
    }

    // Two docs. `/swagger-ui` describes the public surface (see
    // `restrict_to_public_surface`); `/swagger-ui-full` describes everything the
    // router serves, the operator's own operations included.
    let mut public = ApiDoc::openapi();
    restrict_to_public_surface(&mut public);

    let app = Router::new()
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", public))
        .merge(
            SwaggerUi::new("/swagger-ui-full")
                .url("/api-docs/openapi-full.json", ApiDoc::openapi()),
        )
        .merge(routes::router(state))
        .fallback_service(ServeDir::new(&static_dir).fallback(ServeFile::new(index_html)))
        .layer(middleware::from_fn(static_cache_policy))
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = bind.parse()?;
    tracing::info!(
        "listening on http://{addr}  (app at /, api docs at /swagger-ui for the public \
         surface and /swagger-ui-full for everything)"
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// What a browser may keep of the served frontend.
///
/// The defaults are wrong here because nothing is served from a working copy.
/// Every file in a Nix store path carries the *same* mtime, so tower-http's
/// `Last-Modified` is a constant: a browser that has `index.html` cached
/// revalidates, is told `304 Not Modified`, and keeps the previous shell — and
/// with it the previous asset hashes, i.e. the previous UI, however often the
/// frontend is rebuilt and however often `nix run` is restarted. That is exactly
/// what "the web UI is not in sync" looks like.
///
/// So: the shell and the files beside it are handed out uncacheable, and their
/// conditional headers are dropped before anything can answer them from a
/// timestamp that never moves. The content-hashed assets are safe to keep for
/// good — a rebuilt asset is a new name — and a hashed asset that is *missing* is
/// a 404 rather than the shell, so a tab holding an old hash fails loudly instead
/// of running `index.html` as if it were a module.
async fn static_cache_policy(mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    let asset = path.starts_with("/assets/");
    // The API answers with what it has just written, and sets its own headers.
    let api = path.starts_with("/api") || path.starts_with("/swagger-ui");

    if !asset && !api {
        // Nothing served from here can answer these truthfully.
        req.headers_mut().remove(header::IF_MODIFIED_SINCE);
        req.headers_mut().remove(header::IF_NONE_MATCH);
    }

    let mut res = next.run(req).await;

    if asset {
        // `ServeDir`'s fallback is the shell, which is never what a caller after
        // an asset wants to be handed.
        let html = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/html"));
        if html || !res.status().is_success() {
            return StatusCode::NOT_FOUND.into_response();
        }
        res.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
        // The name carries the hash; the timestamp says nothing.
        res.headers_mut().remove(header::LAST_MODIFIED);
        return res;
    }

    if !api {
        res.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        res.headers_mut().remove(header::LAST_MODIFIED);
        res.headers_mut().remove(header::ETAG);
    }
    res
}
