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
    AdminLogin, AdminSession, Card, CardIndex, CardPatch, NeedsAttentionItem, NeedsAttentionKind,
    Prefix, PrefixInput, PrefixModePatch, PrefixStatus, PrintMode, PrintQueue, Ribbon, RibbonStock,
    RibbonStockPatch, ShopConfig, StatusInput,
};
use crate::notify;
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

/// Pull `name`, `serial_number`, `back_text`, an `image` file and an optional
/// `back_image` file out of a multipart form, validating the back fields
/// against the prefix's `print_mode` *before* writing anything to disk.
///
/// The `back_image` is **optional even in `double_sided_color`**: a card exists
/// from its front image, and the colour back is added per card afterwards
/// (`put_card_back_image`, or this same form). Requiring it here would mean the
/// front-image upload — a bulk, folder-at-a-time flow — could create nothing at
/// all in that mode. What has to hold is that no *order* is sold with a back
/// missing, and that is checked where the money is (`ensure_order_printable`),
/// not here.
async fn store_card_form(
    mut mp: Multipart,
    data_dir: &PathBuf,
    prefix: Uuid,
    id: i64,
    mode: PrintMode,
) -> Result<
    (
        String,
        Option<String>,
        Option<String>,
        String,
        String,
        Option<String>,
        Option<String>,
    ),
    ApiError,
