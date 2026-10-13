use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::error::ApiError;
use crate::models::{
    Card, CardPatch, ModePrice, NeedsAttentionKind, PaymentAnomaly, Prefix, PrefixStatus,
    PrintMode, PrintQueue, PrintQueueCard,
};

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
        p.order_ref, p.print_mode, p.paid_print_mode,
        (SELECT COUNT(*) FROM cards c WHERE c.prefix_uuid = p.uuid) AS card_count
 FROM prefixes p";

/// Admin-only variant of `PREFIX_SELECT` that also counts unprinted cards, so
/// the print queue UI can tell a `printing` prefix that is a resume (still has
/// unprinted cards) from one that is done.
const ADMIN_PREFIX_SELECT: &str = "SELECT p.uuid, p.created_at, p.status, p.status_changed_at,
        p.order_ref, p.print_mode, p.paid_print_mode,
        (SELECT COUNT(*) FROM cards c WHERE c.prefix_uuid = p.uuid) AS card_count,
        (SELECT COUNT(*) FROM cards c WHERE c.prefix_uuid = p.uuid AND c.printed_at IS NULL)
            AS unprinted_count
 FROM prefixes p";

fn prefix_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Prefix, ApiError> {
    let status_raw: String = row.get("status");
    let status = PrefixStatus::parse(&status_raw)
        .ok_or_else(|| ApiError::Internal(format!("unknown prefix status '{status_raw}'")))?;
    let mode_raw: String = row.get("print_mode");
    let print_mode = PrintMode::parse(&mode_raw)
        .ok_or_else(|| ApiError::Internal(format!("unknown prefix print_mode '{mode_raw}'")))?;
    let paid_raw: Option<String> = row.get("paid_print_mode");
    let paid_print_mode =
        match paid_raw.as_deref() {
            None => None,
            Some(s) => Some(PrintMode::parse(s).ok_or_else(|| {
                ApiError::Internal(format!("unknown prefix paid_print_mode '{s}'"))
            })?),
        };
    Ok(Prefix {
        uuid: Uuid::parse_str(row.get::<String, _>("uuid").as_str())
            .map_err(|e| ApiError::Internal(e.to_string()))?,
        created_at: row.get("created_at"),
        status,
        status_changed_at: row.get("status_changed_at"),
        print_mode,
        paid_print_mode,
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

pub async fn create_prefix(
    pool: &SqlitePool,
    uuid: Option<Uuid>,
    print_mode: Option<PrintMode>,
) -> Result<Prefix, ApiError> {
    let uuid = uuid.unwrap_or_else(Uuid::new_v4);
    let print_mode = print_mode.unwrap_or(PrintMode::ColorFrontBlackBack);
    let res = sqlx::query(
        "INSERT INTO prefixes (uuid, status, status_changed_at, print_mode)
         VALUES (?1, 'draft', datetime('now'), ?2)",
    )
    .bind(uuid.to_string())
    .bind(print_mode.as_str())
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

/// Change a prefix's print mode. Idempotent: re-applying the current mode is a
/// no-op that still returns the prefix.
pub async fn set_prefix_mode(
    pool: &SqlitePool,
    uuid: Uuid,
    mode: PrintMode,
) -> Result<Prefix, ApiError> {
    let current = get_prefix(pool, uuid).await?;
    if current.print_mode != mode {
        sqlx::query("UPDATE prefixes SET print_mode = ?2 WHERE uuid = ?1")
            .bind(uuid.to_string())
            .bind(mode.as_str())
            .execute(pool)
            .await?;
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

/// Every print mode's price, ordered by mode, for the shop config.
pub async fn list_mode_prices(pool: &SqlitePool) -> Result<Vec<ModePrice>, ApiError> {
    let rows = sqlx::query("SELECT print_mode, price_cents FROM mode_prices ORDER BY print_mode")
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|r| {
            let mode_raw: String = r.get("print_mode");
            let print_mode = PrintMode::parse(&mode_raw).ok_or_else(|| {
                ApiError::Internal(format!("unknown print_mode '{mode_raw}' in mode_prices"))
            })?;
            Ok(ModePrice {
                print_mode,
                price_cents: r.get("price_cents"),
            })
        })
        .collect()
}

/// The price of one card in a given mode, in the currency's minor unit.
///
/// A mode with no configured price is a misconfiguration, not a free card, so
/// it surfaces as an error rather than silently pricing at zero.
pub async fn mode_price(pool: &SqlitePool, mode: PrintMode) -> Result<i64, ApiError> {
    let row = sqlx::query("SELECT price_cents FROM mode_prices WHERE print_mode = ?1")
        .bind(mode.as_str())
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| ApiError::Internal(format!("no price configured for print mode {mode}")))?;
    Ok(row.get("price_cents"))
}

/// Freeze the mode a prefix was charged for, so the spooler prints what the
/// customer paid for even if the operator later changes the prefix's mode.
pub async fn set_paid_print_mode(
    pool: &SqlitePool,
    uuid: Uuid,
    mode: PrintMode,
) -> Result<(), ApiError> {
    sqlx::query("UPDATE prefixes SET paid_print_mode = ?2 WHERE uuid = ?1")
        .bind(uuid.to_string())
        .bind(mode.as_str())
        .execute(pool)
        .await?;
    Ok(())
}

/// Create a prefix at a customer-chosen uuid, or return the existing one if it
/// is already there (idempotent `PUT /api/prefixes/{prefix}`). Returns
/// `(prefix, created)` so the caller can answer 201 vs 200.
pub async fn upsert_prefix(pool: &SqlitePool, uuid: Uuid) -> Result<(Prefix, bool), ApiError> {
    match create_prefix(pool, Some(uuid), None).await {
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
        back_image_content_type: row.get("back_image_content_type"),
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
///
/// A `back_image_path`/`back_image_content_type` of `None` clears any stored
/// colour back image. Which back fields are valid at all is enforced by the
/// route layer against the prefix's `print_mode` before this is called.
pub async fn upsert_card(
    pool: &SqlitePool,
    prefix: Uuid,
    id: i64,
    name: &str,
    serial_number: Option<&str>,
    back_text: Option<&str>,
    image_path: &str,
    image_content_type: &str,
    back_image_path: Option<&str>,
    back_image_content_type: Option<&str>,
) -> Result<Card, ApiError> {
    sqlx::query(
        "INSERT INTO cards
            (prefix_uuid, id, name, serial_number, back_text, image_path, image_content_type,
             back_image_path, back_image_content_type)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT (prefix_uuid, id) DO UPDATE SET
            name = excluded.name,
            serial_number = excluded.serial_number,
            back_text = excluded.back_text,
            image_path = excluded.image_path,
            image_content_type = excluded.image_content_type,
            back_image_path = excluded.back_image_path,
            back_image_content_type = excluded.back_image_content_type,
            updated_at = datetime('now')",
    )
    .bind(prefix.to_string())
    .bind(id)
    .bind(name)
    .bind(serial_number)
    .bind(back_text)
    .bind(image_path)
    .bind(image_content_type)
    .bind(back_image_path)
    .bind(back_image_content_type)
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
    let mode = get_prefix(pool, prefix).await?.print_mode;

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
            if col == "back_text" && !mode.allows_back_text() {
                return Err(ApiError::BadRequest(format!(
                    "back_text is only valid in a colour_front_black_back prefix, not {mode}"
                )));
            }
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
    // The spooler prints the mode that was paid for, not the prefix's current
    // one — an operator changing the mode after payment must not change what
    // gets printed. Prefixes never priced through checkout fall back to the
    // current mode.
    let print_mode = p.paid_print_mode.unwrap_or(p.print_mode);
    Ok(PrintQueue {
        prefix,
        status: p.status,
        print_mode,
        cards,
        unprinted_count,
        resume,
    })
}

// ---------------------------------------------------------------------------
// Payment anomalies — the dead ends behind the admin "needs attention" list
// ---------------------------------------------------------------------------

/// Record a dead-end payment event: a settled payment that could not attach to
/// a prefix (cancelled, draft, deleted, or already past printing). Written by
/// the webhook at the moment the dead end happens, so the "needs attention"
/// list is a query over this table rather than a scrape of the log.
pub async fn insert_payment_anomaly(
    pool: &SqlitePool,
    prefix: Uuid,
    kind: NeedsAttentionKind,
    message: &str,
    action: &str,
    session_id: Option<&str>,
    amount_cents: Option<i64>,
    currency: Option<&str>,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO payment_anomalies
            (prefix, kind, message, action, session_id, amount_cents, currency)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(prefix.to_string())
    .bind(kind.as_str())
    .bind(message)
    .bind(action)
    .bind(session_id)
    .bind(amount_cents)
    .bind(currency)
    .execute(pool)
    .await?;
    Ok(())
}

fn anomaly_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<PaymentAnomaly, ApiError> {
    let kind_raw: String = row.get("kind");
    let kind = NeedsAttentionKind::parse(&kind_raw)
        .ok_or_else(|| ApiError::Internal(format!("unknown anomaly kind '{kind_raw}'")))?;
    Ok(PaymentAnomaly {
        id: row.get("id"),
        prefix: Uuid::parse_str(row.get::<String, _>("prefix").as_str())
            .map_err(|e| ApiError::Internal(e.to_string()))?,
        kind,
        message: row.get("message"),
        action: row.get("action"),
        session_id: row.get("session_id"),
        amount_cents: row.get("amount_cents"),
        currency: row.get("currency"),
        created_at: row.get("created_at"),
    })
}

/// Every recorded dead-end payment event, newest first.
pub async fn list_payment_anomalies(pool: &SqlitePool) -> Result<Vec<PaymentAnomaly>, ApiError> {
    let rows = sqlx::query("SELECT * FROM payment_anomalies ORDER BY created_at DESC, id DESC")
        .fetch_all(pool)
        .await?;
    rows.iter().map(anomaly_from_row).collect()
}

/// Prefixes left in `payment_pending` for more than `older_than_secs`.
///
/// The checkout was abandoned (or its expiry never reached us), nothing was
/// ever paid, and no work queue will touch it — so it is invisible to the
/// normal flow and needs a human. This is the half of the "needs attention"
/// list that needs no webhook to record it: it falls straight out of the
/// prefixes table. `status_changed_at` is stored as SQLite `datetime('now')`,
/// so staleness is computed with `strftime('%s', ...)` in the same units.
pub async fn abandoned_payment_pending(
    pool: &SqlitePool,
    older_than_secs: i64,
) -> Result<Vec<Prefix>, ApiError> {
    let rows = sqlx::query(&format!(
        "{PREFIX_SELECT}
         WHERE p.status = ?1
           AND CAST(strftime('%s', p.status_changed_at) AS INTEGER)
                 < (CAST(strftime('%s', 'now') AS INTEGER) - ?2)
         ORDER BY p.status_changed_at ASC"
    ))
    .bind(PrefixStatus::PaymentPending.as_str())
    .bind(older_than_secs)
    .fetch_all(pool)
    .await?;
    rows.iter().map(prefix_from_row).collect()
}

// ---------------------------------------------------------------------------
// Email notifications — the claim that makes each one exactly-once
// ---------------------------------------------------------------------------

/// Claim the right to send one notification, keyed on the event that caused it
/// (`<kind>:<stripe session id>` for the paid confirmation).
///
/// Returns `true` when this caller won the claim and should send, `false` when
/// the row was already there — a retried or concurrent webhook delivery, or a
/// replayed event. The insert is the check and the write in one statement, so
/// two deliveries racing cannot both win it, and the claim survives a restart
/// in the middle of Stripe's retries because it is a database key.
pub async fn claim_notification(
    pool: &SqlitePool,
    dedupe_key: &str,
    kind: &str,
    prefix: Uuid,
    recipient: &str,
    session_id: Option<&str>,
) -> Result<bool, ApiError> {
    let res = sqlx::query(
        "INSERT INTO email_notifications (dedupe_key, kind, prefix, recipient, session_id)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (dedupe_key) DO NOTHING",
    )
    .bind(dedupe_key)
    .bind(kind)
    .bind(prefix.to_string())
    .bind(recipient)
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Record how a claimed notification ended: sent (with the provider's id) or
/// failed (with the reason).
///
/// The claim row is never deleted. It is the record that this transition has
/// been mailed and what came of it, and a *failed* row keeps its claim on
/// purpose: a resend is an operator decision, not something an automatic retry
/// should risk doubling.
pub async fn finish_notification(
    pool: &SqlitePool,
    dedupe_key: &str,
    sent: bool,
    provider_id: Option<&str>,
    error: Option<&str>,
) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE email_notifications
            SET status = ?2,
                provider_id = COALESCE(?3, provider_id),
                error = ?4,
                sent_at = CASE WHEN ?2 = 'sent' THEN datetime('now') ELSE sent_at END
          WHERE dedupe_key = ?1",
    )
    .bind(dedupe_key)
    .bind(if sent { "sent" } else { "failed" })
    .bind(provider_id)
    .bind(error)
    .execute(pool)
    .await?;
    Ok(())
}

/// Test-only: the dedupe keys recorded as sent, so a test can prove a replayed
/// transition produced no second message.
#[cfg(test)]
pub async fn sent_notification_keys(pool: &SqlitePool) -> Result<Vec<String>, ApiError> {
    let rows = sqlx::query("SELECT dedupe_key FROM email_notifications WHERE status = 'sent'")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| r.get("dedupe_key")).collect())
}

/// Test-only: `(recipient, error)` of every notification whose send failed.
#[cfg(test)]
pub async fn failed_notifications(pool: &SqlitePool) -> Result<Vec<(String, String)>, ApiError> {
    let rows = sqlx::query(
        "SELECT recipient, error FROM email_notifications WHERE status = 'failed' ORDER BY rowid",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| (r.get("recipient"), r.get("error")))
        .collect())
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
        let err = create_prefix(&pool, Some(uuid), None)
            .await
            .expect_err("a bare POST create must still 409 on a duplicate uuid");
        assert!(matches!(err, ApiError::Conflict(_)));
    }

    #[test]
    fn print_mode_parse_round_trips_all_modes() {
        for (text, mode) in [
            ("single_sided", PrintMode::SingleSided),
            ("double_sided_color", PrintMode::DoubleSidedColor),
            ("color_front_black_back", PrintMode::ColorFrontBlackBack),
        ] {
            assert_eq!(PrintMode::parse(text), Some(mode));
            assert_eq!(mode.as_str(), text);
        }
        assert_eq!(PrintMode::parse("colour_front_black"), None);
    }

    #[test]
    fn print_mode_back_field_capabilities() {
        // Only the black-back mode renders markdown back text.
        assert!(PrintMode::ColorFrontBlackBack.allows_back_text());
        assert!(!PrintMode::SingleSided.allows_back_text());
        assert!(!PrintMode::DoubleSidedColor.allows_back_text());

        // Only double-sided colour takes a colour back image.
        assert!(PrintMode::DoubleSidedColor.allows_back_image());
        assert!(!PrintMode::SingleSided.allows_back_image());
        assert!(!PrintMode::ColorFrontBlackBack.allows_back_image());
    }

    #[tokio::test]
    async fn prefix_defaults_to_color_front_black_back() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        assert_eq!(p.print_mode, PrintMode::ColorFrontBlackBack);
    }

    #[tokio::test]
    async fn prefix_accepts_an_explicit_mode() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(PrintMode::SingleSided))
            .await
            .expect("create");
        assert_eq!(p.print_mode, PrintMode::SingleSided);
    }

    #[tokio::test]
    async fn set_prefix_mode_is_idempotent_and_persists() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        let switched = set_prefix_mode(&pool, p.uuid, PrintMode::DoubleSidedColor)
            .await
            .expect("switch");
        assert_eq!(switched.print_mode, PrintMode::DoubleSidedColor);
        // Re-applying is a no-op that still returns the prefix.
        let again = set_prefix_mode(&pool, p.uuid, PrintMode::DoubleSidedColor)
            .await
            .expect("switch again");
        assert_eq!(again.print_mode, PrintMode::DoubleSidedColor);
    }

    #[tokio::test]
    async fn back_image_content_type_round_trips_on_card() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(PrintMode::DoubleSidedColor))
            .await
            .expect("create");
        let card = upsert_card(
            &pool,
            p.uuid,
            1,
            "front",
            None,
            None, // back_text is meaningless in colour mode
            "blobs/1.png",
            "image/png",
            Some("blobs/1-back.png"),
            Some("image/png"),
        )
        .await
        .expect("upsert");
        assert_eq!(card.image_content_type, "image/png");
        assert_eq!(card.back_image_content_type.as_deref(), Some("image/png"));
        assert!(card.back_text.is_none());
    }

    #[tokio::test]
    async fn non_black_back_mode_keeps_back_text_field_null() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(PrintMode::SingleSided))
            .await
            .expect("create");
        let card = upsert_card(
            &pool,
            p.uuid,
            1,
            "front",
            None,
            None,
            "blobs/1.png",
            "image/png",
            None,
            None,
        )
        .await
        .expect("upsert");
        assert!(card.back_image_content_type.is_none());
    }

    #[tokio::test]
    async fn abandoned_payment_pending_flags_only_stale_prefixes() {
        let pool = test_pool().await;
        let old = create_prefix(&pool, None, None).await.expect("create");
        let fresh = create_prefix(&pool, None, None).await.expect("create");
        set_status(&pool, old.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .expect("pending");
        set_status(&pool, fresh.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .expect("pending");

        // Backdate the old one's status beyond the 24h abbandoned window.
        sqlx::query(
            "UPDATE prefixes SET status_changed_at = datetime('now', '-25 hours') WHERE uuid = ?1",
        )
        .bind(old.uuid.to_string())
        .execute(&pool)
        .await
        .expect("backdate");

        let stuck = abandoned_payment_pending(&pool, 24 * 3600)
            .await
            .expect("query");
        assert_eq!(stuck.len(), 1, "only the stale prefix is abandoned");
        assert_eq!(stuck[0].uuid, old.uuid);
    }

    #[tokio::test]
    async fn abandoned_payment_pending_ignores_fresh_and_non_pending_prefixes() {
        let pool = test_pool().await;
        // Fresh pending — the customer may still be at the checkout.
        let fresh = create_prefix(&pool, None, None).await.expect("create");
        set_status(&pool, fresh.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .expect("pending");
        // A paid prefix, however stale, is not abandoned.
        let paid = create_prefix(&pool, None, None).await.expect("create");
        set_status(&pool, paid.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .expect("pending");
        set_status(&pool, paid.uuid, PrefixStatus::Paid, Some("cs_1"), false)
            .await
            .expect("paid");
        sqlx::query(
            "UPDATE prefixes SET status_changed_at = datetime('now', '-25 hours') WHERE uuid = ?1",
        )
        .bind(paid.uuid.to_string())
        .execute(&pool)
        .await
        .expect("backdate");

        let stuck = abandoned_payment_pending(&pool, 24 * 3600)
            .await
            .expect("query");
        assert!(stuck.is_empty());
    }

    #[tokio::test]
    async fn payment_anomaly_round_trips() {
        let pool = test_pool().await;
        let prefix = Uuid::new_v4();
        insert_payment_anomaly(
            &pool,
            prefix,
            NeedsAttentionKind::PaymentForDeleted,
            "a settled payment for a prefix that has since been deleted",
            "refund_or_rebuild_by_hand",
            Some("cs_1"),
            Some(1750),
            Some("eur"),
        )
        .await
        .expect("insert");

        let rows = list_payment_anomalies(&pool).await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prefix, prefix);
        assert_eq!(rows[0].kind, NeedsAttentionKind::PaymentForDeleted);
        assert_eq!(rows[0].action, "refund_or_rebuild_by_hand");
        assert_eq!(rows[0].session_id.as_deref(), Some("cs_1"));
        assert_eq!(rows[0].amount_cents, Some(1750));
        assert_eq!(rows[0].currency.as_deref(), Some("eur"));
    }

    #[tokio::test]
    async fn mode_prices_are_seeded_for_every_mode() {
        let pool = test_pool().await;
        let prices = list_mode_prices(&pool).await.expect("list");
        // The migration seeds every mode at the historical default of 500.
        assert_eq!(prices.len(), 3);
        for p in &prices {
            assert_eq!(
                p.price_cents, 500,
                "mode {:?} should default to 500",
                p.print_mode
            );
        }
        let modes: Vec<_> = prices.iter().map(|p| p.print_mode).collect();
        assert!(modes.contains(&PrintMode::SingleSided));
        assert!(modes.contains(&PrintMode::DoubleSidedColor));
        assert!(modes.contains(&PrintMode::ColorFrontBlackBack));
    }

    #[tokio::test]
    async fn mode_price_lookup_returns_the_configured_price() {
        let pool = test_pool().await;
        assert_eq!(
            mode_price(&pool, PrintMode::ColorFrontBlackBack)
                .await
                .expect("price"),
            500
        );
        assert_eq!(
            mode_price(&pool, PrintMode::SingleSided)
                .await
                .expect("price"),
            500
        );
    }

    #[tokio::test]
    async fn paid_print_mode_is_snapshotted_and_used_by_the_print_queue() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(PrintMode::ColorFrontBlackBack))
            .await
            .expect("create");
        // A prefix never priced through checkout has no paid mode; the queue
        // falls back to the current mode.
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.print_mode, PrintMode::ColorFrontBlackBack);

        // Freeze the mode as if a checkout had priced it.
        set_paid_print_mode(&pool, p.uuid, PrintMode::DoubleSidedColor)
            .await
            .expect("freeze");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.print_mode, PrintMode::DoubleSidedColor);

        // Even if the operator later changes the prefix's current mode, the
        // queue still reports the paid-for one.
        set_prefix_mode(&pool, p.uuid, PrintMode::SingleSided)
            .await
            .expect("operator changes mode");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.print_mode, PrintMode::DoubleSidedColor);
    }

    #[tokio::test]
    async fn a_notification_claim_is_won_once() {
        let pool = test_pool().await;
        let prefix = Uuid::new_v4();
        let key = "customer_order_paid:cs_1";

        assert!(claim_notification(
            &pool,
            key,
            "customer_order_paid",
            prefix,
            "ada@example.test",
            Some("cs_1"),
        )
        .await
        .expect("first claim"));
        // The same key again — a retried delivery — must not be claimable.
        assert!(!claim_notification(
            &pool,
            key,
            "customer_order_paid",
            prefix,
            "ada@example.test",
            Some("cs_1"),
        )
        .await
        .expect("second claim"));

        // A different kind or a different session is a different message.
        assert!(claim_notification(
            &pool,
            "operator_order_paid:cs_1",
            "operator_order_paid",
            prefix,
            "shop@example.test",
            Some("cs_1"),
        )
        .await
        .expect("operator claim"));

        finish_notification(&pool, key, true, Some("re_1"), None)
            .await
            .expect("finish");
        assert_eq!(
            sent_notification_keys(&pool).await.unwrap(),
            vec![key.to_string()]
        );
        assert!(failed_notifications(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_failed_notification_keeps_its_claim_and_records_why() {
        let pool = test_pool().await;
        let key = "customer_order_paid:cs_2";
        claim_notification(
            &pool,
            key,
            "customer_order_paid",
            Uuid::new_v4(),
            "ada@example.test",
            Some("cs_2"),
        )
        .await
        .expect("claim");
        finish_notification(
            &pool,
            key,
            false,
            None,
            Some("Resend refused the message (422)"),
        )
        .await
        .expect("finish");

        let failed = failed_notifications(&pool).await.unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, "ada@example.test");
        assert_eq!(failed[0].1, "Resend refused the message (422)");
        // A failed send does not free the claim: resending is a human's call.
        assert!(!claim_notification(
            &pool,
            key,
            "customer_order_paid",
            Uuid::new_v4(),
            "ada@example.test",
            Some("cs_2"),
        )
        .await
        .expect("reclaim"));
    }
}
