use std::path::PathBuf;

use axum::body::Bytes;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Deserialize;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::auth;
use crate::db;
use crate::error::ApiError;
use crate::models::{
    AdminLogin, AdminSession, BackSource, Card, CardIndex, CardPatch, NeedsAttentionItem,
    NeedsAttentionKind, Prefix, PrefixInput, PrefixRibbonPatch, PrefixStatus, PrintQueue, Ribbon,
    RibbonPrice, RibbonPricePatch, RibbonStock, RibbonStockPatch, ShopConfig, StatusInput,
};
use crate::notify;
use crate::render;
use crate::stripe;
use crate::AppState;

fn parse_uuid(s: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(s).map_err(|_| ApiError::BadRequest(format!("invalid uuid: {s}")))
}

fn extension_for(content_type: &str) -> &'static str {
    match content_type {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

/// The content type we serve for a stored blob, derived from its extension.
///
/// The stored `image_content_type` is attacker-chosen (it comes straight from
/// the multipart upload), so it must never be echoed back verbatim — that is
/// how a `text/html` upload becomes a served `text/html` page. We only ever
/// serve one of the accepted image types, or `application/octet-stream` for
/// anything unrecognised, so a browser can never be tricked into rendering the
/// bytes as HTML.
fn content_type_for(ext: &str) -> &'static str {
    match ext {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        _ => "application/octet-stream",
    }
}

/// Write an image blob under `blobs/{prefix}/`, replacing any earlier blob for
/// the same filename (e.g. a re-upload as jpg over a stored png). `filename`
/// is the full file name, e.g. `"3.png"` (front) or `"3-back.png"` (back).
/// Returns the blob's path relative to the data dir, as stored in the DB.
fn write_blob(
    data_dir: &PathBuf,
    prefix: Uuid,
    filename: &str,
    bytes: &[u8],
) -> Result<String, ApiError> {
    let rel = format!("blobs/{prefix}/{filename}");
    let abs = data_dir.join(&rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
        // Match by bare name prefix (e.g. "3-"), so re-uploading a front image
        // can delete the old front `3.png` without touching the back `3-back.png`
        // (which starts with a different prefix), and vice versa.
        let keep = abs.file_name().and_then(|s| s.to_str()).unwrap_or_default();
        let name_prefix = filename
            .split_once('.')
            .map(|(stem, _)| format!("{stem}."))
            .unwrap_or_default();
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let fname = fname.to_string_lossy();
                if fname.starts_with(&name_prefix) && fname != keep {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
    std::fs::write(&abs, bytes)?;
    Ok(rel)
}

/// A card write, as the multipart form described it.
struct StoredCard {
    name: String,
    serial_number: Option<String>,
    /// The card's back side, already validated against the prefix's ribbon.
    back: BackSource,
    /// Markdown for a `markdown` back; `None` otherwise.
    back_text: Option<String>,
    image_rel: String,
    image_content_type: String,
    /// The stored back image, when the form carried one.
    back_image_rel: Option<String>,
    back_image_content_type: Option<String>,
}

/// Pull `name`, `serial_number`, `back`, `back_text`, an `image` file and an
/// optional `back_image` file out of a multipart form, validating the fields
/// against each other *before* writing anything to disk. The prefix's ribbon
/// has no say here: it changes how a back is rendered, never whether one is
/// allowed.
///
/// The `back_image` is **optional even for an `image` back**: a card exists
/// from its front image, and the back image is added per card afterwards
/// (`put_card_back_image`, or this same form). Requiring it here would mean the
/// front-image upload — a bulk, folder-at-a-time flow — could create nothing at
/// all for a double-sided card. What has to hold is that no *order* is sold with
/// a back missing, and that is checked where the money is
/// (`ensure_order_printable`), not here.
async fn store_card_form(
    mut mp: Multipart,
    data_dir: &PathBuf,
    prefix: Uuid,
    id: i64,
) -> Result<StoredCard, ApiError> {
    let mut name: Option<String> = None;
    let mut serial: Option<String> = None;
    let mut back_field: Option<BackSource> = None;
    let mut back_text: Option<String> = None;
    let mut image: Option<(Vec<u8>, String)> = None;
    let mut back_image: Option<(Vec<u8>, String)> = None;

    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("malformed multipart: {e}")))?
    {
        match field.name() {
            Some("name") => {
                name = Some(
                    field
                        .text()
                        .await
                        .map_err(|e| ApiError::BadRequest(e.to_string()))?,
                )
            }
            Some("serial_number") => {
                let v = field
                    .text()
                    .await
                    .map_err(|e| ApiError::BadRequest(e.to_string()))?;
                if !v.is_empty() {
                    serial = Some(v);
                }
            }
            Some("back") => {
                let v = field
                    .text()
                    .await
                    .map_err(|e| ApiError::BadRequest(e.to_string()))?;
                back_field = Some(BackSource::parse(v.trim()).ok_or_else(|| {
                    ApiError::BadRequest(format!(
                        "unknown back '{v}': expected one of none, front, image, markdown"
                    ))
                })?);
            }
            Some("back_text") => {
                let v = field
                    .text()
                    .await
                    .map_err(|e| ApiError::BadRequest(e.to_string()))?;
                if !v.is_empty() {
                    back_text = Some(v);
                }
            }
            Some("image") => {
                let ct = field
                    .content_type()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::BadRequest(e.to_string()))?;
                image = Some((bytes.to_vec(), ct));
            }
            Some("back_image") => {
                let ct = field
                    .content_type()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                let bytes = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::BadRequest(e.to_string()))?;
                back_image = Some((bytes.to_vec(), ct));
            }
            _ => {}
        }
    }

    let name = name.ok_or_else(|| ApiError::BadRequest("field 'name' is required".into()))?;
    let (bytes, ct) =
        image.ok_or_else(|| ApiError::BadRequest("file field 'image' is required".into()))?;
    if bytes.is_empty() {
        return Err(ApiError::BadRequest("image is empty".into()));
    }
    if let Some((bbytes, _)) = &back_image {
        if bbytes.is_empty() {
            return Err(ApiError::BadRequest("back_image is empty".into()));
        }
    }

    // What the card's back is, from the two fields that can say it. A sent
    // `back_image` implies an `image` back — that is the whole of what it means
    // — but an explicit `back` that contradicts it is a form disagreeing with
    // itself, so it is refused rather than quietly resolved one way.
    let back = match (back_field, back_image.is_some()) {
        (Some(b), true) if b != BackSource::Image => {
            return Err(ApiError::BadRequest(format!(
                "a back_image was sent with back = '{b}', which has no use for one"
            )))
        }
        (Some(b), _) => b,
        (None, true) => BackSource::Image,
        (None, false) => BackSource::None,
    };

    if back_text.is_some() && back != BackSource::Markdown {
        return Err(ApiError::BadRequest(format!(
            "back_text is only valid for a 'markdown' back, not '{back}'"
        )));
    }

    let image_rel = write_blob(
        data_dir,
        prefix,
        &format!("{id}.{}", extension_for(&ct)),
        &bytes,
    )?;
    let (back_image_rel, back_image_content_type) = match back_image {
        Some((bbytes, bct)) => {
            let rel = write_blob(
                data_dir,
                prefix,
                &format!("{id}-back.{}", extension_for(&bct)),
                &bbytes,
            )?;
            (Some(rel), Some(bct))
        }
        None => (None, None),
    };

    Ok(StoredCard {
        name,
        serial_number: serial,
        back,
        back_text,
        image_rel,
        image_content_type: ct,
        back_image_rel,
        back_image_content_type,
    })
}

/// Re-render every derived artifact a card needs, synchronously, and record the
/// availability statuses.
///
/// Called after any write that changes a card's front or back (upload, back
/// image, back source, back_text). Rendering is synchronous: by the time the
/// write response returns, the display images and rasters are `ready` — or
/// `failed`, with the reason stored for the admin to inspect. Originals are
/// never touched.
///
/// A side the card has nothing to render — a single-sided card, or a `image`/
/// `markdown` back whose material has not arrived yet — leaves that side's
/// statuses `pending`, which the model reads as non-existent rather than a real
/// pending render. The UI then shows the card it has, and the print queue
/// reports the card not-yet-printable rather than silently printing the
/// original.
///
/// Raster blobs: `<id>-front-print.bmp` / `<id>-back-print.bmp`; display
/// images: `<id>-front-display.png` / `<id>-back-display.png`, all under
/// `blobs/<prefix>/`.
async fn rerender_card(
    st: &AppState,
    prefix: Uuid,
    id: i64,
) -> Result<crate::models::Card, ApiError> {
    use crate::models::RenderStatus;
    use crate::render::RenderResult;

    // The prefix's ribbon decides the depth every back side is rendered in. It
    // is the same ribbon the print queue advertises to the spooler — the queue's
    // `back` is what tells the spooler whether to call `set_image_buffer` or
    // `set_black_buffer`, so a renderer that disagreed would hand a 24-bit
    // raster to the black-buffer call.
    let ribbon = db::get_prefix(&st.pool, prefix).await?.ribbon;
    let card = db::get_card(&st.pool, prefix, id).await?;
    let raster = st.raster;

    // Read a card's original blob (front or back image) from disk. A missing
    // file is the one case where the whole render must not fail: it is how a
    // back image *not yet uploaded* is represented.
    let read_original = |rel: &str| std::fs::read(st.data_dir.join(rel)).ok();

    // Front side (always present). Display + print raster.
    let front_rel = format!(
        "blobs/{prefix}/{id}.{}",
        extension_for(&card.image_content_type)
    );
    let front_original = read_original(&front_rel);
    let front_pair = match front_original.as_deref() {
        Some(orig) => match render::render_front_display(orig) {
            RenderResult::Ready(bytes) => {
                write_blob(&st.data_dir, prefix, &format!("{id}-front-display.png"), &bytes)?;
                (RenderStatus::Ready, None)
            }
            RenderResult::Failed(e) => (RenderStatus::Failed, Some(e)),
        },
        None => (
            RenderStatus::Failed,
            Some("front image missing from disk".into()),
        ),
    };
    let front_raster = match front_original.as_deref() {
        Some(orig) => match render::render_front_raster(orig, &raster) {
            RenderResult::Ready(bytes) => {
                write_blob(&st.data_dir, prefix, &format!("{id}-front-print.bmp"), &bytes)?;
                (RenderStatus::Ready, None)
            }
            RenderResult::Failed(e) => (RenderStatus::Failed, Some(e)),
        },
        None => (
            RenderStatus::Failed,
            Some("front image missing from disk".into()),
        ),
    };

    // The back's own uploaded image, when this card's back is an image. Named
    // from the stored content type, so a re-upload in another format is found.
    let back_original = card.back_image_content_type.as_deref().and_then(|ct| {
        read_original(&format!("blobs/{prefix}/{id}-back.{}", extension_for(ct)))
    });

    // Back side, from the card's own choice. The *ribbon* decides the colour
    // depth every back source is rendered in — 24-bit colour on YMCKO, 1-bit
    // black on YMCKOKO — and nothing else about the back depends on it. A
    // picture on a black panel is reduced to black and white rather than
    // refused; the choice is the customer's.
    let depth = ribbon.back_depth();
    let (back_display, back_raster) = match card.back {
        // A single-sided card: no back side exists, so there is nothing to
        // render and the statuses stay absent.
        BackSource::None => (None, None),
        // The front panel printed a second time, so its source is the front
        // original and it needs no upload of its own.
        BackSource::Front => match front_original.as_deref() {
            Some(orig) => (
                Some(back_display(st, prefix, id, orig, depth)?),
                Some(back_raster(st, prefix, id, orig, &raster, depth)?),
            ),
            // The front failed to render above; nothing to derive a back from.
            None => (None, None),
        },
        BackSource::Image => match back_original.as_deref() {
            Some(orig) => (
                Some(back_display(st, prefix, id, orig, depth)?),
                Some(back_raster(st, prefix, id, orig, &raster, depth)?),
            ),
            // Not uploaded yet: the card is incomplete rather than having an
            // empty back silently substituted.
            None => (None, None),
        },
        BackSource::Markdown => match card.back_text.as_deref().filter(|t| !t.trim().is_empty()) {
            // The markdown renderer is a stub until a real markdown→image
            // backend lands; until then it always reports failed with the
            // reason, so the card is visibly not printable rather than having a
            // blank back silently substituted.
            Some(text) => {
                let res = render::render_back_markdown_stub(text, &raster, depth);
                let (status, err) = match res {
                    RenderResult::Ready(_) => (RenderStatus::Ready, None),
                    RenderResult::Failed(e) => (RenderStatus::Failed, Some(e)),
                };
                (Some((status, err.clone())), Some((status, err)))
            }
            None => (None, None),
        },
    };

    db::set_card_render_status(
        &st.pool,
        prefix,
        id,
        Some(front_pair),
        Some(front_raster),
        back_display,
        back_raster,
    )
    .await
}

/// Render a back display image from `original`, write it and report. The
/// preview is the depth-accurate render: on a black panel it is the same
/// black-and-white reduction the raster is built from, so what the row shows is
/// what the printer puts on the card.
fn back_display(
    st: &AppState,
    prefix: Uuid,
    id: i64,
    original: &[u8],
    depth: crate::models::BackDepth,
) -> Result<(crate::models::RenderStatus, Option<String>), ApiError> {
    use crate::models::RenderStatus;
    use crate::render::RenderResult;
    Ok(match render::render_back_display(original, depth) {
        RenderResult::Ready(bytes) => {
            write_blob(&st.data_dir, prefix, &format!("{id}-back-display.png"), &bytes)?;
            (RenderStatus::Ready, None)
        }
        RenderResult::Failed(e) => (RenderStatus::Failed, Some(e)),
    })
}

/// Render a back print raster from `original`, write it and report. `size` is
/// the printer's internal bitmap; `depth` is the ribbon's, which decides the
/// format — a 24-bit colour BMP, or a 1-bit one for the second black panel.
fn back_raster(
    st: &AppState,
    prefix: Uuid,
    id: i64,
    original: &[u8],
    size: &crate::render::RasterSize,
    depth: crate::models::BackDepth,
) -> Result<(crate::models::RenderStatus, Option<String>), ApiError> {
    use crate::models::RenderStatus;
    use crate::render::RenderResult;
    Ok(match render::render_back_raster(original, size, depth) {
        RenderResult::Ready(bytes) => {
            write_blob(&st.data_dir, prefix, &format!("{id}-back-print.bmp"), &bytes)?;
            (RenderStatus::Ready, None)
        }
        RenderResult::Failed(e) => (RenderStatus::Failed, Some(e)),
    })
}