> {
    let mut name: Option<String> = None;
    let mut serial: Option<String> = None;
    let mut back: Option<String> = None;
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
            Some("back_text") => {
                let v = field
                    .text()
                    .await
                    .map_err(|e| ApiError::BadRequest(e.to_string()))?;
                if !v.is_empty() {
                    back = Some(v);
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

    // Back side validity is fixed by the prefix's print mode. Reject rather
    // than silently store a field the mode will never render.
    if back.is_some() && !mode.allows_back_text() {
        return Err(ApiError::BadRequest(format!(
            "back_text is only valid in a colour_front_black_back prefix, not {mode}"
        )));
    }
    if back_image.is_some() && !mode.allows_back_image() {
        return Err(ApiError::BadRequest(format!(
            "a back_image is only valid in a double_sided_color prefix, not {mode}"
        )));
    }
    if let Some((bbytes, _)) = &back_image {
        if bbytes.is_empty() {
            return Err(ApiError::BadRequest("back_image is empty".into()));
        }
    }

    let image_rel = write_blob(
        data_dir,
        prefix,
        &format!("{id}.{}", extension_for(&ct)),
        &bytes,
    )?;
    let (back_rel, back_ct) = match back_image {
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

    Ok((name, serial, back, image_rel, ct, back_rel, back_ct))
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
    let p = db::create_prefix(&st.pool, input.uuid, input.print_mode).await?;
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
    let mode = db::ensure_writable(&st.pool, prefix, caller.is_admin())
        .await?
        .print_mode;
    let (name, serial, back, rel, ct, back_rel, back_ct) =
        store_card_form(mp, &st.data_dir, prefix, id, mode).await?;
    let card = db::upsert_card(
        &st.pool,
        prefix,
        id,
        &name,
        serial.as_deref(),
        back.as_deref(),
        &rel,
        &ct,
        back_rel.as_deref(),
        back_ct.as_deref(),
    )
    .await?;
    Ok(Json(card))
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
    let mode = db::ensure_writable(&st.pool, prefix, caller.is_admin())
        .await?
        .print_mode;
    let id = db::next_card_id(&st.pool, prefix).await?;
    let (name, serial, back, rel, ct, back_rel, back_ct) =
        store_card_form(mp, &st.data_dir, prefix, id, mode).await?;
    let card = db::upsert_card(
        &st.pool,
        prefix,
        id,
        &name,
        serial.as_deref(),
        back.as_deref(),
        &rel,
        &ct,
        back_rel.as_deref(),
        back_ct.as_deref(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(card)))
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

/// Reject a colour back-image write that this prefix or card cannot take.
///
/// Split out from the handler so the two refusals that are not multipart
/// plumbing — the mode has no colour back at all, and the card does not exist —
/// are testable without building a multipart body.
async fn ensure_back_image_writable(
    pool: &SqlitePool,
    prefix: Uuid,
    id: i64,
    admin: bool,
) -> Result<(), ApiError> {
    let mode = db::ensure_writable(pool, prefix, admin).await?.print_mode;
    if !mode.allows_back_image() {
        return Err(ApiError::BadRequest(format!(
            "a back_image is only valid in a double_sided_color prefix, not {mode}"
        )));
    }
    db::get_card(pool, prefix, id).await?; // 404 for an unknown card
    Ok(())
}

/// Upload or replace one card's colour back image, leaving the front alone.
///
/// This is what a `double_sided_color` card's back side is set through: cards
/// are created from their front image (the dropzone uploads a folder at a
/// time), and the back is then added per card. Only that mode has a back image
/// at all, so the other two refuse the upload rather than storing a blob
/// nothing will ever print.
///
/// Re-uploading replaces the stored blob, whatever format the old one was in —
/// `write_blob` cleans the earlier file up, so there are no orphans.
#[utoipa::path(put, path = "/api/prefixes/{prefix}/cards/{id}/back-image", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    request_body(content = String, content_type = "multipart/form-data",
        description = "Multipart form carrying exactly one file part, `back_image`."),
    responses(
        (status = 200, body = Card),
        (status = 400, description = "the mode has no colour back, or the file part is missing or empty", body = crate::error::ErrorBody),
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
    Ok(Json(
        db::set_card_back_image(&st.pool, prefix, id, &rel, &ct).await?,
    ))
}

/// Change a prefix's print mode while it is still writable.
#[utoipa::path(patch, path = "/api/prefixes/{prefix}", tag = "prefixes",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    request_body = PrefixModePatch,
    responses(
        (status = 200, body = Prefix),
        (status = 400, body = crate::error::ErrorBody),
        (status = 404, body = crate::error::ErrorBody),
        (status = 409, description = "prefix is frozen", body = crate::error::ErrorBody)
    ))]
pub async fn patch_prefix_mode(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(prefix): Path<String>,
    Json(input): Json<PrefixModePatch>,
) -> Result<Json<Prefix>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let caller = auth::resolve(&st, &headers).await?;
    db::ensure_writable(&st.pool, prefix, caller.is_admin()).await?;
    Ok(Json(
        db::set_prefix_mode(&st.pool, prefix, input.print_mode).await?,
    ))
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
    Ok(Json(
        db::update_card_meta(&st.pool, prefix, id, &patch).await?,
    ))
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

/// Public shop settings: price, currency, ribbon availability, and whether
/// payment works at all. The customer UI needs this to show a total — and which
/// print modes can actually be ordered — before sending anyone to Stripe.
///
/// Each mode carries an `available` flag, and the two ribbon stock flags behind
/// it are served as well. An out-of-stock mode stays in the list rather than
/// disappearing, so the customer sees that the option exists and is out.
#[utoipa::path(get, path = "/api/shop/config", tag = "shop",
    responses((status = 200, body = ShopConfig)))]
