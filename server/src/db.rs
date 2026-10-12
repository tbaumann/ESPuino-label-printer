use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::error::ApiError;
use crate::models::{Card, CardPatch, Prefix, PrefixStatus, PrintQueue, PrintQueueCard};

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

/// Admin-only variant of `PREFIX_SELECT` that also counts unprinted cards, so
/// the print queue UI can tell a `printing` prefix that is a resume (still has
/// unprinted cards) from one that is done.
const ADMIN_PREFIX_SELECT: &str = "SELECT p.uuid, p.created_at, p.status, p.status_changed_at,
        p.order_ref,
        (SELECT COUNT(*) FROM cards c WHERE c.prefix_uuid = p.uuid) AS card_count,
        (SELECT COUNT(*) FROM cards c WHERE c.prefix_uuid = p.uuid AND c.printed_at IS NULL)
            AS unprinted_count
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
        // The public view never carries operator-only print state.
        unprinted_count: None,
    })
}

/// Like `prefix_from_row`, but for the admin list: carries `unprinted_count`.
fn admin_prefix_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Prefix, ApiError> {
    let mut p = prefix_from_row(row)?;
    p.unprinted_count = Some(row.get("unprinted_count"));
    Ok(p)
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

/// Create a prefix at a customer-chosen uuid, or return the existing one if it
/// is already there (idempotent `PUT /api/prefixes/{prefix}`). Returns
/// `(prefix, created)` so the caller can answer 201 vs 200.
pub async fn upsert_prefix(pool: &SqlitePool, uuid: Uuid) -> Result<(Prefix, bool), ApiError> {
    match create_prefix(pool, Some(uuid)).await {
        Ok(p) => Ok((p, true)),
        // Already exists: an idempotent PUT returns it rather than 409ing.
        Err(ApiError::Conflict(_)) => Ok((get_prefix(pool, uuid).await?, false)),
        Err(e) => Err(e),
    }
}

/// List prefixes, optionally filtered by status (e.g. `paid` for the print queue).
/// Admin-only: carries the operator's `unprinted_count` per prefix.
pub async fn list_prefixes(
    pool: &SqlitePool,
    status: Option<PrefixStatus>,
) -> Result<Vec<Prefix>, ApiError> {
    let rows = sqlx::query(&format!(
        "{ADMIN_PREFIX_SELECT} WHERE (?1 IS NULL OR p.status = ?1) ORDER BY p.created_at DESC"
    ))
    .bind(status.map(|s| s.as_str().to_string()))
    .fetch_all(pool)
    .await?;
    rows.iter().map(admin_prefix_from_row).collect()
}

/// Move a prefix to a new status. Re-applying the current status is a no-op
/// (idempotent, so shop/queue callbacks can retry).
///
/// `force` skips transition validation — used for admin writes, which may move
/// a prefix to any status at any time (e.g. requeue a stuck `printing` order,
/// which has no legal edge back).
pub async fn set_status(
    pool: &SqlitePool,
    uuid: Uuid,
    next: PrefixStatus,
    order_ref: Option<&str>,
    force: bool,
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
    if !force && !current.status.can_transition_to(next) {
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

/// Reject card mutations on a sealed prefix, unless the caller is the
/// operator.
///
/// A customer may only write while the prefix is a draft; the operator may
/// write in any state up to and including `queued` (and even while printing,
/// which is what makes a stuck print job recoverable).
pub async fn ensure_writable(
    pool: &SqlitePool,
    uuid: Uuid,
    admin: bool,
) -> Result<Prefix, ApiError> {
    let prefix = get_prefix(pool, uuid).await?;
    let writable = if admin {
        prefix.status.is_writable()
    } else {
        prefix.status.is_customer_writable()
    };
    if !writable {
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
        printed_at: row.get("printed_at"),
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
    let row =
        sqlx::query("SELECT COALESCE(MAX(id), 0) + 1 AS next_id FROM cards WHERE prefix_uuid = ?1")
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

/// Mark a card printed, once. The spooler calls this only after the printer
/// confirms the card was ejected.
///
/// Idempotent on purpose: the `AND printed_at IS NULL` guard means a second
/// call is a no-op, so a retried delivery can never print a card twice. (The
/// spooler walks *unprinted* cards, so marking at queue time would silently
/// skip a card if the process died in between — that is why the mark happens
/// here, after ejection, and not when the card is queued.)
pub async fn mark_card_printed(pool: &SqlitePool, prefix: Uuid, id: i64) -> Result<Card, ApiError> {
    get_card(pool, prefix, id).await?; // 404 if the card is unknown
    sqlx::query(
        "UPDATE cards SET printed_at = datetime('now')
          WHERE prefix_uuid = ?1 AND id = ?2 AND printed_at IS NULL",
    )
    .bind(prefix.to_string())
    .bind(id)
    .execute(pool)
    .await?;
    get_card(pool, prefix, id).await
}

/// The print queue for one prefix: every card ordered explicitly by `id` (not
/// the database's natural order), with the operator-only `printed_at`.
///
/// The spooler walks this to know where to resume after a fault. `resume` is
/// true when the prefix is `printing` and still has unprinted cards — a
/// half-finished order that can be picked up where it left off.
pub async fn print_queue(pool: &SqlitePool, prefix: Uuid) -> Result<PrintQueue, ApiError> {
    let p = get_prefix(pool, prefix).await?;
    let rows = sqlx::query(
        "SELECT id, name, printed_at FROM cards
          WHERE prefix_uuid = ?1 ORDER BY id ASC",
    )
    .bind(prefix.to_string())
    .fetch_all(pool)
    .await?;
    let cards = rows
        .iter()
        .map(|r| PrintQueueCard {
            id: r.get("id"),
            name: r.get("name"),
            printed_at: r.get("printed_at"),
        })
        .collect::<Vec<_>>();
    let unprinted_count = cards.iter().filter(|c| c.printed_at.is_none()).count() as i64;
    let resume = p.status == PrefixStatus::Printing && unprinted_count > 0;
    Ok(PrintQueue {
        prefix,
        status: p.status,
        cards,
        unprinted_count,
        resume,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[tokio::test]
    async fn upsert_creates_and_is_idempotent() {
        let pool = test_pool().await;
        let uuid = Uuid::new_v4();

        // First PUT creates.
        let (p, created) = upsert_prefix(&pool, uuid).await.expect("first upsert");
        assert!(created);
        assert_eq!(p.uuid, uuid);
        assert_eq!(p.status, PrefixStatus::Draft);

        // Second PUT is a no-op, not an error, and returns the same prefix.
        let (p2, created2) = upsert_prefix(&pool, uuid).await.expect("second upsert");
        assert!(!created2);
        assert_eq!(p2.uuid, uuid);
        assert_eq!(p2.status, PrefixStatus::Draft);
    }

    #[tokio::test]
    async fn upsert_does_not_disturb_an_existing_prefix() {
        let pool = test_pool().await;
        let uuid = Uuid::new_v4();
        upsert_prefix(&pool, uuid).await.expect("create");

        // Shuffle the status (only legal from draft in a real transition, so go
        // via the transition table) — the point is that a repeat upsert must not
        // reset it back to draft.
        set_status(&pool, uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .expect("transition to payment_pending");

        let (p, created) = upsert_prefix(&pool, uuid).await.expect("repeat upsert");
        assert!(!created);
        // Force must stay false here: set_status was called non-force above.
        assert_eq!(p.status, PrefixStatus::PaymentPending);
    }

    #[tokio::test]
    async fn create_prefix_still_conflicts_on_duplicate() {
        let pool = test_pool().await;
        let uuid = Uuid::new_v4();
        upsert_prefix(&pool, uuid).await.expect("first create");
        let err = create_prefix(&pool, Some(uuid)).await.expect_err(
            "a bare POST create must still 409 on a duplicate uuid",
        );
        assert!(matches!(err, ApiError::Conflict(_)));
    }
}