/// Create a prefix, optionally choosing its uuid.
#[utoipa::path(
    post, path = "/api/prefixes", tag = "prefixes",
    request_body = PrefixInput,
    responses((status = 201, description = "created", body = Prefix), (status = 409, description = "uuid already exists", body = crate::error::ErrorBody))
)]
pub async fn create_prefix(
    State(st): State<AppState>,
    Json(input): Json<PrefixInput>,
) -> Result<(StatusCode, Json<Prefix>), ApiError> {
    let p = db::create_prefix(&st.pool, input.uuid, input.ribbon).await?;
    Ok((StatusCode::CREATED, Json(p)))
}

/// Create a prefix at the uuid in the path, or return the existing one if it
/// is already there. Idempotent: the same PUT twice is harmless, and answers
/// 201 on first creation and 200 on subsequent (no-op) calls.
#[utoipa::path(
    put, path = "/api/prefixes/{prefix}", tag = "prefixes",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    responses(
        (status = 201, description = "created", body = Prefix),
        (status = 200, description = "already exists", body = Prefix),
        (status = 400, description = "invalid uuid", body = crate::error::ErrorBody)
    )
)]
pub async fn put_prefix(
    State(st): State<AppState>,
    Path(prefix): Path<String>,
) -> Result<(StatusCode, Json<Prefix>), ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let (p, created) = db::upsert_prefix(&st.pool, prefix).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(p)))
}

/// Query for listing prefixes.
#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    /// Only return prefixes in this status (e.g. `paid` for the print queue).
    pub status: Option<String>,
}

/// List prefixes, optionally filtered by status. **Admin only** — the full
/// list exposes every customer's uuid, which is their only credential.
#[utoipa::path(get, path = "/api/prefixes", tag = "prefixes",
    params(("status" = Option<String>, Query, description = "filter by status (draft|payment_pending|paid|queued|printing|printed|cancelled)")),
    security(("bearerAuth" = []), ("sessionCookie" = [])),
    responses(
        (status = 200, body = [Prefix]),
        (status = 401, description = "admin token required", body = crate::error::ErrorBody),
        (status = 503, description = "no admin token configured", body = crate::error::ErrorBody)
    ))]
pub async fn list_prefixes(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<StatusQuery>,
) -> Result<Json<Vec<Prefix>>, ApiError> {
    auth::resolve(&st, &headers).await?.require_admin()?;
    let status = match q.status.as_deref() {
        None => None,
        Some(s) => Some(
            PrefixStatus::parse(s)
                .ok_or_else(|| ApiError::BadRequest(format!("invalid status: {s}")))?,
        ),
    };
    Ok(Json(db::list_prefixes(&st.pool, status).await?))
}

/// Fetch one prefix with its card count.
#[utoipa::path(get, path = "/api/prefixes/{prefix}", tag = "prefixes",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    responses((status = 200, body = Prefix), (status = 404, body = crate::error::ErrorBody)))]
pub async fn get_prefix(
    State(st): State<AppState>,
    Path(prefix): Path<String>,
) -> Result<Json<Prefix>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    Ok(Json(db::get_prefix(&st.pool, prefix).await?))
}

/// Index of all cards under a prefix.
#[utoipa::path(get, path = "/api/prefixes/{prefix}/cards", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    responses((status = 200, body = [Card]), (status = 404, body = crate::error::ErrorBody)))]
pub async fn list_cards(
    State(st): State<AppState>,
    Path(prefix): Path<String>,
) -> Result<Json<CardIndex>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    db::get_prefix(&st.pool, prefix).await?; // 404 if the prefix is unknown
    let cards = db::list_cards(&st.pool, prefix).await?;
    Ok(Json(CardIndex { prefix, cards }))
}

/// Create or replace a card at a client-chosen id (PUT semantics).
#[utoipa::path(
    put, path = "/api/prefixes/{prefix}/cards/{id}", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    request_body(content = CardInput, content_type = "multipart/form-data"),
    responses((status = 200, body = Card), (status = 404, body = crate::error::ErrorBody), (status = 400, body = crate::error::ErrorBody))
)]
pub async fn put_card(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((prefix, id)): Path<(String, i64)>,
    mp: Multipart,
) -> Result<Json<Card>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let caller = auth::resolve(&st, &headers).await?;
    db::ensure_writable(&st.pool, prefix, caller.is_admin()).await?;
    let c = store_card_form(mp, &st.data_dir, prefix, id).await?;
    db::upsert_card(
        &st.pool,
        prefix,
        id,
        &c.name,
        c.serial_number.as_deref(),
        c.back,
        c.back_text.as_deref(),
        &c.image_rel,
        &c.image_content_type,
        c.back_image_rel.as_deref(),
        c.back_image_content_type.as_deref(),
    )
    .await?;
    Ok(Json(rerender_card(&st, prefix, id).await?))
}

/// Create a card with a server-assigned id (POST semantics).
#[utoipa::path(
    post, path = "/api/prefixes/{prefix}/cards", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    request_body(content = CardInput, content_type = "multipart/form-data"),
    responses((status = 201, body = Card), (status = 404, body = crate::error::ErrorBody), (status = 400, body = crate::error::ErrorBody))
)]
pub async fn post_card(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(prefix): Path<String>,
    mp: Multipart,
) -> Result<(StatusCode, Json<Card>), ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let caller = auth::resolve(&st, &headers).await?;
    db::ensure_writable(&st.pool, prefix, caller.is_admin()).await?;
    let id = db::next_card_id(&st.pool, prefix).await?;
    let c = store_card_form(mp, &st.data_dir, prefix, id).await?;
    db::upsert_card(
        &st.pool,
        prefix,
        id,
        &c.name,
        c.serial_number.as_deref(),
        c.back,
        c.back_text.as_deref(),
        &c.image_rel,
        &c.image_content_type,
        c.back_image_rel.as_deref(),
        c.back_image_content_type.as_deref(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(rerender_card(&st, prefix, id).await?)))
}

/// Fetch one card's metadata.
#[utoipa::path(get, path = "/api/prefixes/{prefix}/cards/{id}", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    responses((status = 200, body = Card), (status = 404, body = crate::error::ErrorBody)))]
pub async fn get_card(
    State(st): State<AppState>,
    Path((prefix, id)): Path<(String, i64)>,
) -> Result<Json<Card>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    Ok(Json(db::get_card(&st.pool, prefix, id).await?))
}

/// Fetch a card's front image bytes.
#[utoipa::path(get, path = "/api/prefixes/{prefix}/cards/{id}/image", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    responses((status = 200, description = "image bytes", content_type = "image/png"), (status = 404, body = crate::error::ErrorBody)))]
pub async fn get_card_image(
    State(st): State<AppState>,
    Path((prefix, id)): Path<(String, i64)>,
) -> Result<Response, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let card = db::get_card(&st.pool, prefix, id).await?;
    let ext = extension_for(&card.image_content_type);
    let rel = format!("blobs/{}/{}.{}", prefix, id, ext);
    let bytes = std::fs::read(st.data_dir.join(&rel))
        .map_err(|_| ApiError::NotFound(format!("image for card {id}")))?;
    Ok((
        [
            (header::CONTENT_TYPE, content_type_for(ext)),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}

/// Fetch a card's colour back image bytes (only present in `double_sided_color`).
#[utoipa::path(get, path = "/api/prefixes/{prefix}/cards/{id}/back-image", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    responses((status = 200, description = "image bytes", content_type = "image/png"), (status = 404, body = crate::error::ErrorBody)))]
pub async fn get_card_back_image(
    State(st): State<AppState>,
    Path((prefix, id)): Path<(String, i64)>,
) -> Result<Response, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let card = db::get_card(&st.pool, prefix, id).await?;
    let Some(ct) = &card.back_image_content_type else {
        return Err(ApiError::NotFound(format!(
            "back image for card {id} in prefix {prefix} has never been uploaded"
        )));
    };
    let ext = extension_for(ct);
    let rel = format!("blobs/{prefix}/{id}-back.{ext}");
    let bytes = std::fs::read(st.data_dir.join(&rel))
        .map_err(|_| ApiError::NotFound(format!("back image for card {id}")))?;
    Ok((
        [
            (header::CONTENT_TYPE, content_type_for(ext)),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}

/// Serve a derived artifact (display image or print raster) with the status
/// semantics the handoff specifies: `404` when the side does not exist for this
/// card, `423` when it is still pending, `500` when the render failed, and
/// `200 image/<type>` when ready.
///
/// `status` is the card's render status for this artifact; `rel` is the blob
/// path (only read when `ready`).
///
/// Every ready artifact carries an `ETag` (a hash of the bytes) and
/// `Cache-Control: no-cache`, and a request that presents a matching
/// `If-None-Match` gets a `304` instead. The URL of an artifact never changes —
/// it is `/cards/{id}/display-back`, not a versioned path — but its *content*
/// does, whenever the card is re-rendered: a re-upload, a back-source change, or
/// a ribbon switch that moves the back between colour and 1-bit. Without a
/// validator a browser is free to keep showing the old picture after any of
/// those, which is exactly the bug this prevents; with one, a stale copy is
/// revalidated (cheaply) instead of trusted. Clients that want a hard guarantee
/// can version the URL themselves — the SPA does, off the card's `updated_at`.
fn serve_artifact(
    st: &AppState,
    req: &HeaderMap,
    status: Option<crate::models::RenderStatus>,
    rel: &str,
    content_type: &'static str,
) -> Result<Response, ApiError> {
    use crate::models::RenderStatus;
    match status {
        // The side does not exist for this card (no back in this mode, or a
        // colour card with no back image yet).
        None => Err(ApiError::NotFound(format!("no such side for this card"))),
        Some(RenderStatus::Pending) => Err(ApiError::Locked(
            "the artifact is still being rendered; retry shortly".into(),
        )),
        Some(RenderStatus::Failed) => Err(ApiError::Internal(
            "the artifact failed to render; see the card's render status for the reason".into(),
        )),
        Some(RenderStatus::Ready) => {
            let bytes = std::fs::read(st.data_dir.join(rel))
                .map_err(|_| ApiError::NotFound(format!("artifact {rel} missing from disk")))?;
            let etag = artifact_etag(&bytes);
            if req
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.split(',').any(|tag| {
                        let tag = tag.trim();
                        tag == etag || tag.trim_start_matches("W/") == etag
                    })
                })
            {
                return Ok((
                    StatusCode::NOT_MODIFIED,
                    [(header::ETAG, etag)],
                )
                    .into_response());
            }
            Ok((
                [
                    (header::CONTENT_TYPE, content_type.to_string()),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                    // Revalidate before reuse: the bytes for this path can
                    // change without the path changing.
                    (header::CACHE_CONTROL, "no-cache".to_string()),
                    (header::ETAG, etag),
                ],
                bytes,
            )
                .into_response())
        }
    }
}

/// A cheap, strong-enough validator for one derived artifact: its length plus a
/// hash of its bytes. Two renders that produce the same picture share it, which
/// is the point — a re-render with unchanged output costs a `304`.
fn artifact_etag(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    format!("\"{:x}-{}\"", digest, bytes.len())
}

/// Fetch a card's front display image (the normalized PNG the frontend shows).
#[utoipa::path(get, path = "/api/prefixes/{prefix}/cards/{id}/display-image", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    responses(
        (status = 200, description = "display image bytes", content_type = "image/png"),
        (status = 404, body = crate::error::ErrorBody),
        (status = 423, description = "still rendering", body = crate::error::ErrorBody),
        (status = 500, description = "render failed", body = crate::error::ErrorBody)
    ))]
pub async fn get_card_display_image(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((prefix, id)): Path<(String, i64)>,
) -> Result<Response, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let card = db::get_card(&st.pool, prefix, id).await?;
    serve_artifact(
        &st,
        &headers,
        Some(card.display_front),
        &format!("blobs/{prefix}/{id}-front-display.png"),
        "image/png",
    )
}

/// Fetch a card's back display image (colour back, or the rendered black back).
#[utoipa::path(get, path = "/api/prefixes/{prefix}/cards/{id}/display-back", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    responses(
        (status = 200, description = "back display image bytes", content_type = "image/png"),
        (status = 404, body = crate::error::ErrorBody),
        (status = 423, description = "still rendering", body = crate::error::ErrorBody),
        (status = 500, description = "render failed", body = crate::error::ErrorBody)
    ))]
pub async fn get_card_display_back(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((prefix, id)): Path<(String, i64)>,
) -> Result<Response, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let card = db::get_card(&st.pool, prefix, id).await?;
    serve_artifact(
        &st,
        &headers,
        card.display_back,
        &format!("blobs/{prefix}/{id}-back-display.png"),
        "image/png",
    )
}

/// Fetch a card's front print raster (24-bit BMP at the printer DPI). The print
/// preview and the spooler both read these exact bytes.
#[utoipa::path(get, path = "/api/prefixes/{prefix}/cards/{id}/print-raster", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    responses(
        (status = 200, description = "print raster bytes", content_type = "image/bmp"),
        (status = 404, body = crate::error::ErrorBody),
        (status = 423, description = "still rendering", body = crate::error::ErrorBody),
        (status = 500, description = "render failed", body = crate::error::ErrorBody)
    ))]
pub async fn get_card_print_raster(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((prefix, id)): Path<(String, i64)>,
) -> Result<Response, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let card = db::get_card(&st.pool, prefix, id).await?;
    serve_artifact(
        &st,
        &headers,
        Some(card.print_front),
        &format!("blobs/{prefix}/{id}-front-print.bmp"),
        "image/bmp",
    )
}

