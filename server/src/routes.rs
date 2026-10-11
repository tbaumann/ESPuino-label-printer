use std::path::PathBuf;

use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde::Deserialize;
use uuid::Uuid;

use crate::db;
use crate::error::ApiError;
use crate::models::{Card, CardIndex, CardPatch, Prefix, PrefixInput, PrefixStatus, StatusInput};
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

/// Pull `name`, `serial_number`, `back_text` and an `image` file out of a
/// multipart form, writing the image under the data dir.
async fn store_card_form(
    mut mp: Multipart,
    data_dir: &PathBuf,
    prefix: Uuid,
    id: i64,
) -> Result<(String, Option<String>, Option<String>, String, String), ApiError> {
    let mut name: Option<String> = None;
    let mut serial: Option<String> = None;
    let mut back: Option<String> = None;
    let mut image: Option<(Vec<u8>, String)> = None;

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
            _ => {}
        }
    }

    let name = name.ok_or_else(|| ApiError::BadRequest("field 'name' is required".into()))?;
    let (bytes, ct) =
        image.ok_or_else(|| ApiError::BadRequest("file field 'image' is required".into()))?;
    if bytes.is_empty() {
        return Err(ApiError::BadRequest("image is empty".into()));
    }

    let rel = format!(
        "blobs/{}/{}.{}",
        prefix,
        id,
        extension_for(&ct)
    );
    let abs = data_dir.join(&rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent)?;
        // Drop any previous blob for this card with a different extension,
        // e.g. when a card is re-uploaded as jpg over an existing png.
        let keep = abs.file_name().and_then(|s| s.to_str()).unwrap_or_default();
        let id_prefix = format!("{id}.");
        if let Ok(entries) = std::fs::read_dir(parent) {
            for entry in entries.flatten() {
                let fname = entry.file_name();
                let fname = fname.to_string_lossy();
                if fname.starts_with(&id_prefix) && fname != keep {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
    std::fs::write(&abs, &bytes)?;

    Ok((name, serial, back, rel, ct))
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
    let p = db::create_prefix(&st.pool, input.uuid).await?;
    Ok((StatusCode::CREATED, Json(p)))
}

/// Query for listing prefixes.
#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    /// Only return prefixes in this status (e.g. `paid` for the print queue).
    pub status: Option<String>,
}

/// List prefixes, optionally filtered by status.
#[utoipa::path(get, path = "/api/prefixes", tag = "prefixes",
    params(("status" = Option<String>, Query, description = "filter by status (draft|submitted|paid|queued|printing|printed|cancelled)")),
    responses((status = 200, body = [Prefix])))]
pub async fn list_prefixes(
    State(st): State<AppState>,
    Query(q): Query<StatusQuery>,
) -> Result<Json<Vec<Prefix>>, ApiError> {
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
    Path((prefix, id)): Path<(String, i64)>,
    mp: Multipart,
) -> Result<Json<Card>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    db::ensure_writable(&st.pool, prefix).await?;
    let (name, serial, back, rel, ct) =
        store_card_form(mp, &st.data_dir, prefix, id).await?;
    let card = db::upsert_card(
        &st.pool,
        prefix,
        id,
        &name,
        serial.as_deref(),
        back.as_deref(),
        &rel,
        &ct,
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
    Path(prefix): Path<String>,
    mp: Multipart,
) -> Result<(StatusCode, Json<Card>), ApiError> {
    let prefix = parse_uuid(&prefix)?;
    db::ensure_writable(&st.pool, prefix).await?;
    let id = db::next_card_id(&st.pool, prefix).await?;
    let (name, serial, back, rel, ct) =
        store_card_form(mp, &st.data_dir, prefix, id).await?;
    let card = db::upsert_card(
        &st.pool,
        prefix,
        id,
        &name,
        serial.as_deref(),
        back.as_deref(),
        &rel,
        &ct,
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
    let rel = format!(
        "blobs/{}/{}.{}",
        prefix,
        id,
        extension_for(&card.image_content_type)
    );
    let bytes = std::fs::read(st.data_dir.join(&rel))
        .map_err(|_| ApiError::NotFound(format!("image for card {id}")))?;
    Ok(([(header::CONTENT_TYPE, card.image_content_type)], bytes).into_response())
}

/// Delete a card (metadata + image).
#[utoipa::path(delete, path = "/api/prefixes/{prefix}/cards/{id}", tag = "cards",
    params(("prefix" = Uuid, Path, description = "prefix uuid"), ("id" = i64, Path, description = "card index within the prefix")),
    responses((status = 204, description = "deleted"), (status = 404, body = crate::error::ErrorBody)))]
