use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::error::ApiError;
use crate::models::{Card, CardPatch, Prefix, PrefixStatus};

/// Open (creating if needed) the SQLite database and run migrations.
pub async fn init_pool(database_url: &str) -> anyhow::Result<SqlitePool> {
    if let Some(path) = database_url.strip_prefix("sqlite://") {
        let path = path.split('?').next().unwrap_or(path);
        if let Some(parent) = Path::new(path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
    }

    let opts: SqliteConnectOptions = database_url
        .parse::<SqliteConnectOptions>()?
        .create_if_missing(true)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;

    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

const PREFIX_SELECT: &str = "SELECT p.uuid, p.created_at, p.status, p.status_changed_at,
        p.order_ref,
        (SELECT COUNT(*) FROM cards c WHERE c.prefix_uuid = p.uuid) AS card_count
 FROM prefixes p";

fn prefix_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Prefix, ApiError> {
    let status_raw: String = row.get("status");
    let status = PrefixStatus::parse(&status_raw)
        .ok_or_else(|| ApiError::Internal(format!("unknown prefix status '{status_raw}'")))?;
    Ok(Prefix {
        uuid: Uuid::parse_str(row.get::<String, _>("uuid").as_str())
            .map_err(|e| ApiError::Internal(e.to_string()))?,
        created_at: row.get("created_at"),
        status,
        status_changed_at: row.get("status_changed_at"),
        order_ref: row.get("order_ref"),
        card_count: row.get("card_count"),
    })
}

pub async fn create_prefix(pool: &SqlitePool, uuid: Option<Uuid>) -> Result<Prefix, ApiError> {
    let uuid = uuid.unwrap_or_else(Uuid::new_v4);
    let res = sqlx::query(
        "INSERT INTO prefixes (uuid, status, status_changed_at)
         VALUES (?1, 'draft', datetime('now'))",
    )
    .bind(uuid.to_string())
    .execute(pool)
    .await;

    if let Err(e) = res {
        if let sqlx::Error::Database(db) = &e {
            if db.is_unique_violation() {
                return Err(ApiError::Conflict(format!("prefix {uuid} already exists")));
            }
        }
        return Err(e.into());
    }
    get_prefix(pool, uuid).await
}

pub async fn get_prefix(pool: &SqlitePool, uuid: Uuid) -> Result<Prefix, ApiError> {
    let row = sqlx::query(&format!("{PREFIX_SELECT} WHERE p.uuid = ?1"))
        .bind(uuid.to_string())
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("prefix {uuid}")))?;
    prefix_from_row(&row)
}

/// List prefixes, optionally filtered by status (e.g. `paid` for the print queue).
pub async fn list_prefixes(
    pool: &SqlitePool,
    status: Option<PrefixStatus>,
) -> Result<Vec<Prefix>, ApiError> {
    let rows = sqlx::query(&format!(
        "{PREFIX_SELECT} WHERE (?1 IS NULL OR p.status = ?1) ORDER BY p.created_at DESC"
    ))
    .bind(status.map(|s| s.as_str().to_string()))
    .fetch_all(pool)
    .await?;
    rows.iter().map(prefix_from_row).collect()
}

/// Move a prefix to a new status, validating the transition. Re-applying the
/// current status is a no-op (idempotent, so shop/queue callbacks can retry).
pub async fn set_status(
    pool: &SqlitePool,
    uuid: Uuid,
    next: PrefixStatus,
    order_ref: Option<&str>,
) -> Result<Prefix, ApiError> {
    let current = get_prefix(pool, uuid).await?;
    if current.status == next {
        // Still allow attaching an order_ref without a status change.
        if let Some(r) = order_ref {
            if current.order_ref.as_deref() != Some(r) {
                sqlx::query("UPDATE prefixes SET order_ref = ?2 WHERE uuid = ?1")
                    .bind(uuid.to_string())
                    .bind(r)
                    .execute(pool)
                    .await?;
                return get_prefix(pool, uuid).await;
            }
        }
        return Ok(current);
    }
    if !current.status.can_transition_to(next) {
        return Err(ApiError::Conflict(format!(
            "cannot transition prefix {uuid} from {} to {}",
            current.status, next
        )));
    }
    sqlx::query(
        "UPDATE prefixes
            SET status = ?2,
                status_changed_at = datetime('now'),
                order_ref = COALESCE(?3, order_ref)
          WHERE uuid = ?1",
    )
    .bind(uuid.to_string())
    .bind(next.as_str())
    .bind(order_ref)
    .execute(pool)
    .await?;
    get_prefix(pool, uuid).await
}

/// Reject card mutations on a frozen prefix (printing or later).
pub async fn ensure_writable(pool: &SqlitePool, uuid: Uuid) -> Result<Prefix, ApiError> {
    let prefix = get_prefix(pool, uuid).await?;
    if !prefix.status.is_writable() {
        return Err(ApiError::Conflict(format!(
            "prefix {uuid} is '{}' and can no longer be modified",
            prefix.status
        )));
    }
    Ok(prefix)
}