/// Fetch a card's back print raster (24-bit colour or 1-bit black BMP).
#[utoipa::path(get, path = "/api/prefixes/{prefix}/cards/{id}/print-raster/back", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    responses(
        (status = 200, description = "back print raster bytes", content_type = "image/bmp"),
        (status = 404, body = crate::error::ErrorBody),
        (status = 423, description = "still rendering", body = crate::error::ErrorBody),
        (status = 500, description = "render failed", body = crate::error::ErrorBody)
    ))]
pub async fn get_card_print_raster_back(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((prefix, id)): Path<(String, i64)>,
) -> Result<Response, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let card = db::get_card(&st.pool, prefix, id).await?;
    serve_artifact(
        &st,
        &headers,
        card.print_back,
        &format!("blobs/{prefix}/{id}-back-print.bmp"),
        "image/bmp",
    )
}

/// Pull a single named file part out of a multipart body.
///
/// Used by the back-image upload, which takes exactly one file and no text
/// fields — the card's front, name and serial are already stored.
async fn read_file_part(mut mp: Multipart, part: &str) -> Result<(Vec<u8>, String), ApiError> {
    let mut found: Option<(Vec<u8>, String)> = None;
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|e| ApiError::BadRequest(format!("malformed multipart: {e}")))?
    {
        if field.name() == Some(part) {
            let ct = field
                .content_type()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "application/octet-stream".to_string());
            let bytes = field
                .bytes()
                .await
                .map_err(|e| ApiError::BadRequest(e.to_string()))?;
            found = Some((bytes.to_vec(), ct));
        }
    }
    let (bytes, ct) =
        found.ok_or_else(|| ApiError::BadRequest(format!("file field '{part}' is required")))?;
    if bytes.is_empty() {
        return Err(ApiError::BadRequest(format!("{part} is empty")));
    }
    Ok((bytes, ct))
}

/// Reject a back-image write that this prefix or card cannot take.
///
/// Split out from the handler so the two refusals that are not multipart
/// plumbing — the prefix is frozen, and the card does not exist — are testable
/// without building a multipart body. No ribbon is consulted: an `image` back is
/// legal on either one, and on a black panel it is simply rendered in black.
async fn ensure_back_image_writable(
    pool: &SqlitePool,
    prefix: Uuid,
    id: i64,
    admin: bool,
) -> Result<(), ApiError> {
    db::ensure_writable(pool, prefix, admin).await?;
    db::get_card(pool, prefix, id).await?; // 404 for an unknown card
    Ok(())
}

/// Upload or replace one card's back image, leaving the front alone.
///
/// This is how a card's back side is set when it is an uploaded picture: cards
/// are created from their front image (the dropzone uploads a folder at a
/// time), and the back is then added per card. Attaching the image also makes
/// the card's `back` [`BackSource::Image`] — see `db::set_card_back_image`.
///
/// The ribbon has no say in whether the upload is accepted: on a black panel the
/// picture is rendered to 1 bit rather than refused.
///
/// Re-uploading replaces the stored blob, whatever format the old one was in —
/// `write_blob` cleans the earlier file up, so there are no orphans.
#[utoipa::path(put, path = "/api/prefixes/{prefix}/cards/{id}/back-image", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    request_body(content = String, content_type = "multipart/form-data",
        description = "Multipart form carrying exactly one file part, `back_image`."),
    responses(
        (status = 200, body = Card),
        (status = 400, description = "the file part is missing or empty", body = crate::error::ErrorBody),
        (status = 404, description = "unknown prefix or card", body = crate::error::ErrorBody),
        (status = 409, description = "prefix is frozen", body = crate::error::ErrorBody)
    ))]
pub async fn put_card_back_image(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((prefix, id)): Path<(String, i64)>,
    mp: Multipart,
) -> Result<Json<Card>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let caller = auth::resolve(&st, &headers).await?;
    ensure_back_image_writable(&st.pool, prefix, id, caller.is_admin()).await?;
    let (bytes, ct) = read_file_part(mp, "back_image").await?;
    let rel = write_blob(
        &st.data_dir,
        prefix,
        &format!("{id}-back.{}", extension_for(&ct)),
        &bytes,
    )?;
    db::set_card_back_image(&st.pool, prefix, id, &rel, &ct).await?;
    Ok(Json(rerender_card(&st, prefix, id).await?))
}

/// Change a prefix's ribbon while it is still writable.
///
/// Always allowed, and always lossless: nothing about a card is tied to the
/// ribbon except *how it is rendered*, so the switch cannot strand a card. Every
/// back side is then re-rendered in the new depth — colour to black and back —
/// and the print queue reports the new ribbon on the next read, because there is
/// one ribbon and this is it. Whether the prefix has been paid for is irrelevant:
/// the charge is already taken and recorded, and which load goes in the printer
/// is a production decision, not a repricing.
#[utoipa::path(patch, path = "/api/prefixes/{prefix}", tag = "prefixes",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    request_body = PrefixRibbonPatch,
    responses(
        (status = 200, body = Prefix),
        (status = 400, body = crate::error::ErrorBody),
        (status = 404, body = crate::error::ErrorBody),
        (status = 409, description = "prefix is frozen", body = crate::error::ErrorBody)
    ))]
pub async fn patch_prefix_ribbon(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(prefix): Path<String>,
    Json(input): Json<PrefixRibbonPatch>,
) -> Result<Json<Prefix>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let caller = auth::resolve(&st, &headers).await?;
    db::ensure_writable(&st.pool, prefix, caller.is_admin()).await?;
    let current = db::get_prefix(&st.pool, prefix).await?;
    if current.ribbon == input.ribbon {
        return Ok(Json(current)); // idempotent: nothing to render or write
    }
    let updated = db::set_prefix_ribbon(&st.pool, prefix, input.ribbon).await?;
    // Every derived artifact comes from the stored originals, so this is a
    // re-render, never a data change: the cards keep their bytes and get a new
    // colour depth.
    for card in db::list_cards(&st.pool, prefix).await? {
        rerender_card(&st, prefix, card.id).await?;
    }
    Ok(Json(updated))
}

/// Delete a card (metadata + image).
#[utoipa::path(delete, path = "/api/prefixes/{prefix}/cards/{id}", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    responses((status = 204, description = "deleted"), (status = 404, body = crate::error::ErrorBody)))]
pub async fn delete_card(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((prefix, id)): Path<(String, i64)>,
) -> Result<StatusCode, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let caller = auth::resolve(&st, &headers).await?;
    db::ensure_writable(&st.pool, prefix, caller.is_admin()).await?;
    let card = db::get_card(&st.pool, prefix, id).await?;
    db::delete_card(&st.pool, prefix, id).await?;
    let rel = format!(
        "blobs/{}/{}.{}",
        prefix,
        id,
        extension_for(&card.image_content_type)
    );
    let _ = std::fs::remove_file(st.data_dir.join(&rel));
    if let Some(ct) = &card.back_image_content_type {
        let back_rel = format!("blobs/{prefix}/{id}-back.{}", extension_for(ct));
        let _ = std::fs::remove_file(st.data_dir.join(&back_rel));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a prefix and all of its cards (and their images).
///
/// A customer may delete only a *writable* prefix (draft, payment_pending, paid or
/// queued). The operator may delete one in any state, including while printing.
#[utoipa::path(delete, path = "/api/prefixes/{prefix}", tag = "prefixes",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    responses(
        (status = 204, description = "deleted"),
        (status = 404, body = crate::error::ErrorBody),
        (status = 409, description = "prefix is not deletable without the admin token", body = crate::error::ErrorBody)
    ))]
pub async fn delete_prefix(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(prefix): Path<String>,
) -> Result<StatusCode, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let caller = auth::resolve(&st, &headers).await?;
    let p = db::get_prefix(&st.pool, prefix).await?;
    if !caller.is_admin() && !p.status.is_customer_writable() {
        return Err(ApiError::Conflict(format!(
            "prefix {prefix} is '{}' and cannot be deleted; only a draft prefix can be",
            p.status
        )));
    }
    db::delete_prefix(&st.pool, prefix).await?;
    let _ = std::fs::remove_dir_all(st.data_dir.join("blobs").join(prefix.to_string()));
    Ok(StatusCode::NO_CONTENT)
}

/// Remove every card in a prefix (keeps the prefix itself).
#[utoipa::path(delete, path = "/api/prefixes/{prefix}/cards", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    responses(
        (status = 204, description = "all cards removed"),
        (status = 404, body = crate::error::ErrorBody),
        (status = 409, description = "prefix is frozen", body = crate::error::ErrorBody)
    ))]
pub async fn delete_cards(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(prefix): Path<String>,
) -> Result<StatusCode, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let caller = auth::resolve(&st, &headers).await?;
    db::ensure_writable(&st.pool, prefix, caller.is_admin()).await?;
    db::delete_all_cards(&st.pool, prefix).await?;
    let _ = std::fs::remove_dir_all(st.data_dir.join("blobs").join(prefix.to_string()));
    Ok(StatusCode::NO_CONTENT)
}

/// Update a card's metadata (name / serial / back text) without its image.
#[utoipa::path(patch, path = "/api/prefixes/{prefix}/cards/{id}", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    request_body = CardPatch,
    responses(
        (status = 200, body = Card),
        (status = 400, body = crate::error::ErrorBody),
        (status = 404, body = crate::error::ErrorBody),
        (status = 409, description = "prefix is frozen", body = crate::error::ErrorBody)
    ))]
pub async fn patch_card(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((prefix, id)): Path<(String, i64)>,
    Json(patch): Json<CardPatch>,
) -> Result<Json<Card>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let caller = auth::resolve(&st, &headers).await?;
    db::ensure_writable(&st.pool, prefix, caller.is_admin()).await?;
    db::update_card_meta(&st.pool, prefix, id, &patch).await?;
    Ok(Json(rerender_card(&st, prefix, id).await?))
}

/// The print queue for one prefix: its cards ordered by id, with the operator's
/// per-card print state. **Admin only** — this is operator state, kept out of
/// the customer view and the admin print preview.
///
/// `resume` is true when the prefix is `printing` and still has unprinted
/// cards, i.e. a half-finished order the spooler can pick up where it left off.
#[utoipa::path(get, path = "/api/prefixes/{prefix}/print-queue", tag = "cards",
        params(("prefix" = Uuid, Path, description = "prefix uuid")),
        security(("bearerAuth" = []), ("sessionCookie" = [])),
        responses(
            (status = 200, body = PrintQueue),
            (status = 401, description = "admin token required", body = crate::error::ErrorBody),
            (status = 404, body = crate::error::ErrorBody),
            (status = 503, description = "no admin token configured", body = crate::error::ErrorBody)
        ))]
pub async fn get_print_queue(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(prefix): Path<String>,
) -> Result<Json<PrintQueue>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let caller = auth::resolve(&st, &headers).await?;
    caller.require_admin()?;
    Ok(Json(db::print_queue(&st.pool, prefix).await?))
}

/// Mark one card printed. **Admin only.** The spooler calls this once the
/// printer confirms the card was ejected; it is idempotent, so a retried
/// delivery can never print a card twice.
#[utoipa::path(post, path = "/api/prefixes/{prefix}/cards/{id}/printed", tag = "cards",
        params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
        security(("bearerAuth" = []), ("sessionCookie" = [])),
        responses(
            (status = 200, body = Card),
            (status = 401, description = "admin token required", body = crate::error::ErrorBody),
            (status = 404, body = crate::error::ErrorBody),
            (status = 503, description = "no admin token configured", body = crate::error::ErrorBody)
        ))]
pub async fn mark_card_printed(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((prefix, id)): Path<(String, i64)>,
) -> Result<Json<Card>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let caller = auth::resolve(&st, &headers).await?;
    caller.require_admin()?;
    Ok(Json(db::mark_card_printed(&st.pool, prefix, id).await?))
}

/// Set a prefix's status. **Admin only.**
///
/// The operator may move a prefix to any status at any time, bypassing the
/// transition table — e.g. requeueing a `printing` order that never finished,
/// which a customer has no legal edge for.
#[utoipa::path(patch, path = "/api/prefixes/{prefix}/status", tag = "prefixes",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    request_body = StatusInput,
    security(("bearerAuth" = []), ("sessionCookie" = [])),
    responses(
        (status = 200, body = Prefix),
        (status = 401, description = "admin token required", body = crate::error::ErrorBody),
        (status = 404, body = crate::error::ErrorBody),
        (status = 503, description = "no admin token configured", body = crate::error::ErrorBody)
    ))]
pub async fn patch_status(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(prefix): Path<String>,
    Json(input): Json<StatusInput>,
) -> Result<Json<Prefix>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let caller = auth::resolve(&st, &headers).await?;
    caller.require_admin()?;
    Ok(Json(
        db::set_status(
            &st.pool,
            prefix,
            input.status,
            input.order_ref.as_deref(),
            caller.is_admin(),
        )
        .await?,
    ))
}

// ---------------------------------------------------------------------------
// Admin session — how the admin UI logs in
// ---------------------------------------------------------------------------

/// Should the session cookie carry `Secure`?
///
/// Off by default so local http development works; force it with
/// `SESSION_COOKIE_SECURE=true`, or let a reverse proxy advertise https via
/// `X-Forwarded-Proto`.
fn cookie_secure(st: &AppState, headers: &HeaderMap) -> bool {
    st.cookie_secure.unwrap_or_else(|| {
        headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("https"))
    })
}

/// Log the operator in: exchange the admin token for a session cookie.
///
/// The token travels once, in the body. From then on the browser carries an
/// `HttpOnly` cookie instead, so injected script cannot read the credential
/// back out of the page.
#[utoipa::path(post, path = "/api/admin/session", tag = "admin",
    request_body = AdminLogin,
    responses(
        (status = 204, description = "logged in; a session cookie is set"),
        (status = 401, description = "wrong token", body = crate::error::ErrorBody),
        (status = 503, description = "no admin token configured", body = crate::error::ErrorBody)
    ))]