pub async fn shop_config(State(st): State<AppState>) -> Result<Json<ShopConfig>, ApiError> {
    let prices = db::list_mode_prices(&st.pool).await?;
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

/// Refuse to sell a card that cannot be printed.
///
/// A mode whose ribbon is out of stock is not orderable, however the customer
/// got to it: a page rendered before the ribbon ran out, or a direct API call.
/// The UI already renders such a mode disabled — this is the same rule at the
/// layer that takes the money, which is where it has to hold.
///
/// What it deliberately does *not* do is reconcile anything: a prefix already
/// priced for a mode whose ribbon has since run out is neither repriced,
/// refunded nor flagged — it simply cannot open a *new* session until the
/// ribbon is back. Sorting those out is a background, by-hand matter, and the
/// gate is kept to the one question it can answer cleanly: may this be sold
/// right now.
async fn ensure_mode_orderable(pool: &SqlitePool, mode: PrintMode) -> Result<(), ApiError> {
    let ribbon = mode.ribbon();
    if !db::ribbon_in_stock(pool, ribbon).await? {
        return Err(ApiError::Conflict(format!(
            "print mode '{mode}' is unavailable: the {ribbon} ribbon is out of stock"
        )));
    }
    Ok(())
}

/// Refuse an order that is missing part of a card, so nothing is sold that
/// cannot be printed.
///
/// The one case today is `double_sided_color`, whose colour back is uploaded
/// per card: a prefix can hold cards whose back is still missing, and nothing
/// downstream would notice until the spooler went looking for the file. Same
/// place and same reasoning as the ribbon stock check — the layer that takes the
/// money is where "can this actually be made" has to hold.
///
/// Like `ensure_mode_orderable`, it reconciles nothing: a prefix already priced
/// is not repriced or refunded, it just cannot open a *new* session until the
/// missing back is uploaded.
async fn ensure_order_printable(
    pool: &SqlitePool,
    prefix: Uuid,
    mode: PrintMode,
) -> Result<(), ApiError> {
    if !mode.allows_back_image() {
        return Ok(());
    }
    let missing = db::cards_missing_back_image(pool, prefix).await?;
    if missing > 0 {
        return Err(ApiError::Conflict(format!(
            "prefix {prefix} is {mode} and {missing} of its cards have no back image yet; \
             every card needs one before the order can be printed"
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

    ensure_mode_orderable(&st.pool, p.print_mode).await?;
    // A card is only complete once its back side is: in `double_sided_color`
    // that is an uploaded image, and a card without one would reach the spooler
    // as a front with a hole where its back should be.
    ensure_order_printable(&st.pool, prefix, p.print_mode).await?;

    // The price is the mode's, looked up from the database — not a single
    // environment-wide per-card figure. A mode with no configured price is a
    // misconfiguration and refuses the checkout rather than pricing at zero.
    let price_cents = db::mode_price(&st.pool, p.print_mode).await?;

    // Flip to payment_pending BEFORE opening the session, and note the order: a
    // customer can pay the moment they land on Stripe's page, and the webhook
    // deliberately refuses to force a payment through for a prefix that still
    // reads as a draft. Writing the status second would let a fast payment lose
    // that race and strand real money in a WARN line.
    let was_draft = p.status == PrefixStatus::Draft;
    if was_draft {
        db::set_status(&st.pool, prefix, PrefixStatus::PaymentPending, None, false).await?;
    }

    // Freeze the mode this order is being charged for, so the spooler prints
    // what the customer paid for even if the operator changes the mode later.
    // Done alongside the status flip, before the session opens, so the frozen
    // mode always matches the price locked into the session.
    db::set_paid_print_mode(&st.pool, prefix, p.print_mode).await?;

    let base = public_base_url(&st, &headers);
    let session = match stripe
        .create_checkout_session(prefix, p.card_count, price_cents, &base)
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

    tracing::info!(%prefix, session = %session.id, cards = p.card_count,
        "checkout session created; waiting for the payment webhook");
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
                        // The mode that was charged, which is what the spooler
                        // will print.
                        print_mode: updated.paid_print_mode.or(Some(updated.print_mode)),
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
                .patch(patch_prefix_mode),
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

    /// A minimal card: a front image, plus a colour back image if `back` is
    /// given. The blob paths are never read by these tests, only the metadata.
    async fn insert_card(pool: &SqlitePool, prefix: Uuid, id: i64, back: Option<&str>) {
        db::upsert_card(
            pool,
            prefix,
            id,
            "c",
            None,
            None,
            "blobs/f.png",
            "image/png",
            back,
            back.map(|_| "image/png"),
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

    fn state_with(limiter: rate_limit::RateLimiter, pool: SqlitePool) -> AppState {
        AppState {
            pool: pool.clone(),
            data_dir: Arc::new(PathBuf::new()),
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
    async fn checkout_refuses_a_mode_whose_ribbon_is_out_of_stock() {
        let pool = test_pool().await;
        db::set_ribbon_stock(&pool, crate::models::Ribbon::Ymcko, false)
            .await
            .unwrap();

        // Both YMCKO modes go: one stock flag, two modes.
        for mode in [PrintMode::SingleSided, PrintMode::DoubleSidedColor] {
            let err = ensure_mode_orderable(&pool, mode)
                .await
                .expect_err("an out-of-stock ribbon must refuse the sale");
            assert!(matches!(err, ApiError::Conflict(_)), "got {err:?}");
            assert!(err.to_string().contains("ymcko"), "{err}");
        }
        // The other ribbon is untouched, so the black-back mode still sells.
        ensure_mode_orderable(&pool, PrintMode::ColorFrontBlackBack)
            .await
            .expect("YMCKOKO is still in stock");

        // Restocking reopens the sale, with no state to reset.
        db::set_ribbon_stock(&pool, crate::models::Ribbon::Ymcko, true)
            .await
            .unwrap();
        ensure_mode_orderable(&pool, PrintMode::DoubleSidedColor)
            .await
            .expect("restocked");
    }

    #[tokio::test]
    async fn a_colour_back_image_needs_the_colour_mode_and_a_real_card() {
        let pool = test_pool().await;

        // A mode with no colour back at all: the upload is refused rather than
        // stored as a blob nothing will ever print.
        let single = db::create_prefix(&pool, None, Some(PrintMode::SingleSided))
            .await
            .unwrap();
        insert_card(&pool, single.uuid, 1, None).await;
        let err = ensure_back_image_writable(&pool, single.uuid, 1, false)
            .await
            .expect_err("single_sided has no colour back");
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");
        assert!(err.to_string().contains("double_sided_color"), "{err}");

        // The colour mode, but no such card to attach the back to.
        let colour = db::create_prefix(&pool, None, Some(PrintMode::DoubleSidedColor))
            .await
            .unwrap();
        let err = ensure_back_image_writable(&pool, colour.uuid, 1, false)
            .await
            .expect_err("no such card");
        assert!(matches!(err, ApiError::NotFound(_)), "got {err:?}");

        // With the card there it goes through...
        insert_card(&pool, colour.uuid, 1, None).await;
        ensure_back_image_writable(&pool, colour.uuid, 1, false)
            .await
            .expect("a colour back on a colour-mode card");

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
    async fn an_order_with_a_card_missing_its_colour_back_is_not_sellable() {
        let pool = test_pool().await;

        // The colour mode: one of two cards still has no back image, so the
        // order cannot be printed and must not be sold.
        let colour = db::create_prefix(&pool, None, Some(PrintMode::DoubleSidedColor))
            .await
            .unwrap();
        insert_card(&pool, colour.uuid, 1, Some("blobs/1-back.png")).await;
        insert_card(&pool, colour.uuid, 2, None).await;
        let err = ensure_order_printable(&pool, colour.uuid, PrintMode::DoubleSidedColor)
            .await
            .expect_err("a card is missing its back");
        assert!(matches!(err, ApiError::Conflict(_)), "got {err:?}");
        assert!(err.to_string().contains("back image"), "{err}");

        // Uploading the missing back opens the sale, with nothing to reset.
        db::set_card_back_image(&pool, colour.uuid, 2, "blobs/2-back.png", "image/png")
            .await
            .unwrap();
        ensure_order_printable(&pool, colour.uuid, PrintMode::DoubleSidedColor)
            .await
            .expect("every card is complete");

        // The other two modes have no colour back to be missing: their cards
        // carry no back image at all and that is the whole card.
        let single = db::create_prefix(&pool, None, Some(PrintMode::SingleSided))
            .await
            .unwrap();
        insert_card(&pool, single.uuid, 1, None).await;
        ensure_order_printable(&pool, single.uuid, PrintMode::SingleSided)
            .await
            .expect("single-sided has no back side");
        let black = db::create_prefix(&pool, None, None).await.unwrap();
        insert_card(&pool, black.uuid, 1, None).await;
        ensure_order_printable(&pool, black.uuid, PrintMode::ColorFrontBlackBack)
            .await
            .expect("the black back is text, not an image");
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

        // Every mode is still offered — an out-of-stock one is disabled in the
        // UI, never dropped from the list.
        assert_eq!(config.prices.len(), 3);
        assert!(config
            .prices
            .iter()
            .all(|p| p.available == (p.print_mode != PrintMode::ColorFrontBlackBack)));
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
}
