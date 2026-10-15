use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::error::ApiError;
use crate::models::{
    BackAvailability, BackDepth, BackSource, Card, CardPatch, NeedsAttentionKind, PaymentAnomaly,
    Prefix, PrefixStatus, PrintQueue, PrintQueueCard, RenderStatus, Ribbon, RibbonPrice,
    RibbonStock,
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
        p.order_ref, p.ribbon, p.paid_ribbon,
        (SELECT COUNT(*) FROM cards c WHERE c.prefix_uuid = p.uuid) AS card_count
 FROM prefixes p";

/// Admin-only variant of `PREFIX_SELECT` that also counts unprinted cards, so
/// the print queue UI can tell a `printing` prefix that is a resume (still has
/// unprinted cards) from one that is done.
const ADMIN_PREFIX_SELECT: &str = "SELECT p.uuid, p.created_at, p.status, p.status_changed_at,
        p.order_ref, p.ribbon, p.paid_ribbon,
        (SELECT COUNT(*) FROM cards c WHERE c.prefix_uuid = p.uuid) AS card_count,
        (SELECT COUNT(*) FROM cards c WHERE c.prefix_uuid = p.uuid AND c.printed_at IS NULL)
            AS unprinted_count
 FROM prefixes p";

fn prefix_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Prefix, ApiError> {
    let status_raw: String = row.get("status");
    let status = PrefixStatus::parse(&status_raw)
        .ok_or_else(|| ApiError::Internal(format!("unknown prefix status '{status_raw}'")))?;
    let ribbon_raw: String = row.get("ribbon");
    let ribbon = Ribbon::parse(&ribbon_raw)
        .ok_or_else(|| ApiError::Internal(format!("unknown prefix ribbon '{ribbon_raw}'")))?;
    let paid_raw: Option<String> = row.get("paid_ribbon");
    let paid_ribbon = match paid_raw.as_deref() {
        None => None,
        Some(s) => Some(
            Ribbon::parse(s)
                .ok_or_else(|| ApiError::Internal(format!("unknown prefix paid_ribbon '{s}'")))?,
        ),
    };
    Ok(Prefix {
        uuid: Uuid::parse_str(row.get::<String, _>("uuid").as_str())
            .map_err(|e| ApiError::Internal(e.to_string()))?,
        created_at: row.get("created_at"),
        status,
        status_changed_at: row.get("status_changed_at"),
        ribbon,
        paid_ribbon,
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
    ribbon: Option<Ribbon>,
) -> Result<Prefix, ApiError> {
    let uuid = uuid.unwrap_or_else(Uuid::new_v4);
    let ribbon = ribbon.unwrap_or(Ribbon::Ymckoko);
    let res = sqlx::query(
        "INSERT INTO prefixes (uuid, status, status_changed_at, ribbon)
         VALUES (?1, 'draft', datetime('now'), ?2)",
    )
    .bind(uuid.to_string())
    .bind(ribbon.as_str())
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

/// Change a prefix's ribbon. Idempotent: re-applying the current ribbon is a
/// no-op that still returns the prefix.
///
/// Deliberately not guarded here — whether the prefix's *cards* can be printed
/// on the new ribbon is [`cards_incompatible_with_ribbon`], which the route
/// checks first so a refusal happens before anything is written.
pub async fn set_prefix_ribbon(
    pool: &SqlitePool,
    uuid: Uuid,
    ribbon: Ribbon,
) -> Result<Prefix, ApiError> {
    let current = get_prefix(pool, uuid).await?;
    if current.ribbon != ribbon {
        sqlx::query("UPDATE prefixes SET ribbon = ?2 WHERE uuid = ?1")
            .bind(uuid.to_string())
            .bind(ribbon.as_str())
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

/// Every ribbon's stock flag, ordered by ribbon, for the shop config.
pub async fn list_ribbon_stock(pool: &SqlitePool) -> Result<Vec<RibbonStock>, ApiError> {
    let rows = sqlx::query("SELECT ribbon, in_stock FROM ribbon_stock ORDER BY ribbon")
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|r| {
            let ribbon_raw: String = r.get("ribbon");
            let ribbon = Ribbon::parse(&ribbon_raw).ok_or_else(|| {
                ApiError::Internal(format!("unknown ribbon '{ribbon_raw}' in ribbon_stock"))
            })?;
            Ok(RibbonStock {
                ribbon,
                in_stock: r.get::<i64, _>("in_stock") != 0,
            })
        })
        .collect()
}

/// Whether one ribbon is in stock.
///
/// A ribbon with no flag at all counts as *out of stock*, not as in stock: a
/// half-migrated or hand-edited database must not silently offer a mode that
/// may not be printable.
pub async fn ribbon_in_stock(pool: &SqlitePool, ribbon: Ribbon) -> Result<bool, ApiError> {
    let row = sqlx::query("SELECT in_stock FROM ribbon_stock WHERE ribbon = ?1")
        .bind(ribbon.as_str())
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some_and(|r| r.get::<i64, _>("in_stock") != 0))
}

/// Set one ribbon's stock flag (operator action). Upserts, so a flag that was
/// cleared by hand is restored rather than silently ignored.
pub async fn set_ribbon_stock(
    pool: &SqlitePool,
    ribbon: Ribbon,
    in_stock: bool,
) -> Result<RibbonStock, ApiError> {
    sqlx::query(
        "INSERT INTO ribbon_stock (ribbon, in_stock) VALUES (?1, ?2)
         ON CONFLICT(ribbon) DO UPDATE SET in_stock = excluded.in_stock",
    )
    .bind(ribbon.as_str())
    .bind(i64::from(in_stock))
    .execute(pool)
    .await?;
    Ok(RibbonStock { ribbon, in_stock })
}

/// The price of one printed side per ribbon, ordered by ribbon, for the shop
/// config — each one carrying whether it can be ordered right now.
///
/// The price is per *side*: how many of them a card costs is the ribbon's rule
/// ([`Ribbon::billed_sides`]), applied at the checkout, not stored per price.
pub async fn list_ribbon_prices(pool: &SqlitePool) -> Result<Vec<RibbonPrice>, ApiError> {
    let ribbons = list_ribbon_stock(pool).await?;
    let rows = sqlx::query("SELECT ribbon, price_cents FROM ribbon_prices ORDER BY ribbon")
        .fetch_all(pool)
        .await?;
    rows.iter()
        .map(|r| {
            let ribbon_raw: String = r.get("ribbon");
            let ribbon = Ribbon::parse(&ribbon_raw).ok_or_else(|| {
                ApiError::Internal(format!("unknown ribbon '{ribbon_raw}' in ribbon_prices"))
            })?;
            Ok(RibbonPrice {
                ribbon,
                price_cents: r.get("price_cents"),
                available: ribbons
                    .iter()
                    .find(|s| s.ribbon == ribbon)
                    .is_some_and(|s| s.in_stock),
            })
        })
        .collect()
}

/// The price of one printed side on a ribbon, in the currency's minor unit.
///
/// A ribbon with no configured price is a misconfiguration, not a free card, so
/// it surfaces as an error rather than silently pricing at zero.
pub async fn ribbon_price(pool: &SqlitePool, ribbon: Ribbon) -> Result<i64, ApiError> {
    let row = sqlx::query("SELECT price_cents FROM ribbon_prices WHERE ribbon = ?1")
        .bind(ribbon.as_str())
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| ApiError::Internal(format!("no price configured for ribbon {ribbon}")))?;
    Ok(row.get("price_cents"))
}

/// Freeze the ribbon a prefix was charged for, so the spooler prints what the
/// customer paid for even if the operator later changes the prefix's ribbon.
pub async fn set_paid_ribbon(
    pool: &SqlitePool,
    uuid: Uuid,
    ribbon: Ribbon,
) -> Result<(), ApiError> {
    sqlx::query("UPDATE prefixes SET paid_ribbon = ?2 WHERE uuid = ?1")
        .bind(uuid.to_string())
        .bind(ribbon.as_str())
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

fn render_status(v: &str) -> Result<RenderStatus, ApiError> {
    RenderStatus::parse(v)
        .ok_or_else(|| ApiError::Internal(format!("unknown render status {v}")))
}

fn back_source_of(row: &sqlx::sqlite::SqliteRow) -> Result<BackSource, ApiError> {
    let raw: String = row.get("back_source");
    BackSource::parse(&raw)
        .ok_or_else(|| ApiError::Internal(format!("unknown card back_source '{raw}'")))
}

/// Whether a back side is actually *rendered* for a card: a `none` back never
/// is, and a back whose material has not been supplied yet — an `image` with no
/// upload, a `markdown` with no text — is not there either.
///
/// The distinction matters because the render-status columns default to
/// `'pending'` on every row: without this a single-sided card, or a colour card
/// whose back image has not been uploaded, would report a real pending back
/// render instead of no back at all.
fn back_renders(back: BackSource, has_image: bool, has_text: bool) -> bool {
    match back {
        BackSource::None => false,
        BackSource::Front => true,
        BackSource::Image => has_image,
        BackSource::Markdown => has_text,
    }
}

/// Whether a stored text field counts as supplied. Whitespace is not text, so
/// a card holding a lone newline is still missing its markdown back.
fn has_text(s: Option<&str>) -> bool {
    s.is_some_and(|t| !t.trim().is_empty())
}

/// Map the four status columns onto the Card model, suppressing the back sides
/// a card has nothing to render.
fn card_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Card, ApiError> {
    let back = back_source_of(row)?;
    let back_text: Option<String> = row.get("back_text");
    let back_image: Option<String> = row.get("back_image_content_type");
    let has_back = back_renders(back, back_image.is_some(), has_text(back_text.as_deref()));
    Ok(Card {
        id: row.get("id"),
        name: row.get("name"),
        serial_number: row.get("serial_number"),
        back,
        back_text,
        image_content_type: row.get("image_content_type"),
        back_image_content_type: back_image,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        display_front: render_status(&row.get::<String, _>("front_display_status"))?,
        display_back: if has_back {
            Some(render_status(&row.get::<String, _>("back_display_status"))?)
        } else {
            None
        },
        print_front: render_status(&row.get::<String, _>("front_raster_status"))?,
        print_back: if has_back {
            Some(render_status(&row.get::<String, _>("back_raster_status"))?)
        } else {
            None
        },
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
/// back image. Whether `back` is one the prefix's ribbon can print, and whether
/// a `back_text` goes with it, is enforced by the route layer before this is
/// called.
pub async fn upsert_card(
    pool: &SqlitePool,
    prefix: Uuid,
    id: i64,
    name: &str,
    serial_number: Option<&str>,
    back: BackSource,
    back_text: Option<&str>,
    image_path: &str,
    image_content_type: &str,
    back_image_path: Option<&str>,
    back_image_content_type: Option<&str>,
) -> Result<Card, ApiError> {
    sqlx::query(
        "INSERT INTO cards
            (prefix_uuid, id, name, serial_number, back_source, back_text, image_path,
             image_content_type, back_image_path, back_image_content_type)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (prefix_uuid, id) DO UPDATE SET
            name = excluded.name,
            serial_number = excluded.serial_number,
            back_source = excluded.back_source,
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
    .bind(back.as_str())
    .bind(back_text)
    .bind(image_path)
    .bind(image_content_type)
    .bind(back_image_path)
    .bind(back_image_content_type)
    .execute(pool)
    .await?;

    get_card(pool, prefix, id).await
}

/// Point a card's back image at a freshly written blob, leaving its front image
/// and its text fields alone.
///
/// The back source becomes [`BackSource::Image`]: attaching an image *is* saying
/// that this card's back is that image, so the two are written together rather
/// than leaving a card whose stored image nothing would look at. The caller has
/// already checked that the prefix's ribbon can print a colour back.
pub async fn set_card_back_image(
    pool: &SqlitePool,
    prefix: Uuid,
    id: i64,
    path: &str,
    content_type: &str,
) -> Result<Card, ApiError> {
    let done = sqlx::query(
        "UPDATE cards
            SET back_image_path = ?3,
                back_image_content_type = ?4,
                back_source = 'image',
                updated_at = datetime('now')
          WHERE prefix_uuid = ?1 AND id = ?2",
    )
    .bind(prefix.to_string())
    .bind(id)
    .bind(path)
    .bind(content_type)
    .execute(pool)
    .await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::NotFound(format!(
            "card {id} in prefix {prefix}"
        )));
    }
    get_card(pool, prefix, id).await
}

/// How many of a prefix's cards are still missing the material their back is
/// made of: an `image` back with nothing uploaded, or a `markdown` back with no
/// text. A single-sided card, and a `front` back (which needs nothing extra),
/// can never be incomplete.
///
/// Zero for an empty prefix — this answers "is anything missing", not "is there
/// anything here".
///
/// Deliberately computed through [`back_renders`] rather than re-expressed in
/// SQL: this, the card model and the print queue are three views of one
/// question, and a second copy of the rule (with a second idea of what counts
/// as empty text) is exactly how they would drift apart.
pub async fn cards_missing_back(pool: &SqlitePool, prefix: Uuid) -> Result<i64, ApiError> {
    let rows = sqlx::query(
        "SELECT back_source, back_text, back_image_content_type
           FROM cards WHERE prefix_uuid = ?1",
    )
    .bind(prefix.to_string())
    .fetch_all(pool)
    .await?;
    let mut missing = 0;
    for r in &rows {
        let back = back_source_of(r)?;
        let back_text: Option<String> = r.get("back_text");
        let back_image: Option<String> = r.get("back_image_content_type");
        let supplied = back_renders(back, back_image.is_some(), has_text(back_text.as_deref()));
        if back.is_double_sided() && !supplied {
            missing += 1;
        }
    }
    Ok(missing)
}

/// Total ribbon panels a prefix's cards are billed for on a given ribbon.
///
/// The rule is [`Ribbon::billed_sides`], applied per card: YMCKO charges per
/// printed side, YMCKOKO charges a full double-sided card whatever the back is.
/// Derived here from the stored back sources rather than from the request, so
/// the figure the checkout locks into Stripe is the one the cards actually are.
pub async fn billed_sides(
    pool: &SqlitePool,
    prefix: Uuid,
    ribbon: Ribbon,
) -> Result<i64, ApiError> {
    let rows = sqlx::query(
        "SELECT back_source, COUNT(*) AS n FROM cards WHERE prefix_uuid = ?1 GROUP BY back_source",
    )
    .bind(prefix.to_string())
    .fetch_all(pool)
    .await?;
    let mut total = 0;
    for r in &rows {
        let back = back_source_of(r)?;
        total += ribbon.billed_sides(back) * r.get::<i64, _>("n");
    }
    Ok(total)
}

/// How many of a prefix's cards carry a back the given ribbon cannot print —
/// i.e. how many would be stranded by switching the prefix to it.
///
/// Derived from [`Ribbon::allows_back`] rather than a second copy of the rule,
/// so the two cannot drift. Used to refuse a ribbon change that would leave an
/// order unprintable, which nobody would otherwise notice until the spooler
/// went looking for a raster.
pub async fn cards_incompatible_with_ribbon(
    pool: &SqlitePool,
    prefix: Uuid,
    ribbon: Ribbon,
) -> Result<i64, ApiError> {
    if ribbon == Ribbon::Ymcko {
        return Ok(0); // YMCKO prints any back side.
    }
    let rows = sqlx::query(
        "SELECT back_source, COUNT(*) AS n FROM cards WHERE prefix_uuid = ?1 GROUP BY back_source",
    )
    .bind(prefix.to_string())
    .fetch_all(pool)
    .await?;
    let mut bad = 0;
    for r in &rows {
        let src = back_source_of(r)?;
        if !ribbon.allows_back(src) {
            bad += r.get::<i64, _>("n");
        }
    }
    Ok(bad)
}

/// Update a card's metadata without touching its front image.
///
/// The back side is one choice, so a patch carries at most one intent for it:
/// the new `back` (validated against the prefix's ribbon) and, for a markdown
/// back, its text. Both are validated before anything is written, so a refused
/// patch leaves the card exactly as it was.
///
/// Switching source deliberately leaves the *other* material in place: turning a
/// card from `image` to `markdown` and back again must not lose the uploaded
/// image or the typed text. Only the source decides what is rendered.
pub async fn update_card_meta(
    pool: &SqlitePool,
    prefix: Uuid,
    id: i64,
    patch: &CardPatch,
) -> Result<Card, ApiError> {
    let card = get_card(pool, prefix, id).await?; // 404 if the card is unknown
    let ribbon = get_prefix(pool, prefix).await?.ribbon;

    // Validate the whole back change before writing any of it.
    if let Some(back) = patch.back {
        if !ribbon.allows_back(back) {
            return Err(ApiError::BadRequest(format!(
                "a '{back}' back cannot be printed on a {ribbon} prefix: its back panel is \
                 black only, so use 'none' or 'markdown'"
            )));
        }
    }
    if let Some(text) = patch.back_text.as_deref() {
        let effective = patch.back.unwrap_or(card.back);
        if effective != BackSource::Markdown {
            return Err(ApiError::BadRequest(
                "back_text is only valid for a 'markdown' back".into(),
            ));
        }
        let value = if text.trim().is_empty() { None } else { Some(text) };
        sqlx::query(
            "UPDATE cards SET back_text = ?3, updated_at = datetime('now')
              WHERE prefix_uuid = ?1 AND id = ?2",
        )
        .bind(prefix.to_string())
        .bind(id)
        .bind(value)
        .execute(pool)
        .await?;
    }

    if let Some(back) = patch.back {
        sqlx::query(
            "UPDATE cards SET back_source = ?3, updated_at = datetime('now')
              WHERE prefix_uuid = ?1 AND id = ?2",
        )
        .bind(prefix.to_string())
        .bind(id)
        .bind(back.as_str())
        .execute(pool)
        .await?;
    }

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

    if let Some(v) = patch.serial_number.as_deref() {
        let v = if v.trim().is_empty() { None } else { Some(v) };
        sqlx::query(
            "UPDATE cards SET serial_number = ?3, updated_at = datetime('now')
              WHERE prefix_uuid = ?1 AND id = ?2",
        )
        .bind(prefix.to_string())
        .bind(id)
        .bind(v)
        .execute(pool)
        .await?;
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


/// Record the outcome of rendering a card's derived artifacts.
///
/// Each pair is `(status, error)` where `error` is `Some` only on `failed`. A
/// side the mode has no back for passes `None` for the whole back pair, so its
/// columns stay at the `'pending'` default without ever being read as a real
/// pending back render (the model suppresses them for back-less modes).
pub async fn set_card_render_status(
    pool: &SqlitePool,
    prefix: Uuid,
    id: i64,
    front_display: Option<(RenderStatus, Option<String>)>,
    front_raster: Option<(RenderStatus, Option<String>)>,
    back_display: Option<(RenderStatus, Option<String>)>,
    back_raster: Option<(RenderStatus, Option<String>)>,
) -> Result<Card, ApiError> {
    get_card(pool, prefix, id).await?; // 404 for an unknown card
    let stub = || (RenderStatus::Pending, None);
    let (fd, fde) = front_display.unwrap_or_else(stub);
    let (fr, fre) = front_raster.unwrap_or_else(stub);
    let (bd, bde) = back_display.unwrap_or_else(stub);
    let (br, bre) = back_raster.unwrap_or_else(stub);
    sqlx::query(
        "UPDATE cards SET
            front_display_status = ?3, front_display_error = ?4,
            front_raster_status = ?5, front_raster_error = ?6,
            back_display_status = ?7, back_display_error = ?8,
            back_raster_status = ?9, back_raster_error = ?10,
            updated_at = datetime('now')
          WHERE prefix_uuid = ?1 AND id = ?2",
    )
    .bind(prefix.to_string())
    .bind(id)
    .bind(fd.as_str())
    .bind(fde)
    .bind(fr.as_str())
    .bind(fre)
    .bind(bd.as_str())
    .bind(bde)
    .bind(br.as_str())
    .bind(bre)
    .execute(pool)
    .await?;
    get_card(pool, prefix, id).await
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
    // The spooler prints the ribbon that was paid for, not the prefix's current
    // one — an operator changing it after payment must not change what gets
    // printed. Prefixes never priced through checkout fall back to the current
    // ribbon.
    let ribbon = p.paid_ribbon.unwrap_or(p.ribbon);
    let rows = sqlx::query(
        "SELECT id, name, printed_at, back_source, back_text, back_image_content_type,
                front_raster_status, back_raster_status
          FROM cards WHERE prefix_uuid = ?1 ORDER BY id ASC",
    )
    .bind(prefix.to_string())
    .fetch_all(pool)
    .await?;

    // The back side a card actually has, and whether its needed rasters are
    // ready. This is the spooler's ground truth: it prints only `printable`
    // cards, and fetches `print-raster/back` exactly when `back` is not
    // `none`. It never silently prints an original whose render is pending or
    // failed.
    let parse_status =
        |v: String| RenderStatus::parse(&v).unwrap_or(RenderStatus::Pending);
    let cards = rows
        .iter()
        .map(|r| {
            let back = back_source_of(r)?;
            let back_text: Option<String> = r.get("back_text");
            let back_image: Option<String> = r.get("back_image_content_type");
            let front_ready = parse_status(r.get::<String, _>("front_raster_status"))
                == RenderStatus::Ready;
            let back_ready = parse_status(r.get::<String, _>("back_raster_status"))
                == RenderStatus::Ready;
            // Whether there is anything to print on the back at all. A card
            // whose material is still missing is not printable, exactly as the
            // checkout's gate refuses to sell it.
            let material = back_renders(back, back_image.is_some(), has_text(back_text.as_deref()));
            let (back_avail, printable) = match (back.is_double_sided(), material) {
                (false, _) => (BackAvailability::None, front_ready),
                (true, false) => (BackAvailability::None, false),
                // The colour the back prints in is the ribbon's, not the card's.
                (true, true) => (
                    match ribbon.back_depth() {
                        BackDepth::Color => BackAvailability::Color,
                        BackDepth::Black => BackAvailability::Black,
                    },
                    front_ready && back_ready,
                ),
            };
            Ok(PrintQueueCard {
                id: r.get("id"),
                name: r.get("name"),
                printed_at: r.get("printed_at"),
                back: back_avail,
                billed_sides: ribbon.billed_sides(back),
                printable,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    // Derived before `cards` is moved into the queue.
    let unprinted_count = cards.iter().filter(|c| c.printed_at.is_none()).count() as i64;
    let billed_sides = cards.iter().map(|c| c.billed_sides).sum();
    let resume = p.status == PrefixStatus::Printing && unprinted_count > 0;
    Ok(PrintQueue {
        prefix,
        status: p.status,
        ribbon,
        cards,
        unprinted_count,
        billed_sides,
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

    /// A card with a front image and, if `back` needs it, the material for it.
    /// The blob paths are never read by these tests, only the metadata.
    async fn card(pool: &SqlitePool, prefix: Uuid, id: i64, back: BackSource) {
        let (image, text) = match back {
            BackSource::Image => (Some("blobs/x-back.png"), None),
            BackSource::Markdown => (None, Some("# Songs\n- one\n")),
            _ => (None, None),
        };
        upsert_card(
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

    // -----------------------------------------------------------------------
    // Model: the two enums, and the rules that hang off them
    // -----------------------------------------------------------------------

    #[test]
    fn back_source_parse_round_trips_every_value() {
        for (text, back) in [
            ("none", BackSource::None),
            ("front", BackSource::Front),
            ("image", BackSource::Image),
            ("markdown", BackSource::Markdown),
        ] {
            assert_eq!(BackSource::parse(text), Some(back));
            assert_eq!(back.as_str(), text);
        }
        assert_eq!(BackSource::parse("colour"), None);
        // A card with no back is single-sided; every other value is not.
        assert!(!BackSource::None.is_double_sided());
        for b in [BackSource::Front, BackSource::Image, BackSource::Markdown] {
            assert!(b.is_double_sided(), "{b} is a double-sided card");
        }
    }

    #[test]
    fn ribbon_parse_round_trips_both_ribbons() {
        for (text, ribbon) in [("ymcko", Ribbon::Ymcko), ("ymckoko", Ribbon::Ymckoko)] {
            assert_eq!(Ribbon::parse(text), Some(ribbon));
            assert_eq!(ribbon.as_str(), text);
        }
        assert_eq!(Ribbon::parse("ymckokok"), None);
    }

    #[test]
    fn only_ymcko_can_print_a_colour_back() {
        // YMCKO's back is a panel set of its own: anything print there.
        for back in [
            BackSource::None,
            BackSource::Front,
            BackSource::Image,
            BackSource::Markdown,
        ] {
            assert!(Ribbon::Ymcko.allows_back(back), "ymcko allows {back}");
        }
        // YMCKOKO's back is the second black panel: text yes, colour no.
        assert!(Ribbon::Ymckoko.allows_back(BackSource::None));
        assert!(Ribbon::Ymckoko.allows_back(BackSource::Markdown));
        assert!(!Ribbon::Ymckoko.allows_back(BackSource::Image));
        assert!(!Ribbon::Ymckoko.allows_back(BackSource::Front));
    }

    #[test]
    fn back_depth_is_the_ribbons_consequence() {
        assert_eq!(Ribbon::Ymcko.back_depth(), BackDepth::Color);
        assert_eq!(Ribbon::Ymckoko.back_depth(), BackDepth::Black);
    }

    #[test]
    fn billing_is_per_printed_side_on_ymcko_and_always_two_on_ymckoko() {
        // YMCKO bills the sides actually printed.
        assert_eq!(Ribbon::Ymcko.billed_sides(BackSource::None), 1);
        assert_eq!(Ribbon::Ymcko.billed_sides(BackSource::Front), 2);
        assert_eq!(Ribbon::Ymcko.billed_sides(BackSource::Image), 2);
        assert_eq!(Ribbon::Ymcko.billed_sides(BackSource::Markdown), 2);
        // YMCKOKO always burns a full double-sided card: the second black panel
        // is lost either way, so a blank back saves nothing.
        for back in [
            BackSource::None,
            BackSource::Front,
            BackSource::Image,
            BackSource::Markdown,
        ] {
            assert_eq!(Ribbon::Ymckoko.billed_sides(back), 2, "{back}");
        }
    }

    // -----------------------------------------------------------------------
    // Prefixes
    // -----------------------------------------------------------------------

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

    #[tokio::test]
    async fn prefix_defaults_to_ymckoko() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        assert_eq!(p.ribbon, Ribbon::Ymckoko);
        assert_eq!(p.paid_ribbon, None);
    }

    #[tokio::test]
    async fn prefix_accepts_an_explicit_ribbon() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        assert_eq!(p.ribbon, Ribbon::Ymcko);
    }

    #[tokio::test]
    async fn set_prefix_ribbon_is_idempotent_and_persists() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        let switched = set_prefix_ribbon(&pool, p.uuid, Ribbon::Ymcko)
            .await
            .expect("switch");
        assert_eq!(switched.ribbon, Ribbon::Ymcko);
        // Re-applying is a no-op that still returns the prefix.
        let again = set_prefix_ribbon(&pool, p.uuid, Ribbon::Ymcko)
            .await
            .expect("switch again");
        assert_eq!(again.ribbon, Ribbon::Ymcko);
    }

    // -----------------------------------------------------------------------
    // Cards: the back side and what is still missing
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_cards_back_source_and_material_round_trip() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        card(&pool, p.uuid, 1, BackSource::Image).await;
        card(&pool, p.uuid, 2, BackSource::Markdown).await;
        card(&pool, p.uuid, 3, BackSource::None).await;

        let c = get_card(&pool, p.uuid, 1).await.expect("card 1");
        assert_eq!(c.back, BackSource::Image);
        assert_eq!(c.back_image_content_type.as_deref(), Some("image/png"));
        assert!(c.back_text.is_none());
        // A back image exists, so both back artifacts are reported — the front
        // always is.
        assert!(c.display_back.is_some() && c.print_back.is_some());

        let c = get_card(&pool, p.uuid, 2).await.expect("card 2");
        assert_eq!(c.back, BackSource::Markdown);
        assert!(c.back_image_content_type.is_none());
        assert!(c.display_back.is_some());

        // A single-sided card has no back side at all, so the back statuses are
        // absent rather than a synthetic 'pending'.
        let c = get_card(&pool, p.uuid, 3).await.expect("card 3");
        assert_eq!(c.back, BackSource::None);
        assert!(c.display_back.is_none() && c.print_back.is_none());
    }

    #[tokio::test]
    async fn a_back_without_its_material_reads_as_no_back_yet() {
        // A card created from its front alone is the bulk upload flow: the back
        // is chosen, but its image or text is not there yet. That must read as
        // "no back yet", not as a real pending render.
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        upsert_card(
            &pool,
            p.uuid,
            1,
            "front only",
            None,
            BackSource::Image,
            None,
            "blobs/1.png",
            "image/png",
            None,
            None,
        )
        .await
        .expect("upsert");

        let c = get_card(&pool, p.uuid, 1).await.expect("card");
        assert_eq!(c.back, BackSource::Image);
        assert!(c.display_back.is_none() && c.print_back.is_none());
    }

    #[tokio::test]
    async fn an_image_card_is_completed_by_its_back_upload() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        upsert_card(
            &pool,
            p.uuid,
            1,
            "front only",
            None,
            BackSource::Image,
            None,
            "blobs/1.png",
            "image/png",
            None,
            None,
        )
        .await
        .expect("upsert");
        assert_eq!(cards_missing_back(&pool, p.uuid).await.unwrap(), 1);

        let completed = set_card_back_image(&pool, p.uuid, 1, "blobs/1-back.png", "image/png")
            .await
            .expect("attach the back");
        assert_eq!(
            completed.back_image_content_type.as_deref(),
            Some("image/png")
        );
        // The front and the metadata are untouched by the back write.
        assert_eq!(completed.image_content_type, "image/png");
        assert_eq!(completed.name, "front only");
        assert_eq!(cards_missing_back(&pool, p.uuid).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn attaching_a_back_image_makes_the_card_an_image_back() {
        // Attaching an image *is* saying the back is that image, so the source
        // follows it rather than leaving a stored blob nothing renders.
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        card(&pool, p.uuid, 1, BackSource::Markdown).await;

        let updated = set_card_back_image(&pool, p.uuid, 1, "blobs/1-back.png", "image/png")
            .await
            .expect("attach");
        assert_eq!(updated.back, BackSource::Image);
        // The markdown is kept: switching back and forth must not lose work.
        assert!(updated.back_text.is_some());
    }

    #[tokio::test]
    async fn missing_back_counts_only_unsupplied_material() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        // An empty prefix is not "incomplete" — there is nothing to print.
        assert_eq!(cards_missing_back(&pool, p.uuid).await.unwrap(), 0);

        // A single-sided card, and a `front` back, need nothing extra.
        card(&pool, p.uuid, 1, BackSource::None).await;
        card(&pool, p.uuid, 2, BackSource::Front).await;
        assert_eq!(cards_missing_back(&pool, p.uuid).await.unwrap(), 0);

        // An image back and a markdown back with their material are complete...
        card(&pool, p.uuid, 3, BackSource::Image).await;
        card(&pool, p.uuid, 4, BackSource::Markdown).await;
        assert_eq!(cards_missing_back(&pool, p.uuid).await.unwrap(), 0);

        // ...and without it are not. Whitespace-only text is not text.
        upsert_card(
            &pool,
            p.uuid,
            5,
            "no image",
            None,
            BackSource::Image,
            None,
            "blobs/5.png",
            "image/png",
            None,
            None,
        )
        .await
        .expect("upsert");
        upsert_card(
            &pool,
            p.uuid,
            6,
            "blank text",
            None,
            BackSource::Markdown,
            Some("  \n "),
            "blobs/6.png",
            "image/png",
            None,
            None,
        )
        .await
        .expect("upsert");
        assert_eq!(cards_missing_back(&pool, p.uuid).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn incompatibility_is_counted_from_the_ribbons_own_rule() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;
        card(&pool, p.uuid, 2, BackSource::Front).await;
        card(&pool, p.uuid, 3, BackSource::Image).await;
        card(&pool, p.uuid, 4, BackSource::Markdown).await;

        // YMCKO prints any of them, so nothing blocks a change to it.
        assert_eq!(
            cards_incompatible_with_ribbon(&pool, p.uuid, Ribbon::Ymcko)
                .await
                .unwrap(),
            0
        );
        // YMCKOKO cannot print the colour-backed pair.
        assert_eq!(
            cards_incompatible_with_ribbon(&pool, p.uuid, Ribbon::Ymckoko)
                .await
                .unwrap(),
            2
        );
        // Switching is then legal only after they are single-sided again.
        set_card_back_image(&pool, p.uuid, 3, "blobs/3-back.png", "image/png")
            .await
            .expect("re-attach");
        update_card_meta(
            &pool,
            p.uuid,
            3,
            &CardPatch {
                back: Some(BackSource::None),
                ..Default::default()
            },
        )
        .await
        .expect("single-sided again");
        update_card_meta(
            &pool,
            p.uuid,
            2,
            &CardPatch {
                back: Some(BackSource::None),
                ..Default::default()
            },
        )
        .await
        .expect("single-sided again");
        assert_eq!(
            cards_incompatible_with_ribbon(&pool, p.uuid, Ribbon::Ymckoko)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn back_image_for_an_unknown_card_is_a_404() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        let err = set_card_back_image(&pool, p.uuid, 7, "blobs/7-back.png", "image/png")
            .await
            .expect_err("no such card");
        assert!(matches!(err, ApiError::NotFound(_)), "got {err:?}");
    }

    // -----------------------------------------------------------------------
    // Card patches: the back is one choice, validated as one
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn back_text_is_refused_unless_the_back_is_markdown() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;

        let err = update_card_meta(
            &pool,
            p.uuid,
            1,
            &CardPatch {
                back_text: Some("# Songs".into()),
                ..Default::default()
            },
        )
        .await
        .expect_err("back_text on a single-sided card");
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");

        // Setting the source and the text together is one accepted patch.
        let updated = update_card_meta(
            &pool,
            p.uuid,
            1,
            &CardPatch {
                back: Some(BackSource::Markdown),
                back_text: Some("# Songs".into()),
                ..Default::default()
            },
        )
        .await
        .expect("source and text together");
        assert_eq!(updated.back, BackSource::Markdown);
        assert_eq!(updated.back_text.as_deref(), Some("# Songs"));
    }

    #[tokio::test]
    async fn a_markdown_back_is_refused_on_an_ineligible_ribbon() {
        // Markdown is fine on YMCKOKO; a colour back is not.
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymckoko))
            .await
            .expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;

        let err = update_card_meta(
            &pool,
            p.uuid,
            1,
            &CardPatch {
                back: Some(BackSource::Image),
                ..Default::default()
            },
        )
        .await
        .expect_err("an image back cannot be printed on YMCKOKO");
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");

        let ok = update_card_meta(
            &pool,
            p.uuid,
            1,
            &CardPatch {
                back: Some(BackSource::Markdown),
                back_text: Some("- one".into()),
                ..Default::default()
            },
        )
        .await
        .expect("markdown is printable on YMCKOKO");
        assert_eq!(ok.back, BackSource::Markdown);
    }

    #[tokio::test]
    async fn a_refused_patch_writes_nothing() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;

        // The name is valid, the back is not. Neither may land.
        let err = update_card_meta(
            &pool,
            p.uuid,
            1,
            &CardPatch {
                name: Some("Renamed".into()),
                back: Some(BackSource::Image),
                ..Default::default()
            },
        )
        .await
        .expect_err("image back on YMCKOKO");
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");

        let c = get_card(&pool, p.uuid, 1).await.expect("card");
        assert_eq!(c.name, "c", "the valid half of a refused patch must not land");
        assert_eq!(c.back, BackSource::None);
    }

    // -----------------------------------------------------------------------
    // Pricing and stock
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ribbon_prices_are_seeded_per_ribbon() {
        let pool = test_pool().await;
        let prices = list_ribbon_prices(&pool).await.expect("list");
        // The migration collapses the three mode prices onto the two ribbons,
        // at the historical per-side default of 500.
        assert_eq!(prices.len(), 2);
        for p in &prices {
            assert_eq!(p.price_cents, 500, "ribbon {} should default to 500", p.ribbon);
        }
        let ribbons: Vec<_> = prices.iter().map(|p| p.ribbon).collect();
        assert!(ribbons.contains(&Ribbon::Ymcko));
        assert!(ribbons.contains(&Ribbon::Ymckoko));
    }

    #[tokio::test]
    async fn ribbon_stock_flags_are_seeded_in_stock() {
        let pool = test_pool().await;
        let stock = list_ribbon_stock(&pool).await.expect("stock");
        assert_eq!(stock.len(), 2);
        assert!(stock.iter().all(|s| s.in_stock));
        assert!(ribbon_in_stock(&pool, Ribbon::Ymcko).await.expect("ymcko"));
        assert!(ribbon_in_stock(&pool, Ribbon::Ymckoko).await.expect("ymckoko"));
    }

    #[tokio::test]
    async fn clearing_a_ribbon_flag_makes_its_price_unavailable() {
        let pool = test_pool().await;
        set_ribbon_stock(&pool, Ribbon::Ymcko, false)
            .await
            .expect("clear ymcko");
        assert!(!ribbon_in_stock(&pool, Ribbon::Ymcko).await.expect("ymcko"));

        let prices = list_ribbon_prices(&pool).await.expect("list");
        let available = |ribbon: Ribbon| {
            prices
                .iter()
                .find(|p| p.ribbon == ribbon)
                .expect("ribbon in the list")
                .available
        };
        assert!(!available(Ribbon::Ymcko));
        // The other ribbon is independent.
        assert!(available(Ribbon::Ymckoko));

        // Setting it back restores it.
        set_ribbon_stock(&pool, Ribbon::Ymcko, true)
            .await
            .expect("restock");
        let prices = list_ribbon_prices(&pool).await.expect("list");
        assert!(prices.iter().all(|p| p.available));
    }

    #[tokio::test]
    async fn a_ribbon_with_no_flag_counts_as_out_of_stock() {
        let pool = test_pool().await;
        sqlx::query("DELETE FROM ribbon_stock WHERE ribbon = 'ymckoko'")
            .execute(&pool)
            .await
            .expect("delete flag");
        assert!(!ribbon_in_stock(&pool, Ribbon::Ymckoko).await.expect("ymckoko"));
        // ...and the ribbon is not offered, rather than silently so.
        let prices = list_ribbon_prices(&pool).await.expect("list");
        assert!(
            !prices
                .iter()
                .find(|p| p.ribbon == Ribbon::Ymckoko)
                .expect("ribbon")
                .available
        );
    }

    #[tokio::test]
    async fn ribbon_price_lookup_returns_the_configured_price() {
        let pool = test_pool().await;
        assert_eq!(
            ribbon_price(&pool, Ribbon::Ymckoko).await.expect("price"),
            500
        );
        assert_eq!(
            ribbon_price(&pool, Ribbon::Ymcko).await.expect("price"),
            500
        );
    }

    // -----------------------------------------------------------------------
    // Print queue
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn print_queue_reports_the_back_the_ribbon_prints() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymckoko))
            .await
            .expect("create");
        card(&pool, p.uuid, 1, BackSource::Markdown).await;
        card(&pool, p.uuid, 2, BackSource::None).await;

        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.ribbon, Ribbon::Ymckoko);
        // YMCKOKO's back is the black panel, so a markdown back is a 1-bit one.
        assert_eq!(q.cards[0].back, BackAvailability::Black);
        // A single-sided card has nothing to print on the back.
        assert_eq!(q.cards[1].back, BackAvailability::None);
        // YMCKOKO bills two panels for every card, single-sided or not.
        assert_eq!(q.billed_sides, 4);
        assert_eq!(q.cards[0].billed_sides, 2);
        assert_eq!(q.cards[1].billed_sides, 2);
    }

    #[tokio::test]
    async fn print_queue_colours_the_same_back_differently_on_ymcko() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        card(&pool, p.uuid, 1, BackSource::Markdown).await;
        card(&pool, p.uuid, 2, BackSource::Image).await;
        card(&pool, p.uuid, 3, BackSource::Front).await;
        card(&pool, p.uuid, 4, BackSource::None).await;

        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.ribbon, Ribbon::Ymcko);
        // The same markdown back that is black on YMCKOKO is colour here: the
        // colour depth is the ribbon's, not the source's.
        assert_eq!(q.cards[0].back, BackAvailability::Color);
        assert_eq!(q.cards[1].back, BackAvailability::Color);
        assert_eq!(q.cards[2].back, BackAvailability::Color);
        assert_eq!(q.cards[3].back, BackAvailability::None);
        // YMCKO bills per printed side.
        assert_eq!(q.billed_sides, 2 + 2 + 2 + 1);
    }

    #[tokio::test]
    async fn a_double_sided_card_missing_its_material_is_not_printable() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        upsert_card(
            &pool,
            p.uuid,
            1,
            "no image yet",
            None,
            BackSource::Image,
            None,
            "blobs/1.png",
            "image/png",
            None,
            None,
        )
        .await
        .expect("upsert");

        let q = print_queue(&pool, p.uuid).await.expect("queue");
        // No back material and no back to fetch — and the card must not be
        // printed, rather than silently coming out single-sided.
        assert_eq!(q.cards[0].back, BackAvailability::None);
        assert!(!q.cards[0].printable);
    }

    #[tokio::test]
    async fn print_queue_marks_a_card_printable_only_when_its_rasters_are_ready() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        card(&pool, p.uuid, 1, BackSource::Image).await;

        // No render has run yet, so the rasters are still pending: the card is
        // not printable — never silently printed from the original.
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.cards[0].back, BackAvailability::Color);
        assert!(!q.cards[0].printable);

        // The front alone is not enough: this card prints both sides.
        sqlx::query("UPDATE cards SET front_raster_status = 'ready' WHERE prefix_uuid = ?1 AND id = 1")
            .bind(p.uuid.to_string())
            .execute(&pool)
            .await
            .expect("mark front ready");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert!(!q.cards[0].printable);

        // With both rasters ready, the same card is printable.
        sqlx::query("UPDATE cards SET back_raster_status = 'ready' WHERE prefix_uuid = ?1 AND id = 1")
            .bind(p.uuid.to_string())
            .execute(&pool)
            .await
            .expect("mark back ready");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert!(q.cards[0].printable);
    }

    #[tokio::test]
    async fn a_single_sided_card_needs_only_its_front_raster() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;

        sqlx::query("UPDATE cards SET front_raster_status = 'ready' WHERE prefix_uuid = ?1 AND id = 1")
            .bind(p.uuid.to_string())
            .execute(&pool)
            .await
            .expect("mark front ready");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.cards[0].back, BackAvailability::None);
        assert!(q.cards[0].printable);
    }

    #[tokio::test]
    async fn paid_ribbon_is_snapshotted_and_used_by_the_print_queue() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymckoko))
            .await
            .expect("create");
        // A prefix never priced through checkout has no paid ribbon; the queue
        // falls back to the current one.
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.ribbon, Ribbon::Ymckoko);

        // Freeze the ribbon as if a checkout had priced it.
        set_paid_ribbon(&pool, p.uuid, Ribbon::Ymcko)
            .await
            .expect("freeze");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.ribbon, Ribbon::Ymcko);

        // Even if the operator later changes the prefix's ribbon, the queue
        // still reports the paid-for one.
        set_prefix_ribbon(&pool, p.uuid, Ribbon::Ymckoko)
            .await
            .expect("operator changes ribbon");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.ribbon, Ribbon::Ymcko);
    }

    #[tokio::test]
    async fn only_unprinted_cards_make_a_printing_prefix_a_resume() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;
        card(&pool, p.uuid, 2, BackSource::None).await;
        set_status(&pool, p.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .expect("pending");
        set_status(&pool, p.uuid, PrefixStatus::Paid, Some("cs_1"), false)
            .await
            .expect("paid");
        set_status(&pool, p.uuid, PrefixStatus::Queued, None, false)
            .await
            .expect("queued");
        set_status(&pool, p.uuid, PrefixStatus::Printing, None, false)
            .await
            .expect("printing");

        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.unprinted_count, 2);
        assert!(q.resume);

        mark_card_printed(&pool, p.uuid, 1).await.expect("mark 1");
        mark_card_printed(&pool, p.uuid, 2).await.expect("mark 2");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.unprinted_count, 0);
        assert!(!q.resume, "nothing left to print, so nothing to resume");
    }

    // -----------------------------------------------------------------------
    // Payment anomalies and email claims
    // -----------------------------------------------------------------------

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