pub async fn admin_login(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<AdminLogin>,
) -> Result<Response, ApiError> {
    let key = auth::client_ip(&headers);
    // Refuse before even comparing once this client is locked out.
    st.login_limiter.check(&key)?;
    if let Err(e) = auth::verify_token(&st, &input.token) {
        // Only a *wrong token* (401) counts against the key; a 503 (admin API
        // disabled) is not a failed guess and must not lock anyone out.
        if matches!(e, ApiError::Unauthorized(_)) {
            st.login_limiter.record_failure(&key);
        }
        return Err(e);
    }
    let id = st.sessions.create().await;
    let secure = cookie_secure(&st, &headers);
    let cookie = format!(
        "{name}={id}; Path=/api; HttpOnly; SameSite=Strict; Max-Age={ttl}{secure}",
        name = auth::SESSION_COOKIE,
        ttl = auth::SESSION_TTL.as_secs(),
        secure = if secure { "; Secure" } else { "" },
    );
    Ok(([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response())
}

/// Log out. Idempotent: always 204, whether or not a session existed.
#[utoipa::path(delete, path = "/api/admin/session", tag = "admin",
    responses((status = 204, description = "logged out")))]
pub async fn admin_logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(id) = auth::cookie_value(&headers, auth::SESSION_COOKIE) {
        st.sessions.revoke(&id).await;
    }
    let expired = format!(
        "{name}=; Path=/api; HttpOnly; SameSite=Strict; Max-Age=0",
        name = auth::SESSION_COOKIE,
    );
    ([(header::SET_COOKIE, expired)], StatusCode::NO_CONTENT).into_response()
}

/// Report whether the caller is a logged-in operator. The admin UI calls this
/// on load to choose between the login form and the dashboard.
#[utoipa::path(get, path = "/api/admin/session", tag = "admin",
    security(("bearerAuth" = []), ("sessionCookie" = [])),
    responses(
        (status = 200, body = AdminSession),
        (status = 401, description = "not logged in", body = crate::error::ErrorBody),
        (status = 503, description = "no admin token configured", body = crate::error::ErrorBody)
    ))]
pub async fn admin_session(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AdminSession>, ApiError> {
    auth::resolve(&st, &headers).await?.require_admin()?;
    Ok(Json(AdminSession {
        authenticated: true,
    }))
}

// ---------------------------------------------------------------------------
// Needs attention — payment dead ends where money and orders disagree
// ---------------------------------------------------------------------------

/// How long a `payment_pending` prefix may sit before the checkout counts as
/// abandoned and the prefix lands on the "needs attention" list.
///
/// Stripe Checkout sessions expire by default after 24 hours and fire
/// `checkout.session.expired`, which reverts a pending prefix to draft. So a
/// prefix still `payment_pending` after this window means the expiry never
/// reached us (or was missed) and the order is genuinely stuck — not a
/// customer who is still at the checkout in the last few minutes.
const ABANDONED_CHECKOUT_AFTER_SECS: i64 = 24 * 3600;

/// The admin "needs attention" list: prefixes where money and the order
/// disagree, each with an `action` saying what to do. **Admin only.**
///
/// Two sources, both queried:
///
///  * the `payment_anomalies` table, written at the moment a settled payment
///    could not attach to a prefix (cancelled, draft, deleted, already past
///    printing);
///  * a live query for `payment_pending` prefixes stuck past the checkout
///    window (abandoned, never paid), which need no event to be detectable.
///
/// This is deliberately a query — never a scrape of the log — so an operator
/// who looks at the list sees every unresolved dead end, and entries survive
/// restarts and the Stirpe retry lifecycle.
#[utoipa::path(get, path = "/api/admin/needs-attention", tag = "admin",
    security(("bearerAuth" = []), ("sessionCookie" = [])),
    responses(
        (status = 200, body = [NeedsAttentionItem]),
        (status = 401, description = "admin token required", body = crate::error::ErrorBody),
        (status = 503, description = "no admin token configured", body = crate::error::ErrorBody)
    ))]
pub async fn needs_attention(
    State(st): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<NeedsAttentionItem>>, ApiError> {
    auth::resolve(&st, &headers).await?.require_admin()?;

    let mut items: Vec<NeedsAttentionItem> = Vec::new();
    for a in db::list_payment_anomalies(&st.pool).await? {
        items.push(NeedsAttentionItem {
            prefix: a.prefix,
            kind: a.kind,
            action: a.action,
            message: a.message,
            at: a.created_at,
            session_id: a.session_id,
            amount_cents: a.amount_cents,
            currency: a.currency,
        });
    }
    for p in db::abandoned_payment_pending(&st.pool, ABANDONED_CHECKOUT_AFTER_SECS).await? {
        items.push(NeedsAttentionItem {
            prefix: p.uuid,
            kind: NeedsAttentionKind::AbandonedCheckout,
            action: "rebuild_by_hand".into(),
            message: "this prefix has sat awaiting payment past the checkout window; \
                      nothing was ever paid, so no work queue will pick it up — revert it to \
                      draft, or mark it paid by hand if the money arrived outside Stripe"
                .into(),
            at: p.status_changed_at.unwrap_or(p.created_at),
            session_id: None,
            amount_cents: None,
            currency: None,
        });
    }
    // Both sources carry the same ISO-ish timestamp, so a lexicographic sort
    // interleaves anomalies and abandoned prefixes correctly, newest first.
    items.sort_by(|a, b| b.at.cmp(&a.at));
    Ok(Json(items))
}

// ---------------------------------------------------------------------------
// Shop — Stripe Checkout, and the webhook that records the payment
// ---------------------------------------------------------------------------

/// Where Stripe should send the customer back to.
///
/// `PUBLIC_BASE_URL` wins. Without it we fall back to the request's own `Host`
/// (and `X-Forwarded-Proto`), which is correct behind a proxy but does mean an
/// unset deployment lets a client choose its own redirect target — hence the
/// startup warning. It is not a security hole worth losing sleep over (the
/// attacker needs the prefix uuid, and the redirect is only ever a `success_url`
/// on their own session), but set the variable.
fn public_base_url(st: &AppState, headers: &HeaderMap) -> String {
    if let Some(base) = st.stripe.as_ref().and_then(|s| s.public_base_url()) {
        return base.trim_end_matches('/').to_string();
    }
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| v == "https" || v == "http")
        .unwrap_or_else(|| "http".to_string());
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost");
    format!("{proto}://{host}")
}

/// Public shop settings: price per printed side, currency, ribbon availability,
/// and whether payment works at all. The customer UI needs this to show a total
/// — and which ribbons can actually be ordered — before sending anyone to
/// Stripe.
///
/// Each ribbon carries an `available` flag, and the two stock flags behind it
/// are served as well. An out-of-stock ribbon stays in the list rather than
/// disappearing, so the customer sees that the option exists and is out.
#[utoipa::path(get, path = "/api/shop/config", tag = "shop",
    responses((status = 200, body = ShopConfig)))]
pub async fn shop_config(State(st): State<AppState>) -> Result<Json<ShopConfig>, ApiError> {
    let prices = db::list_ribbon_prices(&st.pool).await?;
    let ribbons = db::list_ribbon_stock(&st.pool).await?;
    let body = match st.stripe.as_deref() {
        None => ShopConfig {
            enabled: false,
            currency: "eur".into(),
            prices,
            ribbons,
            shipping_cents: 0,
            countries: vec![],
        },
        Some(s) => {
            let p = s.pricing();
            ShopConfig {
                enabled: true,
                currency: p.currency.clone(),
                prices,
                ribbons,
                shipping_cents: p.shipping_cents,
                countries: p.countries.clone(),
            }
        }
    };
    Ok(Json(body))
}

/// Set one ribbon's stock flag. **Admin only.**
///
/// This is the operator's "the ribbon ran out" switch: flipping it takes every
/// mode that prints with that ribbon out of the shop, and the shop config
/// immediately reports those modes as unavailable. YMCKO covers both
/// single-sided and double-sided colour, so one flag moves both.
#[utoipa::path(put, path = "/api/shop/ribbons/{ribbon}", tag = "shop",
    params(("ribbon" = String, Path, description = "ribbon: ymcko or ymckoko")),
    request_body = RibbonStockPatch,
    security(("bearerAuth" = []), ("sessionCookie" = [])),
    responses(
        (status = 200, body = RibbonStock),
        (status = 400, description = "unknown ribbon", body = crate::error::ErrorBody),
        (status = 401, description = "admin token required", body = crate::error::ErrorBody),
        (status = 503, description = "no admin token configured", body = crate::error::ErrorBody)
    ))]
pub async fn put_ribbon_stock(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(ribbon): Path<String>,
    Json(input): Json<RibbonStockPatch>,
) -> Result<Json<RibbonStock>, ApiError> {
    auth::resolve(&st, &headers).await?.require_admin()?;
    let ribbon = Ribbon::parse(&ribbon)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown ribbon: {ribbon}")))?;
    tracing::info!(%ribbon, in_stock = input.in_stock, "ribbon stock flag set");
    Ok(Json(db::set_ribbon_stock(&st.pool, ribbon, input.in_stock).await?))
}

/// The most one billing unit may be priced at, in the currency's minor unit.
///
/// A guard against the euro/cents mix-up, not a pricing policy: `10.00` typed as
/// cents is 10, but `1000` meant as cents while thinking in euros is 10.00, and
/// the failure that actually costs money is the reverse — a price two orders of
/// magnitude too high, charged silently. 100 000 minor units is 1000.00 in any
/// currency with a two-digit minor unit, well above any card label.
const MAX_PRICE_CENTS: i64 = 100_000;

/// Set one ribbon's price. **Admin only.**
///
/// The unit is the ribbon's (`side` or `card`), never part of the request: the
/// stored figure is always the price of one unit, and
/// [`crate::models::Ribbon::billed_units`] is what multiplies it at the
/// checkout. A price change affects sessions opened *after* it — one already
/// open was priced when it was created, and the operator changing the price
/// does not re-price it.
#[utoipa::path(put, path = "/api/shop/ribbons/{ribbon}/price", tag = "shop",
    params(("ribbon" = String, Path, description = "ribbon: ymcko or ymckoko")),
    request_body = RibbonPricePatch,
    security(("bearerAuth" = []), ("sessionCookie" = [])),
    responses(
        (status = 200, body = RibbonPrice),
        (status = 400, description = "unknown ribbon, or a price outside the allowed range", body = crate::error::ErrorBody),
        (status = 401, description = "admin token required", body = crate::error::ErrorBody),
        (status = 503, description = "no admin token configured", body = crate::error::ErrorBody)
    ))]
pub async fn put_ribbon_price(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(ribbon): Path<String>,
    Json(input): Json<RibbonPricePatch>,
) -> Result<Json<RibbonPrice>, ApiError> {
    auth::resolve(&st, &headers).await?.require_admin()?;
    let ribbon = Ribbon::parse(&ribbon)
        .ok_or_else(|| ApiError::BadRequest(format!("unknown ribbon: {ribbon}")))?;
    if input.price_cents < 0 || input.price_cents > MAX_PRICE_CENTS {
        return Err(ApiError::BadRequest(format!(
            "price must be between 0 and {MAX_PRICE_CENTS} minor units; got {}. \
             (The figure is in cents/öre, not euros.)",
            input.price_cents
        )));
    }
    tracing::info!(%ribbon, price_cents = input.price_cents, "ribbon price set");
    Ok(Json(
        db::set_ribbon_price(&st.pool, ribbon, input.price_cents).await?,
    ))
}

/// Refuse to sell a job that cannot be printed.
///
/// A ribbon that is out of stock is not orderable, however the customer got to
/// it: a page rendered before the ribbon ran out, or a direct API call. The UI
/// already renders such a ribbon disabled — this is the same rule at the layer
/// that takes the money, which is where it has to hold.
///
/// What it deliberately does *not* do is reconcile anything: a prefix already
/// priced for a ribbon that has since run out is neither repriced, refunded nor
/// flagged — it simply cannot open a *new* session until the ribbon is back.
/// Sorting those out is a background, by-hand matter, and the gate is kept to
/// the one question it can answer cleanly: may this be sold right now.
async fn ensure_ribbon_orderable(pool: &SqlitePool, ribbon: Ribbon) -> Result<(), ApiError> {
    if !db::ribbon_in_stock(pool, ribbon).await? {
        return Err(ApiError::Conflict(format!(
            "the {ribbon} ribbon is out of stock, so nothing can be ordered for it right now"
        )));
    }
    Ok(())
}

/// Refuse an order that is missing part of a card, so nothing is sold that
/// cannot be printed.
///
/// A card's back can be an uploaded image or typed markdown, either of which is
/// filled in *after* the card exists (the dropzone uploads fronts a folder at a
/// time). A prefix can therefore hold cards whose back is still missing, and
/// nothing downstream would notice until the spooler went looking for the
/// raster. Same place and same reasoning as the ribbon stock check — the layer
/// that takes the money is where "can this actually be made" has to hold.
///
/// Like `ensure_ribbon_orderable`, it reconciles nothing: a prefix already
/// priced is not repriced or refunded, it just cannot open a *new* session
/// until the missing back is supplied.
async fn ensure_order_printable(pool: &SqlitePool, prefix: Uuid) -> Result<(), ApiError> {
    let missing = db::cards_missing_back(pool, prefix).await?;
    if missing > 0 {
        return Err(ApiError::Conflict(format!(
            "prefix {prefix} has {missing} card(s) whose back is not finished; every card needs \
             its back image or back text before the order can be printed"
        )));
    }
    Ok(())
}

/// Start a Stripe Checkout session for a prefix and return where to send the
/// browser.
///
/// Caller: anyone who knows the prefix uuid — the same capability that governs
/// every other customer action. There is no submit step: paying is what makes an
/// order real, so a `draft` goes straight to `payment_pending` here. Nothing here
/// marks the prefix paid — that happens when Stripe confirms the money, via the
/// webhook.
///
/// Rate-limited per prefix: opening a Checkout session costs a Stripe round
/// trip, so a loop on one prefix is throttled to a handful per minute. A brand
/// new prefix can still be paid for whenever, so a customer's own re-opens are
/// never in the way — this bounds the Stripe traffic from a hammering caller,
/// which is all it is for.
#[utoipa::path(post, path = "/api/prefixes/{prefix}/checkout", tag = "shop",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    responses(
        (status = 200, body = stripe::CheckoutSession),
        (status = 429, description = "too many checkout sessions for this prefix in the last minute", body = crate::error::ErrorBody),
        (status = 404, description = "unknown prefix", body = crate::error::ErrorBody),
        (status = 409, description = "the prefix has no cards, is already past payment, its print mode's ribbon is out of stock, or a card is missing its back side", body = crate::error::ErrorBody),
        (status = 503, description = "payment is not configured on this deployment", body = crate::error::ErrorBody),
        (status = 502, description = "Stripe refused or could not be reached", body = crate::error::ErrorBody)
    ))]