pub async fn delete_card(
    State(st): State<AppState>,
    Path((prefix, id)): Path<(String, i64)>,
) -> Result<StatusCode, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    db::ensure_writable(&st.pool, prefix).await?;
    let card = db::get_card(&st.pool, prefix, id).await?;
    db::delete_card(&st.pool, prefix, id).await?;
    let rel = format!(
        "blobs/{}/{}.{}",
        prefix,
        id,
        extension_for(&card.image_content_type)
    );
    let _ = std::fs::remove_file(st.data_dir.join(&rel));
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a prefix and all of its cards (and their images).
#[utoipa::path(delete, path = "/api/prefixes/{prefix}", tag = "prefixes",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    responses(
        (status = 204, description = "deleted"),
        (status = 404, body = crate::error::ErrorBody),
        (status = 409, description = "prefix is currently printing", body = crate::error::ErrorBody)
    ))]
pub async fn delete_prefix(
    State(st): State<AppState>,
    Path(prefix): Path<String>,
) -> Result<StatusCode, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    let p = db::get_prefix(&st.pool, prefix).await?;
    if p.status == PrefixStatus::Printing {
        return Err(ApiError::Conflict(format!(
            "prefix {prefix} is 'printing' and cannot be deleted"
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
    Path(prefix): Path<String>,
) -> Result<StatusCode, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    db::ensure_writable(&st.pool, prefix).await?;
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
    Path((prefix, id)): Path<(String, i64)>,
    Json(patch): Json<CardPatch>,
) -> Result<Json<Card>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    db::ensure_writable(&st.pool, prefix).await?;
    Ok(Json(db::update_card_meta(&st.pool, prefix, id, &patch).await?))
}

/// Submit a prefix for payment (draft -> submitted). Customer action.
#[utoipa::path(post, path = "/api/prefixes/{prefix}/submit", tag = "prefixes",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    responses(
        (status = 200, body = Prefix),
        (status = 404, body = crate::error::ErrorBody),
        (status = 409, description = "not in draft", body = crate::error::ErrorBody)
    ))]
pub async fn submit_prefix(
    State(st): State<AppState>,
    Path(prefix): Path<String>,
) -> Result<Json<Prefix>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    Ok(Json(
        db::set_status(&st.pool, prefix, PrefixStatus::Submitted, None).await?,
    ))
}

/// Transition a prefix's status (shop / print-queue side).
#[utoipa::path(patch, path = "/api/prefixes/{prefix}/status", tag = "prefixes",
    params(("prefix" = Uuid, Path, description = "prefix uuid")),
    request_body = StatusInput,
    responses(
        (status = 200, body = Prefix),
        (status = 404, body = crate::error::ErrorBody),
        (status = 409, description = "invalid transition", body = crate::error::ErrorBody)
    ))]
pub async fn patch_status(
    State(st): State<AppState>,
    Path(prefix): Path<String>,
    Json(input): Json<StatusInput>,
) -> Result<Json<Prefix>, ApiError> {
    let prefix = parse_uuid(&prefix)?;
    Ok(Json(
        db::set_status(&st.pool, prefix, input.status, input.order_ref.as_deref()).await?,
    ))
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/prefixes", axum::routing::get(list_prefixes).post(create_prefix))
        .route(
            "/api/prefixes/:prefix",
            axum::routing::get(get_prefix).delete(delete_prefix),
        )
        .route(
            "/api/prefixes/:prefix/submit",
            axum::routing::post(submit_prefix),
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
        .with_state(state)
}