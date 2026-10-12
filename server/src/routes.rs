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
    AdminLogin, AdminSession, Card, CardIndex, CardPatch, Prefix, PrefixInput, PrefixModePatch,
    PrefixStatus, PrintMode, PrintQueue, ShopConfig, StatusInput,
};
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
fn write_blob(data_dir: &PathBuf, prefix: Uuid, filename: &str, bytes: &[u8]) -> Result<String, ApiError> {
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
async fn store_card_form(
    mut mp: Multipart,
    data_dir: &PathBuf,
    prefix: Uuid,
    id: i64,
    mode: PrintMode,
) -> Result<(String, Option<String>, Option<String>, String, String, Option<String>, Option<String>), ApiError> {
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
    if mode.allows_back_image() {
        let Some((_, bct)) = &back_image else {
            return Err(ApiError::BadRequest(
                "a double_sided_color prefix requires a back_image on every card".into(),
            ));
        };
        if bct.is_empty() {
            return Err(ApiError::BadRequest("back_image is empty".into()));
        }
    } else if back_image.is_some() {
        return Err(ApiError::BadRequest(format!(
            "a back_image is only valid in a double_sided_color prefix, not {mode}"
        )));
    }

    let image_rel = write_blob(data_dir, prefix, &format!("{id}.{}", extension_for(&ct)), &bytes)?;
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

/// Public shop settings: price, currency, and whether payment works at all.
/// The customer UI needs this to show a total before sending anyone to Stripe.
#[utoipa::path(get, path = "/api/shop/config", tag = "shop",
    responses((status = 200, body = ShopConfig)))]
pub async fn shop_config(State(st): State<AppState>) -> Json<ShopConfig> {
    let body = match st.stripe.as_deref() {
        None => ShopConfig {
            enabled: false,
            currency: "eur".into(),
            per_card_cents: 0,
            shipping_cents: 0,
            countries: vec![],
        },
        Some(s) => {
            let p = s.pricing();
            ShopConfig {
                enabled: true,
                currency: p.currency.clone(),
                per_card_cents: p.per_card_cents,
                shipping_cents: p.shipping_cents,
                countries: p.countries.clone(),
            }
        }
    };
    Json(body)
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
        (status = 409, description = "the prefix has no cards, or is already past payment", body = crate::error::ErrorBody),
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
        .create_checkout_session(prefix, p.card_count, &base)
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
            record_payment(&st, &event).await?;
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
async fn record_payment(st: &AppState, event: &stripe::Event) -> Result<(), ApiError> {
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

    let current = match db::get_prefix(&st.pool, prefix).await {
        Ok(p) => p,
        Err(ApiError::NotFound(_)) => {
            // Paid, then deleted. The money is real and there is nowhere to put
            // it, so this needs a human — shout, and let it be 200 so Stripe
            // stops retrying an event that can never succeed.
            tracing::error!(%prefix, session = %session.id, amount = ?session.amount_total,
                "payment received for a prefix that no longer exists: refund it, or rebuild \
                 the order by hand");
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    match current.status {
        PrefixStatus::PaymentPending => {
            let updated = db::set_status(
                &st.pool,
                prefix,
                PrefixStatus::Paid,
                Some(&session.id),
                false,
            )
            .await?;
            tracing::info!(%prefix, session = %session.id, amount = ?session.amount_total,
                currency = ?session.currency, "payment recorded, prefix is now {}", updated.status);
        }
        // Already handled — the normal case for a retried delivery.
        PrefixStatus::Paid
        | PrefixStatus::Queued
        | PrefixStatus::Printing
        | PrefixStatus::Printed => {
            tracing::info!(%prefix, session = %session.id, status = %current.status,
                "payment already recorded; ignoring the duplicate");
        }
        // A draft (nothing in flight) or a cancelled order. Deliberately not
        // forced: whether to refund or to honour the order is the operator's
        // call, and guessing wrong either prints an order nobody wants or
        // silently keeps money.
        PrefixStatus::Draft | PrefixStatus::Cancelled => {
            tracing::warn!(%prefix, session = %session.id, status = %current.status,
                "payment settled for a prefix that is neither payment_pending nor paid; it has been \
                 left alone — decide in the admin UI whether to refund or to honour it");
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
            axum::routing::get(get_card_back_image),
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
        .route("/api/shop/config", axum::routing::get(shop_config))
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
    use std::sync::Arc;
    use std::time::Duration;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

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
            metadata: None,
        }
    }

    async fn status_of(pool: &SqlitePool, uuid: Uuid) -> PrefixStatus {
        db::get_prefix(pool, uuid).await.unwrap().status
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

    fn state_with(limiter: rate_limit::RateLimiter, pool: SqlitePool) -> AppState {
        AppState {
            pool: pool.clone(),
            data_dir: Arc::new(PathBuf::new()),
            admin_token: None,
            sessions: crate::auth::Sessions::new(pool.clone()),
            cookie_secure: None,
            stripe: None,
            login_limiter: crate::auth::LoginLimiter::default(),
            checkout_limiter: limiter,
        }
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
}