pub async fn create_checkout(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(prefix): Path<String>,
) -> Result<Json<stripe::CheckoutSession>, ApiError> {
    let prefix = parse_uuid(&prefix)?;

    // Throttle before doing any work: created sessions cost a Stripe round
    // trip, and each one also flips the prefix out of draft, so re-opening
    // beyond the limit is either a loop or a broken customer flow — both should
    // slow down.
    if !st.checkout_limiter.allow(&prefix.to_string()) {
        return Err(ApiError::TooManyRequests(format!(
            "too many checkout sessions for prefix {prefix}; try again in a minute"
        )));
    }

    let stripe = st.stripe.as_deref().ok_or_else(|| {
        ApiError::Unavailable("payment is disabled: STRIPE_SECRET_KEY is not set".into())
    })?;

    let p = db::get_prefix(&st.pool, prefix).await?;
    // Re-opening a checkout is fine — the customer may have closed the window, or
    // had a card declined. Anything at or past payment is not.
    if !matches!(p.status, PrefixStatus::Draft | PrefixStatus::PaymentPending) {
        return Err(ApiError::Conflict(format!(
            "prefix {prefix} is '{}': only a draft or payment_pending prefix can be paid for",
            p.status
        )));
    }
    if p.card_count <= 0 {
        return Err(ApiError::Conflict(format!(
            "prefix {prefix} has no cards, so there is nothing to print"
        )));
    }

    ensure_ribbon_orderable(&st.pool, p.ribbon).await?;
    // A card is only complete once its back side is: an uploaded image, or the
    // text it is rendered from. A card without its material would reach the
    // spooler as a front with a hole where its back should be.
    ensure_order_printable(&st.pool, prefix).await?;

    // The price is per billing *unit*, looked up from the database — not a
    // single environment-wide figure, and not the same unit on both ribbons. A
    // ribbon with no configured price is a misconfiguration and refuses the
    // checkout rather than pricing at zero.
    let price_cents = db::ribbon_price(&st.pool, p.ribbon).await?;
    // How many units this order actually costs, which is what is charged: one
    // per printed side on YMCKO, one per card whatever its back is on YMCKOKO —
    // the two ribbons price the same order differently.
    let units = db::billed_units(&st.pool, prefix, p.ribbon).await?;

    // Flip to payment_pending BEFORE opening the session, and note the order: a
    // customer can pay the moment they land on Stripe's page, and the webhook
    // deliberately refuses to force a payment through for a prefix that still
    // reads as a draft. Writing the status second would let a fast payment lose
    // that race and strand real money in a WARN line.
    let was_draft = p.status == PrefixStatus::Draft;
    if was_draft {
        db::set_status(&st.pool, prefix, PrefixStatus::PaymentPending, None, false).await?;
    }

    let base = public_base_url(&st, &headers);
    let session = match stripe
        .create_checkout_session(prefix, units, price_cents, p.ribbon.billing_unit(), &base)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            // No session was opened, so nothing should read as awaiting payment.
            if was_draft {
                if let Err(back) =
                    db::set_status(&st.pool, prefix, PrefixStatus::Draft, None, false).await
                {
                    tracing::error!(%prefix, error = %back,
                        "could not put the prefix back to draft after a failed checkout");
                }
            }
            return Err(e);
        }
    };

    tracing::info!(%prefix, session = %session.id, cards = p.card_count, units,
        ribbon = %p.ribbon, "checkout session created; waiting for the payment webhook");
    Ok(Json(session))
}

/// Stripe's webhook. This is the only thing that marks an order paid.
///
/// Authenticated by the `Stripe-Signature` header over the raw body — there is
/// no token and no cookie involved, so the CSRF middleware lets it through (it
/// only speaks up when the session cookie is present). Every answer is 2xx
/// except a bad signature, because Stripe retries anything else for days and a
/// permanent, deliberate no-op is not worth that.
#[utoipa::path(post, path = "/api/webhooks/stripe", tag = "shop",
    request_body(content = String, content_type = "application/json",
        description = "Raw Stripe event. Verified by the Stripe-Signature header, not by a token."),
    responses(
        (status = 200, description = "accepted, including events deliberately ignored"),
        (status = 400, description = "missing, stale or mismatching signature", body = crate::error::ErrorBody),
        (status = 503, description = "payment is not configured on this deployment", body = crate::error::ErrorBody)
    ))]