fn card_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Card, ApiError> {
    Ok(Card {
        id: row.get("id"),
        name: row.get("name"),
        serial_number: row.get("serial_number"),
        back_text: row.get("back_text"),
        image_content_type: row.get("image_content_type"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

pub async fn list_cards(pool: &SqlitePool, prefix: Uuid) -> Result<Vec<Card>, ApiError> {
    let rows = sqlx::query("SELECT * FROM cards WHERE prefix_uuid = ?1 ORDER BY id ASC")
        .bind(prefix.to_string())
        .fetch_all(pool)
        .await?;
    rows.iter().map(card_from_row).collect()
}

pub async fn get_card(pool: &SqlitePool, prefix: Uuid, id: i64) -> Result<Card, ApiError> {
    let row = sqlx::query("SELECT * FROM cards WHERE prefix_uuid = ?1 AND id = ?2")
        .bind(prefix.to_string())
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| ApiError::NotFound(format!("card {id} in prefix {prefix}")))?;
    card_from_row(&row)
}

/// Next free card id within a prefix (1-based).
pub async fn next_card_id(pool: &SqlitePool, prefix: Uuid) -> Result<i64, ApiError> {
    let row = sqlx::query(
        "SELECT COALESCE(MAX(id), 0) + 1 AS next_id FROM cards WHERE prefix_uuid = ?1",
    )
    .bind(prefix.to_string())
    .fetch_one(pool)
    .await?;
    Ok(row.get("next_id"))
}

/// Insert or replace a card. Mirrors a PUT semantics on (prefix, id).
pub async fn upsert_card(
    pool: &SqlitePool,
    prefix: Uuid,
    id: i64,
    name: &str,
    serial_number: Option<&str>,
    back_text: Option<&str>,
    image_path: &str,
    image_content_type: &str,
) -> Result<Card, ApiError> {
    sqlx::query(
        "INSERT INTO cards
            (prefix_uuid, id, name, serial_number, back_text, image_path, image_content_type)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (prefix_uuid, id) DO UPDATE SET
            name = excluded.name,
            serial_number = excluded.serial_number,
            back_text = excluded.back_text,
            image_path = excluded.image_path,
            image_content_type = excluded.image_content_type,
            updated_at = datetime('now')",
    )
    .bind(prefix.to_string())
    .bind(id)
    .bind(name)
    .bind(serial_number)
    .bind(back_text)
    .bind(image_path)
    .bind(image_content_type)
    .execute(pool)
    .await?;

    get_card(pool, prefix, id).await
}

/// Update a card's metadata without touching its image.
pub async fn update_card_meta(
    pool: &SqlitePool,
    prefix: Uuid,
    id: i64,
    patch: &CardPatch,
) -> Result<Card, ApiError> {
    get_card(pool, prefix, id).await?; // 404 if the card is unknown

    if let Some(raw) = patch.name.as_deref() {
        let name = raw.trim();
        if name.is_empty() {
            return Err(ApiError::BadRequest("name must not be empty".into()));
        }
        sqlx::query(
            "UPDATE cards SET name = ?3, updated_at = datetime('now')
              WHERE prefix_uuid = ?1 AND id = ?2",
        )
        .bind(prefix.to_string())
        .bind(id)
        .bind(name)
        .execute(pool)
        .await?;
    }

    // Column names come from this fixed list, so interpolating them is safe.
    for (col, value) in [
        ("serial_number", patch.serial_number.as_deref()),
        ("back_text", patch.back_text.as_deref()),
    ] {
        if let Some(v) = value {
            let v = if v.trim().is_empty() { None } else { Some(v) };
            sqlx::query(&format!(
                "UPDATE cards SET {col} = ?3, updated_at = datetime('now')
                  WHERE prefix_uuid = ?1 AND id = ?2"
            ))
            .bind(prefix.to_string())
            .bind(id)
            .bind(v)
            .execute(pool)
            .await?;
        }
    }

    get_card(pool, prefix, id).await
}

/// Delete a prefix together with all of its cards. Returns cards removed.
pub async fn delete_prefix(pool: &SqlitePool, uuid: Uuid) -> Result<i64, ApiError> {
    let mut tx = pool.begin().await?;
    let removed = sqlx::query("DELETE FROM cards WHERE prefix_uuid = ?1")
        .bind(uuid.to_string())
        .execute(&mut *tx)
        .await?
        .rows_affected() as i64;
    let gone = sqlx::query("DELETE FROM prefixes WHERE uuid = ?1")
        .bind(uuid.to_string())
        .execute(&mut *tx)
        .await?;
    if gone.rows_affected() == 0 {
        return Err(ApiError::NotFound(format!("prefix {uuid}")));
    }
    tx.commit().await?;
    Ok(removed)
}

/// Remove every card in a prefix, keeping the prefix itself.
pub async fn delete_all_cards(pool: &SqlitePool, prefix: Uuid) -> Result<i64, ApiError> {
    let removed = sqlx::query("DELETE FROM cards WHERE prefix_uuid = ?1")
        .bind(prefix.to_string())
        .execute(pool)
        .await?
        .rows_affected() as i64;
    Ok(removed)
}

pub async fn delete_card(pool: &SqlitePool, prefix: Uuid, id: i64) -> Result<(), ApiError> {
    let res = sqlx::query("DELETE FROM cards WHERE prefix_uuid = ?1 AND id = ?2")
        .bind(prefix.to_string())
        .bind(id)
        .execute(pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ApiError::NotFound(format!("card {id} in prefix {prefix}")));
    }
    Ok(())
}