pub async fn stripe_webhook(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let stripe = st.stripe.as_deref().ok_or_else(|| {
        ApiError::Unavailable("payment is disabled: STRIPE_SECRET_KEY is not set".into())
    })?;

    let signature = headers
        .get("stripe-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::BadRequest("missing Stripe-Signature header".into()))?;
    stripe.verify_webhook(signature, &body)?;

    let event: stripe::Event = serde_json::from_slice(&body)
        .map_err(|e| ApiError::BadRequest(format!("malformed Stripe event: {e}")))?;

    match event.kind.as_str() {
        "checkout.session.completed" | "checkout.session.async_payment_succeeded" => {
            // The email notifications are sent from inside `record_payment`,
            // after the status write and outside its error path: a mail that
            // cannot be delivered must never turn into a webhook Stripe retries
            // for days, or into a paid order that never gets recorded.
            record_payment(
                &st.pool,
                st.notifier.as_deref(),
                &public_base_url(&st, &headers),
                &event,
            )
            .await?;
        }
        "checkout.session.expired" => {
            revert_expired_checkout(&st.pool, &event.data.object).await?;
        }
        other => {
            tracing::debug!(event = other, id = %event.id, "ignoring Stripe event type");
        }
    }
    Ok(StatusCode::OK)
}

/// Attach a settled payment to the prefix it was made for.
///
/// Idempotent on purpose: Stripe retries, and a customer refreshing the success
/// URL does not produce a second event, but a duplicated delivery is normal
/// enough that it must be harmless.
///
/// `notifier` is `None` when email is not configured, in which case nothing is
/// sent and nothing changes. When it is set, the paid confirmation goes out from
/// the transition branch below — after the status write, and with every failure
/// swallowed there, so the webhook's own health never depends on Resend.
async fn record_payment(
    pool: &SqlitePool,
    notifier: Option<&notify::Notifier>,
    base_url: &str,
    event: &stripe::Event,
) -> Result<(), ApiError> {
    let session = &event.data.object;

    // Klarna and SEPA report `completed` before the money actually moves; the
    // paired `async_payment_succeeded` is the one that means settled. Waiting
    // for payment_status == "paid" covers both event types in one rule.
    if session.payment_status.as_deref() != Some("paid") {
        tracing::debug!(session = %session.id, status = ?session.payment_status,
            "payment not settled yet; a later event will carry it");
        return Ok(());
    }

    let Some(raw) = session.prefix_uuid() else {
        tracing::error!(session = %session.id, event = %event.id, livemode = ?event.livemode,
            "Stripe event carries no prefix reference; cannot attribute the payment");
        return Ok(());
    };
    let Ok(prefix) = Uuid::parse_str(&raw) else {
        tracing::error!(session = %session.id, reference = %raw,
            "Stripe event's prefix reference is not a uuid");
        return Ok(());
    };

    let current = match db::get_prefix(pool, prefix).await {
        Ok(p) => p,
        Err(ApiError::NotFound(_)) => {
            // Paid, then deleted. The money is real and there is nowhere to put
            // it, so this needs a human — record it for the needs-attention
            // list (this survives the deleted prefix row), shout, and let it
            // be 200 so Stripe stops retrying an event that can never succeed.
            let message = "a settled payment for a prefix that has since been deleted; \
                           the money is real but there is nowhere to put it"
                .to_string();
            db::insert_payment_anomaly(
                pool,
                prefix,
                NeedsAttentionKind::PaymentForDeleted,
                &message,
                "refund_or_rebuild_by_hand",
                Some(&session.id),
                session.amount_total,
                session.currency.as_deref(),
            )
            .await?;
            tracing::error!(%prefix, session = %session.id, amount = ?session.amount_total,
                "payment received for a prefix that no longer exists: refund it, or rebuild \
                 the order by hand");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    match current.status {
        PrefixStatus::PaymentPending => {
            let updated =
                db::set_status(pool, prefix, PrefixStatus::Paid, Some(&session.id), false).await?;
            tracing::info!(%prefix, session = %session.id, amount = ?session.amount_total,
                currency = ?session.currency, "payment recorded, prefix is now {}", updated.status);

            // Tell the customer, and the operator, that this order is real.
            //
            // Deliberately here and not earlier: the status write above has
            // already happened, so the order is recorded whatever becomes of the
            // mail, and `order_paid` never fails its caller. Both messages are
            // claimed under the Stripe session id, so Stripe retrying this very
            // delivery cannot produce a second confirmation.
            if let Some(notifier) = notifier {
                notify::order_paid(
                    pool,
                    notifier,
                    &notify::PaidOrder {
                        prefix,
                        base_url: base_url.to_string(),
                        session_id: session.id.clone(),
                        card_count: updated.card_count,
                        amount_cents: session.amount_total,
                        currency: session.currency.clone(),
                        // The ribbon this order prints with: the prefix's own,
                        // which is what the printer is fed.
                        ribbon: Some(updated.ribbon),
                        customer_email: session.payer_email().map(str::to_string),
                        customer_name: session.payer_name().map(str::to_string),
                    },
                )
                .await;
            }
        }
        // Already handled — the normal case for a retried delivery of a prefix
        // still in flight (paid or queued). Nothing to do.
        PrefixStatus::Paid | PrefixStatus::Queued => {
            tracing::info!(%prefix, session = %session.id, status = %current.status,
                "payment already recorded; ignoring the duplicate");
        }
        // Money for an order that is already past printing. It must have been
        // paid to reach printing/printed, so a new settlement is most likely a
        // duplicate charge — flag it for a human to confirm rather than
        // silently keeping the extra money.
        PrefixStatus::Printing | PrefixStatus::Printed => {
            let message = format!(
                "a settled payment for a prefix that is already past printing \
                 ({status}); the order was already paid to get here, so this is \
                 likely a duplicate charge",
                status = current.status
            );
            db::insert_payment_anomaly(
                pool,
                prefix,
                NeedsAttentionKind::PaymentAfterPrinting,
                &message,
                "verify_then_refund_if_duplicate",
                Some(&session.id),
                session.amount_total,
                session.currency.as_deref(),
            )
            .await?;
            tracing::warn!(%prefix, session = %session.id, status = %current.status,
                "payment settled for a prefix already past printing; recorded for review");
        }
        // A draft (nothing in flight) or a cancelled order. Deliberately not
        // forced: whether to refund or to honour the order is the operator's
        // call, and guessing wrong either prints an order nobody wants or
        // silently keeps money. Recorded so that call is visible, not buried
        // in a log line.
        PrefixStatus::Draft => {
            let message = "a settled payment for a prefix that still reads as a draft; \
                           nothing was awaiting it"
                .to_string();
            db::insert_payment_anomaly(
                pool,
                prefix,
                NeedsAttentionKind::PaymentForDraft,
                &message,
                "refund_or_honour",
                Some(&session.id),
                session.amount_total,
                session.currency.as_deref(),
            )
            .await?;
            tracing::warn!(%prefix, session = %session.id, status = %current.status,
                "payment settled for a draft prefix; recorded for review");
        }
        PrefixStatus::Cancelled => {
            let message = "a settled payment for a prefix that was cancelled; if the \
                           cancellation was communicated, the money probably needs to go back"
                .to_string();
            db::insert_payment_anomaly(
                pool,
                prefix,
                NeedsAttentionKind::PaymentForCancelled,
                &message,
                "refund_or_honour",
                Some(&session.id),
                session.amount_total,
                session.currency.as_deref(),
            )
            .await?;
            tracing::warn!(%prefix, session = %session.id, status = %current.status,
                "payment settled for a cancelled prefix; recorded for review");
        }
    }
    Ok(())
}

/// Revert a Checkout session that expired without payment back to `draft`.
///
/// `checkout.session.expired` fires when a session times out (the customer
/// abandoned it, or a card was declined) — nothing was paid, so the prefix
/// should stop reading as `payment_pending` and give the customer back their
/// write window instead of stranding a half-open order. Idempotent like
/// `record_payment`: a retried delivery finds a draft already and is a no-op.
///
/// Deliberately narrow: only a `payment_pending` prefix is reverted. A prefix
/// already at `draft` is a duplicated delivery; anything past `payment_pending`
/// means a payment landed despite this expiry, and un-sealing that is a human's
/// call, not a webhook's.
async fn revert_expired_checkout(
    pool: &SqlitePool,
    session: &stripe::SessionObject,
) -> Result<(), ApiError> {
    let Some(raw) = session.prefix_uuid() else {
        tracing::warn!(session = %session.id,
            "expired session carries no prefix reference; nothing to revert");
        return Ok(());
    };
    let Ok(prefix) = Uuid::parse_str(&raw) else {
        tracing::warn!(session = %session.id, reference = %raw,
            "expired session's prefix reference is not a uuid");
        return Ok(());
    };

    let current = match db::get_prefix(pool, prefix).await {
        Ok(p) => p,
        Err(ApiError::NotFound(_)) => {
            // Deleted meanwhile — nothing left to un-seal, and the customer is
            // not coming back to it. Accept and move on.
            tracing::debug!(%prefix, session = %session.id,
                "expired session for a prefix that no longer exists");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    match current.status {
        PrefixStatus::PaymentPending => {
            let updated = db::set_status(pool, prefix, PrefixStatus::Draft, None, false).await?;
            tracing::info!(%prefix, session = %session.id,
                "checkout expired without payment; prefix reverted to {}", updated.status);
        }
        // Already a draft — a duplicated delivery of the same expired event.
        PrefixStatus::Draft => {
            tracing::debug!(%prefix, session = %session.id,
                "checkout already reverted to draft; ignoring the duplicate");
        }
        // A payment landed despite the expiry, or the status moved on. Not our
        // call to un-seal: leave it and flag it for a human.
        other => {
            tracing::warn!(%prefix, session = %session.id, status = %other,
                "expired session met a prefix that is '{}' — past payment_pending, so left alone \
                 (a payment may have landed; check in the admin UI)", other);
        }
    }
    Ok(())
}

/// The `submit` step is gone: paying is what makes an order real, so the
/// checkout call moves a draft straight to `payment_pending`.
///
/// This tombstone answers a stale client explicitly — an unmatched POST would
/// otherwise fall through to the static-file fallback and come back as a bare
/// `405`, which reads like a bug rather than a deliberate removal.
pub async fn submit_prefix_gone() -> Response {
    (
        StatusCode::GONE,
        Json(crate::error::ErrorBody {
            error: "the submit step was removed: POST /api/prefixes/{prefix}/checkout now moves \
                    a draft straight to payment_pending"
                .to_string(),
        }),
    )
        .into_response()
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/api/prefixes",
            axum::routing::get(list_prefixes).post(create_prefix),
        )
        .route(
            "/api/prefixes/:prefix",
            axum::routing::get(get_prefix)
                .put(put_prefix)
                .delete(delete_prefix)
                .patch(patch_prefix_ribbon),
        )
        // Kept as an explicit tombstone rather than deleted, so a stale client
        // gets a 410 with a reason instead of a 405 from the static fallback.
        .route(
            "/api/prefixes/:prefix/submit",
            axum::routing::post(submit_prefix_gone),
        )
        .route(
            "/api/prefixes/:prefix/status",
            axum::routing::patch(patch_status),
        )
        .route(
            "/api/prefixes/:prefix/cards",
            axum::routing::get(list_cards)
                .post(post_card)
                .delete(delete_cards),
        )
        .route(
            "/api/prefixes/:prefix/cards/:id",
            axum::routing::get(get_card)
                .put(put_card)
                .patch(patch_card)
                .delete(delete_card),
        )
        .route(
            "/api/prefixes/:prefix/cards/:id/image",
            axum::routing::get(get_card_image),
        )
        .route(
            "/api/prefixes/:prefix/cards/:id/back-image",
            axum::routing::get(get_card_back_image).put(put_card_back_image),
        )
        .route(
            "/api/prefixes/:prefix/cards/:id/display-image",
            axum::routing::get(get_card_display_image),
        )
        .route(
            "/api/prefixes/:prefix/cards/:id/display-back",
            axum::routing::get(get_card_display_back),
        )
        .route(
            "/api/prefixes/:prefix/cards/:id/print-raster",
            axum::routing::get(get_card_print_raster),
        )
        .route(
            "/api/prefixes/:prefix/cards/:id/print-raster/back",
            axum::routing::get(get_card_print_raster_back),
        )
        .route(
            "/api/prefixes/:prefix/cards/:id/printed",
            axum::routing::post(mark_card_printed),
        )
        .route(
            "/api/prefixes/:prefix/print-queue",
            axum::routing::get(get_print_queue),
        )
        .route(
            "/api/admin/session",
            axum::routing::post(admin_login)
                .get(admin_session)
                .delete(admin_logout),
        )
        .route(
            "/api/admin/needs-attention",
            axum::routing::get(needs_attention),
        )
        .route("/api/shop/config", axum::routing::get(shop_config))
        .route(
            "/api/shop/ribbons/:ribbon/price",
            axum::routing::put(put_ribbon_price),
        )
        .route(
            "/api/shop/ribbons/:ribbon",
            axum::routing::put(put_ribbon_stock),
        )
        .route(
            "/api/prefixes/:prefix/checkout",
            axum::routing::post(create_checkout),
        )
        // Stripe's callback. Behind the CSRF layer like everything else, which
        // is harmless: that guard only speaks up when the session cookie is
        // present, and Stripe sends no cookie.
        .route("/api/webhooks/stripe", axum::routing::post(stripe_webhook))
        // Second CSRF lock, on top of SameSite=Strict: a cookie-authenticated
        // mutation must also carry the custom header. Applied to the API router
        // only, so the SPA and Swagger UI are untouched.
        .layer(axum::middleware::from_fn(auth::csrf_guard))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::RenderStatus;
    use crate::rate_limit;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::sync::Arc;
    use std::time::Duration;

    /// A fresh in-memory database with all migrations applied.
    async fn test_pool() -> SqlitePool {
        let opts = SqliteConnectOptions::new()
            .in_memory(true)
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .expect("in-memory sqlite pool");
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .expect("migrations applied to the test pool");
        pool
    }

    /// An expired-session webhook payload keyed to a prefix (or none).
    fn expired_session(prefix: Option<&str>) -> stripe::SessionObject {
        stripe::SessionObject {
            id: "cs_expired".into(),
            client_reference_id: prefix.map(str::to_string),
            payment_status: None,
            amount_total: None,
            currency: None,
            customer_details: None,
            customer_email: None,
            metadata: None,
        }
    }

    /// A settled `checkout.session.completed` event for prefix `prefix`, the
    /// shape the payment webhook acts on.
    fn payment_event(prefix: Option<&str>) -> stripe::Event {
        stripe::Event {
            id: "evt_paid".into(),
            kind: "checkout.session.completed".into(),
            livemode: None,
            data: stripe::EventData {
                object: stripe::SessionObject {
                    id: "cs_paid".into(),
                    client_reference_id: prefix.map(str::to_string),
                    payment_status: Some("paid".into()),
                    amount_total: Some(1750),
                    currency: Some("eur".into()),
                    customer_details: Some(stripe::CustomerDetails {
                        email: Some("ada@example.test".into()),
                        name: Some("Ada Lovelace".into()),
                    }),
                    customer_email: None,
                    metadata: None,
                },
            },
        }
    }

    /// The public origin the tests hand to the notification path.
    const TEST_BASE: &str = "https://cards.example.test";

    async fn status_of(pool: &SqlitePool, uuid: Uuid) -> PrefixStatus {
        db::get_prefix(pool, uuid).await.unwrap().status
    }

    /// A minimal card: a front image, plus the material for its back when the
    /// back source needs any. The blob paths are never read by these tests,
    /// only the metadata.
    async fn insert_card(pool: &SqlitePool, prefix: Uuid, id: i64, back: BackSource) {
        let (image, text) = match back {
            BackSource::Image => (Some("blobs/x-back.png"), None),
            BackSource::Markdown => (None, Some("# Songs\n")),
            _ => (None, None),
        };
        db::upsert_card(
            pool,
            prefix,
            id,
            "c",
            None,
            back,
            text,
            "blobs/f.png",
            "image/png",
            image,
            image.map(|_| "image/png"),
        )
        .await
        .expect("upsert card");
    }

    #[tokio::test]
    async fn expired_checkout_reverts_payment_pending_to_draft() {
        let pool = test_pool().await;
        let p = db::create_prefix(&pool, None, None).await.unwrap();
        // Mirror what /checkout does: a draft opens a session, flipping to
        // payment_pending.
        db::set_status(&pool, p.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .unwrap();
        assert_eq!(status_of(&pool, p.uuid).await, PrefixStatus::PaymentPending);

        revert_expired_checkout(&pool, &expired_session(Some(&p.uuid.to_string())))
            .await
            .unwrap();
        assert_eq!(status_of(&pool, p.uuid).await, PrefixStatus::Draft);
    }

    #[tokio::test]
    async fn replayed_expired_event_leaves_draft_alone() {
        let pool = test_pool().await;
        let p = db::create_prefix(&pool, None, None).await.unwrap(); // already draft

        revert_expired_checkout(&pool, &expired_session(Some(&p.uuid.to_string())))
            .await
            .unwrap();
        assert_eq!(status_of(&pool, p.uuid).await, PrefixStatus::Draft);
    }

    #[tokio::test]
    async fn expired_event_does_not_unseal_a_paid_prefix() {
        let pool = test_pool().await;
        let p = db::create_prefix(&pool, None, None).await.unwrap();
        db::set_status(&pool, p.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .unwrap();
        db::set_status(&pool, p.uuid, PrefixStatus::Paid, Some("cs_1"), false)
            .await
            .unwrap();

        // A later, out-of-order expired event must not silently reopen an order
        // whose money already landed.
        revert_expired_checkout(&pool, &expired_session(Some(&p.uuid.to_string())))
            .await
            .unwrap();
        assert_eq!(status_of(&pool, p.uuid).await, PrefixStatus::Paid);
    }

    #[tokio::test]
    async fn expired_event_without_a_prefix_is_a_harmless_noop() {
        let pool = test_pool().await;
        // Missing reference, and a junk reference — neither should error nor
        // touch anything.
        revert_expired_checkout(&pool, &expired_session(None))
            .await
            .unwrap();
        revert_expired_checkout(&pool, &expired_session(Some("not-a-uuid")))
            .await
            .unwrap();
        // And a session for a prefix that never existed is a no-op too.
        revert_expired_checkout(&pool, &expired_session(Some(&Uuid::new_v4().to_string())))
            .await
            .unwrap();
    }

    async fn anomaly_kinds(pool: &SqlitePool) -> Vec<NeedsAttentionKind> {
        db::list_payment_anomalies(pool)
            .await
            .unwrap()
            .into_iter()
            .map(|a| a.kind)
            .collect()
    }

    #[tokio::test]
    async fn settled_payment_marks_a_pending_prefix_paid_without_an_anomaly() {
        let pool = test_pool().await;
        let p = db::create_prefix(&pool, None, None).await.unwrap();
        db::set_status(&pool, p.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .unwrap();

        record_payment(
            &pool,
            None,
            TEST_BASE,
            &payment_event(Some(&p.uuid.to_string())),
        )
        .await
        .unwrap();
        assert_eq!(status_of(&pool, p.uuid).await, PrefixStatus::Paid);
        assert!(anomaly_kinds(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn replayed_payment_for_a_paid_prefix_is_a_noop() {
        let pool = test_pool().await;
        let p = db::create_prefix(&pool, None, None).await.unwrap();
        db::set_status(&pool, p.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .unwrap();
        db::set_status(&pool, p.uuid, PrefixStatus::Paid, Some("cs_1"), false)
            .await
            .unwrap();

        // A retried delivery of the same settled event finds the prefix already
        // paid (or queued) — harmless, and not a dead end.
        record_payment(
            &pool,
            None,
            TEST_BASE,
            &payment_event(Some(&p.uuid.to_string())),
        )
        .await
        .unwrap();
        assert_eq!(status_of(&pool, p.uuid).await, PrefixStatus::Paid);
        assert!(anomaly_kinds(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn settled_payment_for_a_cancelled_prefix_is_recorded_and_left_alone() {
        let pool = test_pool().await;
        let p = db::create_prefix(&pool, None, None).await.unwrap();
        db::set_status(&pool, p.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .unwrap();
        db::set_status(&pool, p.uuid, PrefixStatus::Cancelled, None, false)
            .await
            .unwrap();

        record_payment(
            &pool,
            None,
            TEST_BASE,
            &payment_event(Some(&p.uuid.to_string())),
        )
        .await
        .unwrap();
        // The order stays cancelled; the money is flagged, not spent.
        assert_eq!(status_of(&pool, p.uuid).await, PrefixStatus::Cancelled);
        assert_eq!(
            anomaly_kinds(&pool).await,
            vec![NeedsAttentionKind::PaymentForCancelled]
        );
    }

    #[tokio::test]
    async fn settled_payment_for_a_draft_prefix_is_recorded_and_left_alone() {
        let pool = test_pool().await;
        let p = db::create_prefix(&pool, None, None).await.unwrap(); // draft

        record_payment(
            &pool,
            None,
            TEST_BASE,
            &payment_event(Some(&p.uuid.to_string())),
        )
        .await
        .unwrap();
        assert_eq!(status_of(&pool, p.uuid).await, PrefixStatus::Draft);
        assert_eq!(
            anomaly_kinds(&pool).await,
            vec![NeedsAttentionKind::PaymentForDraft]
        );
    }

    #[tokio::test]
    async fn settled_payment_for_a_deleted_prefix_is_recorded() {
        let pool = test_pool().await;
        let p = db::create_prefix(&pool, None, None).await.unwrap();
        db::delete_prefix(&pool, p.uuid).await.unwrap();

        record_payment(
            &pool,
            None,
            TEST_BASE,
            &payment_event(Some(&p.uuid.to_string())),
        )
        .await
        .unwrap();
        // The prefix row is gone, but the anomaly survives with its uuid — this
        // is exactly the case the table exists for.
        assert_eq!(
            anomaly_kinds(&pool).await,
            vec![NeedsAttentionKind::PaymentForDeleted]
        );
    }

    #[tokio::test]
    async fn settled_payment_for_a_prefix_past_printing_is_recorded() {
        let pool = test_pool().await;
        let p = db::create_prefix(&pool, None, None).await.unwrap();
        // Force to printing (as the operator would requeue a half-finished job).
        db::set_status(&pool, p.uuid, PrefixStatus::Printing, None, true)
            .await
            .unwrap();

        record_payment(
            &pool,
            None,
            TEST_BASE,
            &payment_event(Some(&p.uuid.to_string())),
        )
        .await
        .unwrap();
        assert_eq!(status_of(&pool, p.uuid).await, PrefixStatus::Printing);
        assert_eq!(
            anomaly_kinds(&pool).await,
            vec![NeedsAttentionKind::PaymentAfterPrinting]
        );
    }

    #[tokio::test]
    async fn settled_payment_for_a_queued_prefix_is_not_an_anomaly() {
        let pool = test_pool().await;
        let p = db::create_prefix(&pool, None, None).await.unwrap();
        db::set_status(&pool, p.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .unwrap();
        db::set_status(&pool, p.uuid, PrefixStatus::Paid, Some("cs_1"), false)
            .await
            .unwrap();
        db::set_status(&pool, p.uuid, PrefixStatus::Queued, None, false)
            .await
            .unwrap();

        // Still in flight — a retried delivery is the normal case here.
        record_payment(
            &pool,
            None,
            TEST_BASE,
            &payment_event(Some(&p.uuid.to_string())),
        )
        .await
        .unwrap();
        assert!(anomaly_kinds(&pool).await.is_empty());
    }

    /// The printer bitmap the tests render at. Small on purpose: the default is
    /// the 600-dpi 2032x1296, and an unoptimised resize to that per card is what
    /// makes the suite slow, not what it is testing.
    const TEST_RASTER: crate::render::RasterSize = crate::render::RasterSize {
        width: 400,
        height: 256,
    };

    fn state_with(limiter: rate_limit::RateLimiter, pool: SqlitePool) -> AppState {
        AppState {
            pool: pool.clone(),
            data_dir: Arc::new(PathBuf::new()),
            raster: TEST_RASTER,
            admin_token: None,
            sessions: crate::auth::Sessions::new(pool.clone()),
            cookie_secure: None,
            stripe: None,
            notifier: None,
            login_limiter: crate::auth::LoginLimiter::default(),
            checkout_limiter: limiter,
        }
    }

    #[tokio::test]
    async fn checkout_refuses_a_ribbon_that_is_out_of_stock() {
        let pool = test_pool().await;
        db::set_ribbon_stock(&pool, Ribbon::Ymcko, false)
            .await
            .unwrap();

        let err = ensure_ribbon_orderable(&pool, Ribbon::Ymcko)
            .await
            .expect_err("an out-of-stock ribbon must refuse the sale");
        assert!(matches!(err, ApiError::Conflict(_)), "got {err:?}");
        assert!(err.to_string().contains("ymcko"), "{err}");

        // The other ribbon is untouched, so a YMCKOKO job still sells.
        ensure_ribbon_orderable(&pool, Ribbon::Ymckoko)
            .await
            .expect("YMCKOKO is still in stock");

        // Restocking reopens the sale, with no state to reset.
        db::set_ribbon_stock(&pool, Ribbon::Ymcko, true)
            .await
            .unwrap();
        ensure_ribbon_orderable(&pool, Ribbon::Ymcko)
            .await
            .expect("restocked");
    }

    #[tokio::test]
    async fn a_back_image_needs_a_real_card_on_an_unsealed_prefix() {
        let pool = test_pool().await;

        // The ribbon has no vote: a picture back is accepted on the black-panel
        // ribbon too, and reduced to black when it is rendered.
        let black = db::create_prefix(&pool, None, Some(Ribbon::Ymckoko))
            .await
            .unwrap();
        insert_card(&pool, black.uuid, 1, BackSource::None).await;
        ensure_back_image_writable(&pool, black.uuid, 1, false)
            .await
            .expect("a picture back on a YMCKOKO card");

        // ...but there must be a card to attach it to.
        let colour = db::create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .unwrap();
        let err = ensure_back_image_writable(&pool, colour.uuid, 1, false)
            .await
            .expect_err("no such card");
        assert!(matches!(err, ApiError::NotFound(_)), "got {err:?}");

        // With the card there it goes through...
        insert_card(&pool, colour.uuid, 1, BackSource::None).await;
        ensure_back_image_writable(&pool, colour.uuid, 1, false)
            .await
            .expect("a colour back on a YMCKO card");

        // ...until the prefix is sealed for the customer. The operator keeps the
        // write while the order is in flight, which is what lets a missing back
        // be filled in after the customer has paid.
        db::set_status(&pool, colour.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .unwrap();
        let err = ensure_back_image_writable(&pool, colour.uuid, 1, false)
            .await
            .expect_err("sealed for the customer");
        assert!(matches!(err, ApiError::Conflict(_)), "got {err:?}");
        ensure_back_image_writable(&pool, colour.uuid, 1, true)
            .await
            .expect("the operator may still edit a pending order");

        // Printing freezes the write for everybody, operator included. (The
        // *status* stays movable, which is what makes a stuck job recoverable;
        // the card's sides are fixed by then.)
        db::set_status(&pool, colour.uuid, PrefixStatus::Printing, None, true)
            .await
            .unwrap();
        let err = ensure_back_image_writable(&pool, colour.uuid, 1, true)
            .await
            .expect_err("printing is frozen");
        assert!(matches!(err, ApiError::Conflict(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn an_order_with_a_card_missing_its_back_is_not_sellable() {
        let pool = test_pool().await;

        // One of the cards still has no back image, so the order cannot be
        // printed and must not be sold.
        let p = db::create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .unwrap();
        insert_card(&pool, p.uuid, 1, BackSource::Image).await;
        db::upsert_card(
            &pool,
            p.uuid,
            2,
            "no back yet",
            None,
            BackSource::Image,
            None,
            "blobs/2.png",
            "image/png",
            None,
            None,
        )
        .await
        .unwrap();
        let err = ensure_order_printable(&pool, p.uuid)
            .await
            .expect_err("a card is missing its back image");
        assert!(matches!(err, ApiError::Conflict(_)), "got {err:?}");
        assert!(err.to_string().contains("1 card"), "{err}");

        // Supplying the missing back opens the sale, with nothing to reset.
        db::set_card_back_image(&pool, p.uuid, 2, "blobs/2-back.png", "image/png")
            .await
            .unwrap();
        ensure_order_printable(&pool, p.uuid)
            .await
            .expect("every card is complete");

        // A markdown back with no text is just as incomplete...
        db::upsert_card(
            &pool,
            p.uuid,
            3,
            "blank text",
            None,
            BackSource::Markdown,
            Some("  "),
            "blobs/3.png",
            "image/png",
            None,
            None,
        )
        .await
        .unwrap();
        let err = ensure_order_printable(&pool, p.uuid)
            .await
            .expect_err("a markdown back with no text is not a back");
        assert!(err.to_string().contains("1 card"), "{err}");

        // ...while a single-sided card, a front copy and real markdown need
        // nothing extra at all.
        insert_card(&pool, p.uuid, 4, BackSource::Markdown).await;
        insert_card(&pool, p.uuid, 5, BackSource::Front).await;
        insert_card(&pool, p.uuid, 6, BackSource::None).await;
        db::update_card_meta(
            &pool,
            p.uuid,
            3,
            &CardPatch {
                back: Some(BackSource::None),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        ensure_order_printable(&pool, p.uuid)
            .await
            .expect("nothing is missing");
    }

    #[tokio::test]
    async fn shop_config_serves_both_stock_flags_and_derives_availability() {
        let pool = test_pool().await;
        db::set_ribbon_stock(&pool, crate::models::Ribbon::Ymckoko, false)
            .await
            .unwrap();
        let Json(config) = shop_config(State(state_with(
            rate_limit::RateLimiter::new(5, Duration::from_secs(60)),
            pool,
        )))
        .await
        .expect("shop config");

        // Two flags, not three, and both are served.
        assert_eq!(config.ribbons.len(), 2);
        assert!(config
            .ribbons
            .iter()
            .any(|r| r.ribbon == crate::models::Ribbon::Ymcko && r.in_stock));
        assert!(config
            .ribbons
            .iter()
            .any(|r| r.ribbon == crate::models::Ribbon::Ymckoko && !r.in_stock));

        // Every ribbon is still offered — an out-of-stock one is disabled in
        // the UI, never dropped from the list.
        assert_eq!(config.prices.len(), 2);
        assert!(config
            .prices
            .iter()
            .all(|p| p.available == (p.ribbon != Ribbon::Ymckoko)));
    }

    #[tokio::test]
    async fn ribbon_stock_is_admin_only() {
        let pool = test_pool().await;
        // No ADMIN_TOKEN configured: the admin API is off, so this is a 503
        // rather than a silent success.
        let st = state_with(
            rate_limit::RateLimiter::new(5, Duration::from_secs(60)),
            pool.clone(),
        );
        let off = put_ribbon_stock(
            State(st.clone()),
            HeaderMap::new(),
            Path("ymcko".to_string()),
            Json(crate::models::RibbonStockPatch { in_stock: false }),
        )
        .await
        .expect_err("no admin token configured");
        assert!(matches!(off, ApiError::Unavailable(_)), "got {off:?}");

        // Configured, but the caller presents no credential: 401, and nothing
        // was written.
        let mut st = st;
        st.admin_token = Some(Arc::from("secret"));
        let denied = put_ribbon_stock(
            State(st.clone()),
            HeaderMap::new(),
            Path("ymcko".to_string()),
            Json(crate::models::RibbonStockPatch { in_stock: false }),
        )
        .await
        .expect_err("customer call");
        assert!(matches!(denied, ApiError::Unauthorized(_)), "got {denied:?}");
        assert!(db::ribbon_in_stock(&pool, crate::models::Ribbon::Ymcko)
            .await
            .unwrap());

        // An unknown ribbon is a 400, even for the operator.
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());
        let bad = put_ribbon_stock(
            State(st.clone()),
            headers.clone(),
            Path("ymckokok".to_string()),
            Json(crate::models::RibbonStockPatch { in_stock: false }),
        )
        .await
        .expect_err("unknown ribbon");
        assert!(matches!(bad, ApiError::BadRequest(_)), "got {bad:?}");

        // The operator's call lands.
        let Json(updated) = put_ribbon_stock(
            State(st),
            headers,
            Path("ymcko".to_string()),
            Json(crate::models::RibbonStockPatch { in_stock: false }),
        )
        .await
        .expect("operator clears the flag");
        assert_eq!(updated.ribbon, crate::models::Ribbon::Ymcko);
        assert!(!updated.in_stock);
        assert!(!db::ribbon_in_stock(&pool, crate::models::Ribbon::Ymcko)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn ribbon_price_is_admin_only_and_bounded() {
        let pool = test_pool().await;
        let mut st = state_with(
            rate_limit::RateLimiter::new(5, Duration::from_secs(60)),
            pool.clone(),
        );
        st.admin_token = Some(Arc::from("secret"));
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer secret".parse().unwrap());

        // A customer may not price a ribbon.
        let denied = put_ribbon_price(
            State(st.clone()),
            HeaderMap::new(),
            Path("ymcko".to_string()),
            Json(RibbonPricePatch { price_cents: 1 }),
        )
        .await
        .expect_err("customer call");
        assert!(matches!(denied, ApiError::Unauthorized(_)), "got {denied:?}");

        // Unknown ribbon, and a price that is really a euro figure typed as
        // cents, are both refused before anything is written.
        for (ribbon, price) in [("ymckokok", 500), ("ymcko", -1), ("ymcko", 999_999)] {
            let bad = put_ribbon_price(
                State(st.clone()),
                headers.clone(),
                Path(ribbon.to_string()),
                Json(RibbonPricePatch { price_cents: price }),
            )
            .await
            .unwrap_err();
            assert!(matches!(bad, ApiError::BadRequest(_)), "{ribbon} {price}");
        }
        assert_eq!(
            db::ribbon_price(&pool, crate::models::Ribbon::Ymcko)
                .await
                .unwrap(),
            500,
            "a refused write must not touch the stored price"
        );

        // The operator's call lands, with the unit derived from the ribbon.
        let Json(updated) = put_ribbon_price(
            State(st),
            headers,
            Path("ymckoko".to_string()),
            Json(RibbonPricePatch { price_cents: 720 }),
        )
        .await
        .expect("operator sets a price");
        assert_eq!(updated.ribbon, crate::models::Ribbon::Ymckoko);
        assert_eq!(updated.price_cents, 720);
        assert_eq!(updated.unit, "card");
        assert!(updated.available);
        assert_eq!(
            db::ribbon_price(&pool, crate::models::Ribbon::Ymckoko)
                .await
                .unwrap(),
            720
        );
    }

    #[tokio::test]
    async fn a_price_change_does_not_reprice_an_open_order() {
        // The checkout's figure is computed when the session opens; pricing a
        // ribbon afterwards must not change what a live session charges.
        let pool = test_pool().await;
        let p = db::create_prefix(&pool, None, Some(crate::models::Ribbon::Ymcko))
            .await
            .unwrap();
        insert_card(&pool, p.uuid, 1, BackSource::None).await;

        let before = db::billed_units(&pool, p.uuid, p.ribbon).await.unwrap()
            * db::ribbon_price(&pool, p.ribbon).await.unwrap();
        db::set_ribbon_price(&pool, p.ribbon, 900).await.unwrap();
        let after = db::billed_units(&pool, p.uuid, p.ribbon).await.unwrap()
            * db::ribbon_price(&pool, p.ribbon).await.unwrap();

        assert_eq!(before, 500, "one printed side at the seeded price");
        assert_eq!(after, 900, "the same order, priced again after the change");
    }

    #[tokio::test]
    async fn checkout_is_throttled_per_prefix() {
        let pool = test_pool().await;
        // Limit 1 per window: the first call is admitted and proceeds to the
        // (off) shop check — 503 — and the second is throttled with 429 before
        // it touches the database or Stripe.
        let st = state_with(
            crate::rate_limit::RateLimiter::new(1, Duration::from_secs(60)),
            pool,
        );
        let p = Uuid::new_v4().to_string();

        let first = create_checkout(State(st.clone()), HeaderMap::new(), Path(p.clone())).await;
        assert!(
            matches!(first, Err(ApiError::Unavailable(_))),
            "first use should pass the limiter and hit the not-configured shop, got {first:?}"
        );

        let second = create_checkout(State(st), HeaderMap::new(), Path(p)).await;
        assert!(
            matches!(second, Err(ApiError::TooManyRequests(_))),
            "second use inside the window should be throttled, got {second:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Email notifications on a paid order
    // -----------------------------------------------------------------------

    use crate::notify::tests::{spawn_mock_resend, test_notifier};

    /// A pending prefix, ready for a settled payment.
    async fn pending_prefix(pool: &SqlitePool) -> Prefix {
        let p = db::create_prefix(pool, None, None).await.unwrap();
        db::set_status(pool, p.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .unwrap();
        p
    }

    #[tokio::test]
    async fn a_paid_order_mails_the_customer_and_the_operator_once() {
        let pool = test_pool().await;
        let mock = spawn_mock_resend().await;
        let notifier = test_notifier(&mock.base_url, Some("shop@example.test"));
        let p = pending_prefix(&pool).await;
        let event = payment_event(Some(&p.uuid.to_string()));

        // The same event three times, as Stripe redelivers it when a response is
        // slow or lost. The confirmation must still go out once.
        for _ in 0..3 {
            record_payment(&pool, Some(&notifier), TEST_BASE, &event)
                .await
                .expect("the webhook path");
        }

        assert_eq!(status_of(&pool, p.uuid).await, PrefixStatus::Paid);
        assert_eq!(
            mock.len(),
            2,
            "one customer confirmation, one operator note"
        );

        let customer = mock.to("ada@example.test").expect("customer email");
        let text = customer["text"].as_str().expect("a text part");
        assert!(
            text.contains(&format!("{TEST_BASE}/p/{}", p.uuid)),
            "the customer needs a way back to their project: {text}"
        );
        assert!(text.contains("17.50 EUR"), "{text}");

        let operator = mock.to("shop@example.test").expect("operator note");
        assert!(
            operator["text"]
                .as_str()
                .expect("a text part")
                .contains(&format!("{TEST_BASE}/admin/p/{}", p.uuid)),
            "the operator note should link to the admin view"
        );
    }

    #[tokio::test]
    async fn an_unreachable_resend_never_fails_the_webhook() {
        let pool = test_pool().await;
        // Nothing listens on port 1, so connecting is refused immediately — a
        // Resend outage, in effect.
        let notifier = test_notifier("http://127.0.0.1:1", Some("shop@example.test"));
        let p = pending_prefix(&pool).await;

        // `Ok` is the assertion that matters: a mail failure must not become a
        // webhook error, because Stripe would then retry for days, and the order
        // must still be recorded.
        record_payment(
            &pool,
            Some(&notifier),
            TEST_BASE,
            &payment_event(Some(&p.uuid.to_string())),
        )
        .await
        .expect("an email failure must never fail the webhook");
        assert_eq!(status_of(&pool, p.uuid).await, PrefixStatus::Paid);
    }

    #[tokio::test]
    async fn a_dead_end_payment_mails_nobody() {
        let pool = test_pool().await;
        let mock = spawn_mock_resend().await;
        let notifier = test_notifier(&mock.base_url, Some("shop@example.test"));
        // A draft prefix: a settled payment with nothing awaiting it. That is a
        // dead end for the "needs attention" list, not an order confirmation —
        // there is no order to confirm.
        let p = db::create_prefix(&pool, None, None).await.unwrap();

        record_payment(
            &pool,
            Some(&notifier),
            TEST_BASE,
            &payment_event(Some(&p.uuid.to_string())),
        )
        .await
        .expect("the webhook path");

        assert_eq!(mock.len(), 0, "no email for a payment that cannot attach");
        assert_eq!(
                    anomaly_kinds(&pool).await,
                    vec![NeedsAttentionKind::PaymentForDraft]
                );
            }

            #[tokio::test]
            async fn rerender_produces_display_and_raster_for_a_front() {
                // A real PNG in memory, written to a temp data dir so the pipeline can
                // read the original back and write the derived blobs.
                let dir = std::env::temp_dir().join(format!("esplp-test-{}", Uuid::new_v4()));
                std::fs::create_dir_all(&dir).unwrap();
                let pool = test_pool().await;
                let p = db::create_prefix(&pool, None, Some(Ribbon::Ymcko))
                    .await
                    .unwrap();

                // Write a 2:1 PNG as the front original, then create the card pointing
                // at it.
                let img = image::RgbImage::from_pixel(200, 100, image::Rgb([200, 100, 50]));
                let mut out = std::io::Cursor::new(Vec::new());
                image::DynamicImage::ImageRgb8(img)
                    .write_to(&mut out, image::ImageFormat::Png)
                    .unwrap();
                let bytes = out.into_inner();
                let rel = format!("blobs/{}/1.png", p.uuid);
                let abs = dir.join(&rel);
                std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
                std::fs::write(&abs, &bytes).unwrap();

                db::upsert_card(
                    &pool,
                    p.uuid,
                    1,
                    "front",
                    None,
                    BackSource::None,
                    None,
                    &rel,
                    "image/png",
                    None,
                    None,
                )
                .await
                .unwrap();

                let st = AppState {
                    pool: pool.clone(),
                    data_dir: Arc::new(dir.clone()),
                    raster: TEST_RASTER,
                    admin_token: None,
                    sessions: crate::auth::Sessions::new(pool.clone()),
                    cookie_secure: None,
                    stripe: None,
                    notifier: None,
                    login_limiter: crate::auth::LoginLimiter::default(),
                    checkout_limiter: rate_limit::RateLimiter::new(5, Duration::from_secs(60)),
                };

                let card = rerender_card(&st, p.uuid, 1).await.unwrap();
                assert_eq!(card.display_front, RenderStatus::Ready);
                assert_eq!(card.print_front, RenderStatus::Ready);
                // Single-sided: no back side at all.
                assert_eq!(card.display_back, None);
                assert_eq!(card.print_back, None);

                // The derived blobs exist and are the right kinds.
                let display = std::fs::read(dir.join(format!("blobs/{}/1-front-display.png", p.uuid))).unwrap();
                assert_eq!(&display[0..8], b"\x89PNG\r\n\x1a\n");
                let raster = std::fs::read(dir.join(format!("blobs/{}/1-front-print.bmp", p.uuid))).unwrap();
                assert_eq!(&raster[0..2], b"BM");
                assert_eq!(raster[28], 24);

                // The endpoints serve them with the right status semantics.
                let resp = get_card_display_image(
                    State(st.clone()),
                    HeaderMap::new(),
                    Path((p.uuid.to_string(), 1)),
                )
                    .await
                    .unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                let resp = get_card_print_raster(
                    State(st.clone()),
                    HeaderMap::new(),
                    Path((p.uuid.to_string(), 1)),
                )
                    .await
                    .unwrap();
                assert_eq!(resp.status(), axum::http::StatusCode::OK);
                // A back side that does not exist is a 404.
                let err = get_card_display_back(
                    State(st.clone()),
                    HeaderMap::new(),
                    Path((p.uuid.to_string(), 1)),
                )
                    .await
                    .unwrap_err();
                assert!(matches!(err, ApiError::NotFound(_)), "{err:?}");

                let _ = std::fs::remove_dir_all(&dir);
            }

            /// A prefix with one picture-backed card, rendered, on a real temp data dir.
            /// Returns the state, the prefix and the temporary directory (so the caller
            /// can read the blobs it wrote).
            async fn rendered_picture_back(ribbon: Ribbon) -> (AppState, Uuid, PathBuf) {
                let dir = std::env::temp_dir().join(format!("esplp-test-{}", Uuid::new_v4()));
                std::fs::create_dir_all(&dir).unwrap();
                let pool = test_pool().await;
                let p = db::create_prefix(&pool, None, Some(ribbon)).await.unwrap();

                let write_png = |name: &str, w: u32, h: u32, v: u8| {
                    let img = image::RgbImage::from_pixel(w, h, image::Rgb([v, v, v]));
                    let mut out = std::io::Cursor::new(Vec::new());
                    image::DynamicImage::ImageRgb8(img)
                        .write_to(&mut out, image::ImageFormat::Png)
                        .unwrap();
                    let rel = format!("blobs/{}/{name}", p.uuid);
                    let abs = dir.join(&rel);
                    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
                    std::fs::write(&abs, out.into_inner()).unwrap();
                    rel
                };
                let front = write_png("1.png", 200, 126, 200);
                let back = write_png("1-back.png", 100, 100, 16);

                db::upsert_card(
                    &pool,
                    p.uuid,
                    1,
                    "picture back",
                    None,
                    BackSource::Image,
                    None,
                    &front,
                    "image/png",
                    Some(&back),
                    Some("image/png"),
                )
                .await
                .unwrap();
                let st = AppState {
                    pool: pool.clone(),
                    data_dir: Arc::new(dir.clone()),
                    raster: TEST_RASTER,
                    admin_token: None,
                    sessions: crate::auth::Sessions::new(pool.clone()),
                    cookie_secure: None,
                    stripe: None,
                    notifier: None,
                    login_limiter: crate::auth::LoginLimiter::default(),
                    checkout_limiter: rate_limit::RateLimiter::new(5, Duration::from_secs(60)),
                };
                (st, p.uuid, dir)
            }

            /// The bit depth of a rendered back raster on disk.
            fn back_raster_bits(dir: &PathBuf, prefix: Uuid) -> u16 {
                let raw = std::fs::read(dir.join(format!("blobs/{prefix}/1-back-print.bmp"))).unwrap();
                assert_eq!(&raw[0..2], b"BM");
                u16::from_le_bytes([raw[28], raw[29]])
            }

            /// One ribbon, and the renderer and the print queue both read it. The
            /// queue's `back` is what decides whether the spooler calls
            /// `set_image_buffer` or `set_black_buffer`, so a raster rendered at the
            /// other depth would be fed to the wrong call — and a *paid* prefix is no
            /// exception: the charge is history, the ribbon is a production decision.
            #[tokio::test]
            async fn the_back_raster_follows_the_prefix_s_ribbon_after_a_switch() {
                let (st, prefix, dir) = rendered_picture_back(Ribbon::Ymcko).await;
                rerender_card(&st, prefix, 1).await.unwrap();
                assert_eq!(back_raster_bits(&dir, prefix), 24, "a colour back");
                let queue = db::print_queue(&st.pool, prefix).await.unwrap();
                assert_eq!(queue.ribbon, Ribbon::Ymcko);

                // A ribbon switch: the rasters are re-rendered, and the queue reports
                // the same thing the raster is.
                let updated = db::set_prefix_ribbon(&st.pool, prefix, Ribbon::Ymckoko)
                    .await
                    .unwrap();
                assert_eq!(updated.ribbon, Ribbon::Ymckoko);
                rerender_card(&st, prefix, 1).await.unwrap();
                assert_eq!(back_raster_bits(&dir, prefix), 1, "re-rendered in 1 bit");
                let queue = db::print_queue(&st.pool, prefix).await.unwrap();
                assert_eq!(queue.ribbon, Ribbon::Ymckoko);
                assert_eq!(queue.cards[0].back, crate::models::BackAvailability::Black);

                // And back again, so the depth is a function of `ribbon` and nothing
                // else.
                db::set_prefix_ribbon(&st.pool, prefix, Ribbon::Ymcko).await.unwrap();
                rerender_card(&st, prefix, 1).await.unwrap();
                assert_eq!(back_raster_bits(&dir, prefix), 24);
                let queue = db::print_queue(&st.pool, prefix).await.unwrap();
                assert_eq!(queue.cards[0].back, crate::models::BackAvailability::Color);
            }

            /// An artifact's path never changes but its bytes do, so it has to carry a
            /// validator: a browser that keeps the old URL must be able to be told
            /// "still current" or "here is the new one" rather than trusting its copy.
            #[tokio::test]
            async fn artifacts_carry_an_etag_and_answer_if_none_match() {
                let (st, prefix, _dir) = rendered_picture_back(Ribbon::Ymcko).await;
                rerender_card(&st, prefix, 1).await.unwrap();
                let (p, id) = (prefix.to_string(), 1i64);

                let res = get_card_display_image(State(st.clone()), HeaderMap::new(), Path((p.clone(), id)))
                    .await
                    .expect("the front display image");
                assert_eq!(res.status(), StatusCode::OK);
                let etag = res
                    .headers()
                    .get(header::ETAG)
                    .expect("an ETag")
                    .to_str()
                    .unwrap()
                    .to_string();
                assert_eq!(
                    res.headers().get(header::CACHE_CONTROL).unwrap(),
                    "no-cache",
                    "a cached copy must be revalidated, not reused blind"
                );

                // The same validator comes back as a 304 with no body.
                let mut req = HeaderMap::new();
                req.insert(header::IF_NONE_MATCH, etag.parse().unwrap());
                let res = get_card_display_image(State(st), req, Path((p, id)))
                    .await
                    .expect("the conditional request");
                assert_eq!(res.status(), StatusCode::NOT_MODIFIED);
            }

            #[tokio::test]
            async fn rerender_marks_a_markdown_back_failed_until_a_renderer_lands() {
                let dir = std::env::temp_dir().join(format!("esplp-test-{}", Uuid::new_v4()));
                std::fs::create_dir_all(&dir).unwrap();
                let pool = test_pool().await;
                let p = db::create_prefix(&pool, None, Some(Ribbon::Ymckoko))
                    .await
                    .unwrap();

                let img = image::RgbImage::from_pixel(200, 100, image::Rgb([10, 20, 30]));
                let mut out = std::io::Cursor::new(Vec::new());
                image::DynamicImage::ImageRgb8(img)
                    .write_to(&mut out, image::ImageFormat::Png)
                    .unwrap();
                let bytes = out.into_inner();
                let rel = format!("blobs/{}/1.png", p.uuid);
                let abs = dir.join(&rel);
                std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
                std::fs::write(&abs, &bytes).unwrap();

                db::upsert_card(
                    &pool,
                    p.uuid,
                    1,
                    "card",
                    None,
                    BackSource::Markdown,
                    Some("# Title"),
                    &rel,
                    "image/png",
                    None,
                    None,
                )
                .await
                .unwrap();

                let st = AppState {
                    pool: pool.clone(),
                    data_dir: Arc::new(dir.clone()),
                    raster: TEST_RASTER,
                    admin_token: None,
                    sessions: crate::auth::Sessions::new(pool.clone()),
                    cookie_secure: None,
                    stripe: None,
                    notifier: None,
                    login_limiter: crate::auth::LoginLimiter::default(),
                    checkout_limiter: rate_limit::RateLimiter::new(5, Duration::from_secs(60)),
                };

                let card = rerender_card(&st, p.uuid, 1).await.unwrap();
                // Front renders; the markdown back is a stub until a real
                // renderer lands.
                assert_eq!(card.display_front, RenderStatus::Ready);
                assert_eq!(card.print_front, RenderStatus::Ready);
                assert_eq!(card.display_back, Some(RenderStatus::Failed));
                assert_eq!(card.print_back, Some(RenderStatus::Failed));

                // The failed back raster answers 500, not a blank 200.
                let err = get_card_print_raster_back(
                    State(st.clone()),
                    HeaderMap::new(),
                    Path((p.uuid.to_string(), 1)),
                )
                    .await
                    .unwrap_err();
                assert!(matches!(err, ApiError::Internal(_)), "{err:?}");

                let _ = std::fs::remove_dir_all(&dir);
            }
        }
