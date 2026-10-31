use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::error::ApiError;
use crate::models::{
    BackAvailability, BackDepth, BackSource, Card, CardPatch, NeedsAttentionKind, PaymentAnomaly,
    Prefix, PrefixStatus, PrintQueue, PrintQueueCard, RenderStatus, Ribbon, RibbonPrice,
    RibbonStock, ShippingAddress,
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
        p.order_ref, p.ribbon,
        (SELECT COUNT(*) FROM cards c WHERE c.prefix_uuid = p.uuid) AS card_count
 FROM prefixes p";

/// Admin-only variant of `PREFIX_SELECT` that also counts unprinted cards, so
/// the print queue UI can tell a `printing` prefix that is a resume (still has
/// unprinted cards) from one that is done.
const ADMIN_PREFIX_SELECT: &str = "SELECT p.uuid, p.created_at, p.status, p.status_changed_at,
        p.order_ref, p.ribbon,
        (SELECT COUNT(*) FROM cards c WHERE c.prefix_uuid = p.uuid) AS card_count,
        (SELECT COUNT(*) FROM cards c WHERE c.prefix_uuid = p.uuid AND c.printed_at IS NULL)
            AS unprinted_count
 FROM prefixes p";

/// The INSERT that creates a prefix row: a draft on the default ribbon.
///
/// [`create_prefix`] runs it bare — a duplicate uuid is a `409`. The writes that
/// create a project lazily (`upsert_card`, `set_prefix_ribbon`) append their own
/// `ON CONFLICT` clause, so the row a first write creates is the row this
/// statement would have created for it.
const DRAFT_INSERT: &str = "\
INSERT INTO prefixes (uuid, status, status_changed_at, ribbon)
 VALUES (?1, 'draft', datetime('now'), ?2)";

fn prefix_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Prefix, ApiError> {
    let status_raw: String = row.get("status");
    let status = PrefixStatus::parse(&status_raw)
        .ok_or_else(|| ApiError::Internal(format!("unknown prefix status '{status_raw}'")))?;
    let ribbon_raw: String = row.get("ribbon");
    let ribbon = Ribbon::parse(&ribbon_raw)
        .ok_or_else(|| ApiError::Internal(format!("unknown prefix ribbon '{ribbon_raw}'")))?;
    Ok(Prefix {
        uuid: Uuid::parse_str(row.get::<String, _>("uuid").as_str())
            .map_err(|e| ApiError::Internal(e.to_string()))?,
        created_at: row.get("created_at"),
        status,
        status_changed_at: row.get("status_changed_at"),
        ribbon,
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
    let ribbon = ribbon.unwrap_or_default();
    let res = sqlx::query(DRAFT_INSERT)
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

/// Where a prefix ships, as the Stripe webhook stored it.
///
/// `None` when no address was ever stored: an order that has not been paid yet,
/// or a deployment whose Checkout session collects no address. The route that
/// serves this is admin-only — it is the customer's address, and the public
/// prefix JSON deliberately carries none of it.
pub async fn prefix_shipping(
    pool: &SqlitePool,
    uuid: Uuid,
) -> Result<Option<ShippingAddress>, ApiError> {
    let row = sqlx::query(
        "SELECT shipping_name, shipping_line1, shipping_line2, shipping_postal_code,
                shipping_city, shipping_state, shipping_country
           FROM prefixes WHERE uuid = ?1",
    )
    .bind(uuid.to_string())
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::NotFound(format!("prefix {uuid}")))?;

    let address = ShippingAddress {
        name: row.get("shipping_name"),
        line1: row.get("shipping_line1"),
        line2: row.get("shipping_line2"),
        postal_code: row.get("shipping_postal_code"),
        city: row.get("shipping_city"),
        state: row.get("shipping_state"),
        country: row.get("shipping_country"),
    };
    // The columns are written together or not at all, so an all-empty row is a
    // prefix that simply has no address rather than one with a broken one.
    Ok((!address.is_empty()).then_some(address))
}

/// Store where a prefix ships. Idempotent, and the only writer is the Stripe
/// webhook.
///
/// The caller must not pass an empty address: every column here is nullable, so
/// writing blanks over a real address would lose it for good with nothing to
/// recover it from. [`ShippingAddress::is_empty`] is the check, and the webhook
/// makes it before calling this — the alternative is a second delivery of a
/// degraded event silently un-shipping a paid order.
pub async fn set_prefix_shipping(
    pool: &SqlitePool,
    uuid: Uuid,
    address: &ShippingAddress,
) -> Result<(), ApiError> {
    let affected = sqlx::query(
        "UPDATE prefixes
            SET shipping_name = ?2, shipping_line1 = ?3, shipping_line2 = ?4,
                shipping_postal_code = ?5, shipping_city = ?6, shipping_state = ?7,
                shipping_country = ?8
          WHERE uuid = ?1",
    )
    .bind(uuid.to_string())
    .bind(&address.name)
    .bind(&address.line1)
    .bind(&address.line2)
    .bind(&address.postal_code)
    .bind(&address.city)
    .bind(&address.state)
    .bind(&address.country)
    .execute(pool)
    .await?
    .rows_affected();

    if affected == 0 {
        return Err(ApiError::NotFound(format!("prefix {uuid}")));
    }
    Ok(())
}

/// Change a prefix's ribbon, creating the prefix if this is the first write to
/// its uuid. Idempotent: re-applying the current ribbon changes nothing.
///
/// Nothing about a card is checked, because nothing about a card is tied to the
/// ribbon except how it is rendered. Re-rendering the derived artifacts is the
/// route's job; this only writes the column — and creating the row is exactly
/// what its caller checked the ribbon for, so no separate "does it exist" step
/// is needed. The route skips this call when the ribbon is unchanged on a stored
/// prefix, which is what keeps a no-op PATCH from re-rendering every card.
pub async fn set_prefix_ribbon(
    pool: &SqlitePool,
    uuid: Uuid,
    ribbon: Ribbon,
) -> Result<Prefix, ApiError> {
    sqlx::query(&format!(
        "{DRAFT_INSERT} ON CONFLICT(uuid) DO UPDATE SET ribbon = excluded.ribbon"
    ))
    .bind(uuid.to_string())
    .bind(ribbon.as_str())
    .execute(pool)
    .await?;
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

/// The prefix a request is about: the stored row, or an empty draft that is not
/// in the database ([`Prefix::empty`]).
///
/// Every read and every write the customer's editor makes goes through this, so
/// a well-formed uuid nothing has been written to yet is the empty project it
/// names rather than a failed lookup — and the write that follows is what stores
/// it. [`get_prefix`] stays the honest "is it there" question, for the callers
/// that need one: the admin list, the order operations, and the webhook, whose
/// `NotFound` arm is a real anomaly to report.
pub async fn prefix_or_draft(pool: &SqlitePool, uuid: Uuid) -> Result<Prefix, ApiError> {
    match get_prefix(pool, uuid).await {
        Ok(p) => Ok(p),
        Err(ApiError::NotFound(_)) => Ok(Prefix::empty(uuid)),
        Err(e) => Err(e),
    }
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

/// The price of one billing unit per ribbon, ordered by ribbon, for the shop
/// config — each one carrying what its unit is and whether it can be ordered
/// right now.
///
/// The stored figure is per unit, and the *unit* is the ribbon's own
/// ([`Ribbon::billing_unit`]): one printed side on YMCKO, one whole card on
/// YMCKOK. How many units a card costs is [`Ribbon::billed_units`], applied at
/// the checkout, not stored per price.
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
                unit: ribbon.billing_unit().to_string(),
                available: ribbons
                    .iter()
                    .find(|s| s.ribbon == ribbon)
                    .is_some_and(|s| s.in_stock),
            })
        })
        .collect()
}

/// The price of one billing unit on a ribbon, in the currency's minor unit.
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

/// Set one ribbon's price (operator action). Upserts, so a price row deleted by
/// hand is restored rather than silently ignored — a ribbon with no price would
/// refuse every checkout.
///
/// Returns the price as the shop serves it, with the unit derived from the
/// ribbon and `available` from the stock flag, so the caller never has to
/// re-derive either.
pub async fn set_ribbon_price(
    pool: &SqlitePool,
    ribbon: Ribbon,
    price_cents: i64,
) -> Result<RibbonPrice, ApiError> {
    sqlx::query(
        "INSERT INTO ribbon_prices (ribbon, price_cents) VALUES (?1, ?2)
         ON CONFLICT(ribbon) DO UPDATE SET price_cents = excluded.price_cents",
    )
    .bind(ribbon.as_str())
    .bind(price_cents)
    .execute(pool)
    .await?;
    Ok(RibbonPrice {
        ribbon,
        price_cents,
        unit: ribbon.billing_unit().to_string(),
        available: ribbon_in_stock(pool, ribbon).await?,
    })
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
/// **Entering `queued` clears every card's `printed_at`,** in the same
/// transaction as the status write. `queued` means "this order is a print job
/// waiting to be run", so requeueing reprints the whole order: a `printing`
/// order that died halfway, or one already `printed` that has to be printed
/// again. This is the operator's reprint lever, and the only place card print
/// state is ever reset.
///
/// Two consequences are deliberate. `printing` does *not* clear anything — a
/// spooler picking a half-finished job back up resumes from the marks. And
/// re-applying `queued` to a prefix that is already `queued` is not a
/// transition, so it resets nothing: a retried callback must not silently spend
/// a ribbon.
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
    // One transaction, so a prefix can never read as `queued` while its cards
    // still carry the previous run's marks.
    let mut tx = pool.begin().await?;
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
    .execute(&mut *tx)
    .await?;
    if next == PrefixStatus::Queued {
        let cleared = sqlx::query(
            "UPDATE cards SET printed_at = NULL, updated_at = datetime('now')
              WHERE prefix_uuid = ?1 AND printed_at IS NOT NULL",
        )
        .bind(uuid.to_string())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if cleared > 0 {
            tracing::info!(%uuid, cleared, "prefix queued: card print marks cleared, the order reprints");
        }
    }
    tx.commit().await?;
    get_prefix(pool, uuid).await
}

/// Reject card mutations on a sealed prefix, unless the caller is the
/// operator.
///
/// A customer may only write while the prefix is a draft; the operator may
/// write in any state up to and including `queued` (and even while printing,
/// which is what makes a stuck print job recoverable).
///
/// An unknown uuid is a draft ([`prefix_or_draft`]) and therefore writable: the
/// write that follows is what stores the project, so a guard that 404'd here
/// would leave the first upload with nowhere to go.
pub async fn ensure_writable(
    pool: &SqlitePool,
    uuid: Uuid,
    admin: bool,
) -> Result<Prefix, ApiError> {
    let prefix = prefix_or_draft(pool, uuid).await?;
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
    RenderStatus::parse(v).ok_or_else(|| ApiError::Internal(format!("unknown render status {v}")))
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

/// A card's content, as written by [`upsert_card`]: everything about the card
/// except its `(prefix, id)` key. Borrowed, so a caller that owns the strings
/// (the route layer's `StoredCard`) can pass them without cloning.
pub struct CardContent<'a> {
    pub name: &'a str,
    pub serial_number: Option<&'a str>,
    pub back: BackSource,
    pub back_text: Option<&'a str>,
    pub image_path: &'a str,
    pub image_content_type: &'a str,
    pub back_image_path: Option<&'a str>,
    pub back_image_content_type: Option<&'a str>,
}

/// Insert or replace a card. Mirrors a PUT semantics on (prefix, id).
///
/// This is also what creates the prefix on the first write to its uuid: a card
/// is what makes a project real — the customer's page is created by uploading
/// into it, not by opening it. Both rows go in one transaction, because
/// `cards.prefix_uuid` is a foreign key and a card insert that fails (a UID
/// another card already carries, say) must not leave the empty prefix it created
/// behind: a half-done upload that left a dead project is exactly what opening a
/// page must never produce.
///
/// A `back_image_path`/`back_image_content_type` of `None` clears any stored
/// back image. A `back_text` is stored whatever the back source is — only a
/// `markdown` back renders it (see [`back_renders`]) — and no ribbon rules out a
/// back source.
pub async fn upsert_card(
    pool: &SqlitePool,
    prefix: Uuid,
    id: i64,
    card: CardContent<'_>,
) -> Result<Card, ApiError> {
    let mut tx = pool.begin().await?;
    sqlx::query(&format!("{DRAFT_INSERT} ON CONFLICT(uuid) DO NOTHING"))
        .bind(prefix.to_string())
        .bind(Ribbon::default().as_str())
        .execute(&mut *tx)
        .await?;
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
    .bind(card.name)
    .bind(card.serial_number)
    .bind(card.back.as_str())
    .bind(card.back_text)
    .bind(card.image_path)
    .bind(card.image_content_type)
    .bind(card.back_image_path)
    .bind(card.back_image_content_type)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    get_card(pool, prefix, id).await
}

/// Point a card's back image at a freshly written blob, leaving its front image
/// and its text fields alone.
///
/// The back source becomes [`BackSource::Image`]: attaching an image *is* saying
/// that this card's back is that image, so the two are written together rather
/// than leaving a card whose stored image nothing would look at.
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
        return Err(ApiError::NotFound(format!("card {id} in prefix {prefix}")));
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

/// Total billing units a prefix's cards cost on a given ribbon: the sum of
/// [`Ribbon::billed_units`] over the cards.
///
/// The unit is the ribbon's own ([`Ribbon::billing_unit`]) — one printed side on
/// YMCKO (one or two per card), one whole card on YMCKOK — and it is the
/// quantity the checkout's line item multiplies that ribbon's price by. Derived
/// here from the stored back sources rather than from the request, so the figure
/// the checkout locks into Stripe is the one the cards actually are.
pub async fn billed_units(
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
        total += ribbon.billed_units(back) * r.get::<i64, _>("n");
    }
    Ok(total)
}

/// Update a card's metadata without touching its front image.
///
/// The back side is one choice, so a patch carries at most one intent for it:
/// the new `back` and its text. The text is deliberately **not** tied to a
/// `markdown` source: it is stored whatever the back is and only rendered once
/// the back is markdown, so a client that pre-fills a track list does not
/// thereby pick a back side — which spends a ribbon panel.
///
/// The one field with a rule of its own, the name, is validated before anything
/// is written, so a refused patch leaves the card exactly as it was.
///
/// No ribbon can refuse a back: every source is printable on either ribbon, and
/// the ribbon only decides the depth the back is rendered in.
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
    // 404 if the card is unknown — nothing below can then write anything.
    get_card(pool, prefix, id).await?;

    // Validated first, so nothing of a refused patch lands.
    let name = match patch.name.as_deref() {
        Some(raw) => {
            let name = raw.trim();
            if name.is_empty() {
                return Err(ApiError::BadRequest("name must not be empty".into()));
            }
            Some(name)
        }
        None => None,
    };

    if let Some(text) = patch.back_text.as_deref() {
        let value = if text.trim().is_empty() {
            None
        } else {
            Some(text)
        };
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

    if let Some(name) = name {
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
        let updated = sqlx::query(
            "UPDATE cards SET serial_number = ?3, updated_at = datetime('now')
              WHERE prefix_uuid = ?1 AND id = ?2",
        )
        .bind(prefix.to_string())
        .bind(id)
        .bind(v)
        .execute(pool)
        .await;
        if let Err(e) = updated {
            // A UID already on another card of this prefix is a 409 naming that
            // card, not a 500 — the same shape as the print path, which writes
            // the same column.
            if let (true, Some(serial)) = (is_serial_conflict(&e), v) {
                return Err(serial_conflict(pool, prefix, id, serial).await);
            }
            return Err(e.into());
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

/// Whether a failed write is the serial-number uniqueness index refusing a
/// second card of one prefix the same UID.
///
/// `serial_number` is a physical card's identity, so a collision is an
/// operator-visible mistake, not a server fault: an unmapped `Db` error would
/// reach the client as a 500 that says nothing, and the writer here is the print
/// spooler, which cannot ask a question mid-run.
fn is_serial_conflict(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.is_unique_violation())
}

/// The `409` for a UID another card of this prefix already carries, naming that
/// card. Falls back to a plain conflict when the holder cannot be read back.
async fn serial_conflict(pool: &SqlitePool, prefix: Uuid, id: i64, serial: &str) -> ApiError {
    let holder: Result<Option<i64>, sqlx::Error> = sqlx::query_scalar(
        "SELECT id FROM cards
          WHERE prefix_uuid = ?1 AND serial_number = ?2 AND id <> ?3
          ORDER BY id ASC",
    )
    .bind(prefix.to_string())
    .bind(serial)
    .bind(id)
    .fetch_optional(pool)
    .await;
    match holder {
        Ok(Some(other)) => ApiError::Conflict(format!(
            "serial number {serial} already belongs to card {other} of this prefix"
        )),
        _ => ApiError::Conflict(format!("serial number {serial} is already used here")),
    }
}

/// Mark a card printed, once. The spooler calls this only after the printer
/// confirms the card was ejected.
///
/// `serial_number` is the physical RFID UID the spooler read from the chip, when
/// it managed to read one; `None` records the ejection alone and leaves any
/// stored UID alone. This is the only card write that still applies once the
/// prefix is `printing`/`printed` (`ensure_writable` is false there), which is
/// exactly when the UID becomes known — hence an argument on this call rather
/// than a separate write path.
///
/// Idempotent on purpose: `printed_at` only fills when it is NULL, so a second
/// call re-writes the same values and a retried delivery can never print a card
/// twice. (The spooler walks *unprinted* cards, so marking at queue time would
/// silently skip a card if the process died in between — that is why the mark
/// happens here, after ejection, and not when the card is queued.) The UID may
/// arrive with the mark or later, and a re-print on a different chip overwrites
/// it — which is what "the card in this order is <that chip>" means.
///
/// The column is written directly, never through [`update_card_meta`]: nothing
/// about a UID affects a rendered artifact, and that path bumps `updated_at` and
/// makes the caller re-render the rasters.
pub async fn mark_card_printed(
    pool: &SqlitePool,
    prefix: Uuid,
    id: i64,
    serial_number: Option<&str>,
) -> Result<Card, ApiError> {
    get_card(pool, prefix, id).await?; // 404 if the card is unknown
    let updated = sqlx::query(
        "UPDATE cards SET printed_at    = COALESCE(printed_at, datetime('now')),
                          serial_number = COALESCE(?3, serial_number)
          WHERE prefix_uuid = ?1 AND id = ?2",
    )
    .bind(prefix.to_string())
    .bind(id)
    .bind(serial_number)
    .execute(pool)
    .await;
    if let Err(e) = updated {
        if let (true, Some(serial)) = (is_serial_conflict(&e), serial_number) {
            return Err(serial_conflict(pool, prefix, id, serial).await);
        }
        return Err(e.into());
    }
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
    // The prefix's own ribbon decides what prints — which load goes in the
    // printer, and therefore the colour depth of every back. There is exactly
    // one source for it, and the renderer reads the same one.
    let ribbon = p.ribbon;
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
    let parse_status = |v: String| RenderStatus::parse(&v).unwrap_or(RenderStatus::Pending);
    let cards = rows
        .iter()
        .map(|r| {
            let back = back_source_of(r)?;
            let back_text: Option<String> = r.get("back_text");
            let back_image: Option<String> = r.get("back_image_content_type");
            let front_ready =
                parse_status(r.get::<String, _>("front_raster_status")) == RenderStatus::Ready;
            let back_ready =
                parse_status(r.get::<String, _>("back_raster_status")) == RenderStatus::Ready;
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
                billed_units: ribbon.billed_units(back),
                printable,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    // Derived before `cards` is moved into the queue.
    let unprinted_count = cards.iter().filter(|c| c.printed_at.is_none()).count() as i64;
    let billed_units = cards.iter().map(|c| c.billed_units).sum();
    let resume = p.status == PrefixStatus::Printing && unprinted_count > 0;
    Ok(PrintQueue {
        prefix,
        status: p.status,
        ribbon,
        cards,
        unprinted_count,
        billed_units,
        resume,
    })
}

// ---------------------------------------------------------------------------
// Payment anomalies — the dead ends behind the admin "needs attention" list
// ---------------------------------------------------------------------------

/// The fields of a [`PaymentAnomaly`] row the caller supplies; `id` and
/// `created_at` are the database's.
pub struct NewPaymentAnomaly<'a> {
    pub kind: NeedsAttentionKind,
    pub message: &'a str,
    pub action: &'a str,
    pub session_id: Option<&'a str>,
    pub amount_cents: Option<i64>,
    pub currency: Option<&'a str>,
}

/// Record a dead-end payment event: a settled payment that could not attach to
/// a prefix (cancelled, draft, deleted, or already past printing). Written by
/// the webhook at the moment the dead end happens, so the "needs attention"
/// list is a query over this table rather than a scrape of the log.
pub async fn insert_payment_anomaly(
    pool: &SqlitePool,
    prefix: Uuid,
    anomaly: NewPaymentAnomaly<'_>,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO payment_anomalies
            (prefix, kind, message, action, session_id, amount_cents, currency)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(prefix.to_string())
    .bind(anomaly.kind.as_str())
    .bind(anomaly.message)
    .bind(anomaly.action)
    .bind(anomaly.session_id)
    .bind(anomaly.amount_cents)
    .bind(anomaly.currency)
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
            CardContent {
                name: "c",
                serial_number: None,
                back,
                back_text: text,
                image_path: "blobs/f.png",
                image_content_type: "image/png",
                back_image_path: image,
                back_image_content_type: image.map(|_| "image/png"),
            },
        )
        .await
        .expect("upsert card");
    }

    /// `printed_at` is operator-only state: it lives on the print queue's card,
    /// never on the public [`Card`].
    async fn printed_at(pool: &SqlitePool, prefix: Uuid, id: i64) -> Option<String> {
        print_queue(pool, prefix)
            .await
            .expect("queue")
            .cards
            .into_iter()
            .find(|c| c.id == id)
            .expect("card in the queue")
            .printed_at
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
        for (text, ribbon) in [("ymcko", Ribbon::Ymcko), ("ymckok", Ribbon::Ymckok)] {
            assert_eq!(Ribbon::parse(text), Some(ribbon));
            assert_eq!(ribbon.as_str(), text);
        }
        assert_eq!(Ribbon::parse("ymckokk"), None);
    }

    /// No ribbon rules out a back side: the only thing the ribbon changes is the
    /// depth the back is rendered in. A picture on the second black panel is a
    /// poor choice, not a forbidden one.
    #[test]
    fn every_back_is_legal_on_either_ribbon_and_only_the_depth_differs() {
        let backs = [
            BackSource::None,
            BackSource::Front,
            BackSource::Image,
            BackSource::Markdown,
        ];
        for back in backs {
            // The depth is the ribbon's consequence, whatever the source.
            assert_eq!(
                Ribbon::Ymcko.back_depth(),
                BackDepth::Color,
                "{back} on ymcko"
            );
            assert_eq!(
                Ribbon::Ymckok.back_depth(),
                BackDepth::Black,
                "{back} on ymckok"
            );
        }
    }

    /// The two ribbons price the same order differently, because their unit is
    /// different: YMCKO charges per printed side, YMCKOK per whole card —
    /// its panel set is spent whether or not the second black panel is used.
    #[test]
    fn billing_units_are_sides_on_ymcko_and_cards_on_ymckok() {
        assert_eq!(Ribbon::Ymcko.billing_unit(), "side");
        assert_eq!(Ribbon::Ymckok.billing_unit(), "card");

        assert_eq!(Ribbon::Ymcko.billed_units(BackSource::None), 1);
        assert_eq!(Ribbon::Ymcko.billed_units(BackSource::Front), 2);
        assert_eq!(Ribbon::Ymcko.billed_units(BackSource::Image), 2);
        assert_eq!(Ribbon::Ymcko.billed_units(BackSource::Markdown), 2);

        // One unit — one card — per card, single-sided or not. Not two: the
        // stored price is the price of a *card* on this ribbon, so charging two
        // of them per card doubles the bill.
        for back in [
            BackSource::None,
            BackSource::Front,
            BackSource::Image,
            BackSource::Markdown,
        ] {
            assert_eq!(Ribbon::Ymckok.billed_units(back), 1, "{back}");
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
    async fn prefix_defaults_to_ymcko() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        assert_eq!(p.ribbon, Ribbon::Ymcko);
    }

    #[tokio::test]
    async fn prefix_accepts_an_explicit_ribbon() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        assert_eq!(p.ribbon, Ribbon::Ymcko);
    }

    // -----------------------------------------------------------------------
    // A project exists from its first write, not from opening its page
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn an_unwritten_uuid_reads_as_an_empty_draft() {
        let pool = test_pool().await;
        let uuid = Uuid::new_v4();

        let draft = prefix_or_draft(&pool, uuid).await.expect("a draft");
        assert!(!draft.is_stored());
        assert_eq!(draft.status, PrefixStatus::Draft);
        assert_eq!(draft.ribbon, Ribbon::Ymcko);
        assert_eq!(draft.card_count, 0);
        assert!(draft.status_changed_at.is_none());

        // Reading it stored nothing...
        assert!(matches!(
            get_prefix(&pool, uuid).await,
            Err(ApiError::NotFound(_))
        ));

        // ...and once it is written the same read answers the stored row.
        create_prefix(&pool, Some(uuid), None).await.expect("create");
        let stored = prefix_or_draft(&pool, uuid).await.expect("stored");
        assert!(stored.is_stored());
        assert!(stored.created_at.is_some());
    }

    #[tokio::test]
    async fn writing_a_card_creates_the_prefix_it_is_written_into() {
        let pool = test_pool().await;
        let uuid = Uuid::new_v4();
        assert!(matches!(
            get_prefix(&pool, uuid).await,
            Err(ApiError::NotFound(_))
        ));

        card(&pool, uuid, 1, BackSource::None).await;

        let p = get_prefix(&pool, uuid)
            .await
            .expect("the card stored the project");
        assert!(p.is_stored());
        assert_eq!(p.status, PrefixStatus::Draft);
        assert_eq!(p.card_count, 1);
    }

    #[tokio::test]
    async fn setting_the_ribbon_of_an_unwritten_uuid_stores_it() {
        let pool = test_pool().await;
        let uuid = Uuid::new_v4();

        let p = set_prefix_ribbon(&pool, uuid, Ribbon::Ymckok)
            .await
            .expect("the ribbon write stores the project");
        assert!(p.is_stored());
        assert_eq!(p.ribbon, Ribbon::Ymckok);
        assert_eq!(p.status, PrefixStatus::Draft);
    }

    #[tokio::test]
    async fn setting_the_ribbon_leaves_the_rest_of_the_prefix_alone() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        set_status(&pool, p.uuid, PrefixStatus::PaymentPending, None, false)
            .await
            .expect("seal it");

        let after = set_prefix_ribbon(&pool, p.uuid, Ribbon::Ymckok)
            .await
            .expect("switch");

        // The upsert writes `ribbon` and nothing else: a sealed order stays
        // sealed, and its status is not quietly reset to draft.
        assert_eq!(after.status, PrefixStatus::PaymentPending);
        assert_eq!(after.ribbon, Ribbon::Ymckok);
        assert!(after.is_stored());
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
            CardContent {
                name: "front only",
                serial_number: None,
                back: BackSource::Image,
                back_text: None,
                image_path: "blobs/1.png",
                image_content_type: "image/png",
                back_image_path: None,
                back_image_content_type: None,
            },
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
            CardContent {
                name: "front only",
                serial_number: None,
                back: BackSource::Image,
                back_text: None,
                image_path: "blobs/1.png",
                image_content_type: "image/png",
                back_image_path: None,
                back_image_content_type: None,
            },
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
            CardContent {
                name: "no image",
                serial_number: None,
                back: BackSource::Image,
                back_text: None,
                image_path: "blobs/5.png",
                image_content_type: "image/png",
                back_image_path: None,
                back_image_content_type: None,
            },
        )
        .await
        .expect("upsert");
        upsert_card(
            &pool,
            p.uuid,
            6,
            CardContent {
                name: "blank text",
                serial_number: None,
                back: BackSource::Markdown,
                back_text: Some("  \n "),
                image_path: "blobs/6.png",
                image_content_type: "image/png",
                back_image_path: None,
                back_image_content_type: None,
            },
        )
        .await
        .expect("upsert");
        assert_eq!(cards_missing_back(&pool, p.uuid).await.unwrap(), 2);
    }

    /// What the checkout charges: the *same* four cards cost one thing on YMCKO
    /// (per printed side) and another on YMCKOK (per card), because each ribbon
    /// prices its own unit. Every back source counts on either ribbon — no card
    /// is unprintable, so no card has to be changed to switch.
    #[tokio::test]
    async fn an_orders_billed_units_depend_on_the_ribbon() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymcko))
            .await
            .expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;
        card(&pool, p.uuid, 2, BackSource::Front).await;
        card(&pool, p.uuid, 3, BackSource::Image).await;
        card(&pool, p.uuid, 4, BackSource::Markdown).await;

        // YMCKO: 1 + 2 + 2 + 2 printed sides.
        assert_eq!(billed_units(&pool, p.uuid, Ribbon::Ymcko).await.unwrap(), 7);
        // YMCKOK: one card each, whatever the back is.
        assert_eq!(
            billed_units(&pool, p.uuid, Ribbon::Ymckok).await.unwrap(),
            4
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
    // Card patches: the back is one choice, and its text does not pick it
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn back_text_is_stored_without_a_markdown_back() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;

        // A text can arrive on a card whose back is not markdown: that is how a
        // client pre-fills a track list without choosing a back side (which
        // spends a ribbon panel). It is stored, and nothing renders it yet.
        let updated = update_card_meta(
            &pool,
            p.uuid,
            1,
            &CardPatch {
                back_text: Some("# Songs".into()),
                ..Default::default()
            },
        )
        .await
        .expect("back_text on a single-sided card");
        assert_eq!(updated.back, BackSource::None);
        assert_eq!(updated.back_text.as_deref(), Some("# Songs"));
        assert!(
            updated.display_back.is_none(),
            "a `none` back renders nothing"
        );

        // Setting the source and the text together is one accepted patch, and
        // the stored text is then what the back renders from.
        let updated = update_card_meta(
            &pool,
            p.uuid,
            1,
            &CardPatch {
                back: Some(BackSource::Markdown),
                ..Default::default()
            },
        )
        .await
        .expect("the source alone keeps the text");
        assert_eq!(updated.back, BackSource::Markdown);
        assert_eq!(updated.back_text.as_deref(), Some("# Songs"));
        assert!(updated.display_back.is_some(), "a markdown back renders");
    }

    #[tokio::test]
    async fn every_back_source_is_storable_on_either_ribbon() {
        // A picture back on a black-panel ribbon is a poor choice, not a
        // refused one: it is stored, and rendered in black.
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymckok))
            .await
            .expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;

        for back in [BackSource::Front, BackSource::Image] {
            let ok = update_card_meta(
                &pool,
                p.uuid,
                1,
                &CardPatch {
                    back: Some(back),
                    ..Default::default()
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{back} on YMCKOK: {e:?}"));
            assert_eq!(ok.back, back);
        }

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
        .expect("markdown on YMCKOK");
        assert_eq!(ok.back, BackSource::Markdown);
    }

    #[tokio::test]
    async fn a_refused_patch_writes_nothing() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;

        // The name is the one field with a rule of its own now (no ribbon rule
        // can refuse a patch, and a `back_text` no longer needs a markdown
        // back), so it is what refuses the patch — and neither half may land.
        let err = update_card_meta(
            &pool,
            p.uuid,
            1,
            &CardPatch {
                name: Some("   ".into()),
                back_text: Some("- one".into()),
                ..Default::default()
            },
        )
        .await
        .expect_err("a blank name");
        assert!(matches!(err, ApiError::BadRequest(_)), "got {err:?}");

        let c = get_card(&pool, p.uuid, 1).await.expect("card");
        assert_eq!(
            c.name, "c",
            "the valid half of a refused patch must not land"
        );
        assert_eq!(c.back_text, None);
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
            assert_eq!(
                p.price_cents, 500,
                "ribbon {} should default to 500",
                p.ribbon
            );
        }
        let ribbons: Vec<_> = prices.iter().map(|p| p.ribbon).collect();
        assert!(ribbons.contains(&Ribbon::Ymcko));
        assert!(ribbons.contains(&Ribbon::Ymckok));
    }

    #[tokio::test]
    async fn ribbon_stock_flags_are_seeded_in_stock() {
        let pool = test_pool().await;
        let stock = list_ribbon_stock(&pool).await.expect("stock");
        assert_eq!(stock.len(), 2);
        assert!(stock.iter().all(|s| s.in_stock));
        assert!(ribbon_in_stock(&pool, Ribbon::Ymcko).await.expect("ymcko"));
        assert!(ribbon_in_stock(&pool, Ribbon::Ymckok)
            .await
            .expect("ymckok"));
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
        assert!(available(Ribbon::Ymckok));

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
        sqlx::query("DELETE FROM ribbon_stock WHERE ribbon = 'ymckok'")
            .execute(&pool)
            .await
            .expect("delete flag");
        assert!(!ribbon_in_stock(&pool, Ribbon::Ymckok)
            .await
            .expect("ymckok"));
        // ...and the ribbon is not offered, rather than silently so.
        let prices = list_ribbon_prices(&pool).await.expect("list");
        assert!(
            !prices
                .iter()
                .find(|p| p.ribbon == Ribbon::Ymckok)
                .expect("ribbon")
                .available
        );
    }

    #[tokio::test]
    async fn ribbon_price_lookup_returns_the_configured_price() {
        let pool = test_pool().await;
        assert_eq!(
            ribbon_price(&pool, Ribbon::Ymckok).await.expect("price"),
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

    /// Queuing is the operator's reprint lever: it puts every card back into
    /// the run. Nothing else resets print state, and a retry must not.
    #[tokio::test]
    async fn entering_queued_clears_every_card_print_mark() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;
        card(&pool, p.uuid, 2, BackSource::None).await;
        mark_card_printed(&pool, p.uuid, 1, None)
            .await
            .expect("printed");
        mark_card_printed(&pool, p.uuid, 2, None)
            .await
            .expect("printed");
        assert_eq!(print_queue(&pool, p.uuid).await.unwrap().unprinted_count, 0);

        // Somewhere that is not `queued` leaves the marks alone.
        set_status(&pool, p.uuid, PrefixStatus::Paid, None, true)
            .await
            .expect("paid");
        assert_eq!(
            print_queue(&pool, p.uuid).await.unwrap().unprinted_count,
            0,
            "moving to paid is not a reprint"
        );

        // Entering `queued` resets the whole run.
        set_status(&pool, p.uuid, PrefixStatus::Queued, None, true)
            .await
            .expect("queued");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.unprinted_count, 2, "every card is unprinted again");
        assert!(q.cards.iter().all(|c| c.printed_at.is_none()));
        assert!(!q.resume, "a fresh run is not a resume");

        // Re-applying `queued` is not a transition: a card printed from this
        // queue stays marked, so a retried call cannot reprint a whole order.
        mark_card_printed(&pool, p.uuid, 1, None)
            .await
            .expect("printed");
        set_status(&pool, p.uuid, PrefixStatus::Queued, None, true)
            .await
            .expect("queued again");
        assert_eq!(
            print_queue(&pool, p.uuid).await.unwrap().unprinted_count,
            1,
            "a no-op re-queue must not clear the marks"
        );

        // A half-finished order picked back up keeps its marks too: `printing`
        // is where a spooler resumes from, not where it restarts.
        set_status(&pool, p.uuid, PrefixStatus::Printing, None, true)
            .await
            .expect("printing");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.unprinted_count, 1);
        assert!(q.resume, "printing with cards still unprinted is a resume");
    }

    #[tokio::test]
    async fn print_queue_reports_the_back_the_ribbon_prints() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, Some(Ribbon::Ymckok))
            .await
            .expect("create");
        card(&pool, p.uuid, 1, BackSource::Markdown).await;
        card(&pool, p.uuid, 2, BackSource::None).await;

        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.ribbon, Ribbon::Ymckok);
        // YMCKOK's back is the black panel, so a markdown back is a 1-bit one.
        assert_eq!(q.cards[0].back, BackAvailability::Black);
        // A single-sided card has nothing to print on the back.
        assert_eq!(q.cards[1].back, BackAvailability::None);
        // YMCKOK bills one card per card, single-sided or not.
        assert_eq!(q.billed_units, 2);
        assert_eq!(q.cards[0].billed_units, 1);
        assert_eq!(q.cards[1].billed_units, 1);
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
        // The same markdown back that is black on YMCKOK is colour here: the
        // colour depth is the ribbon's, not the source's.
        assert_eq!(q.cards[0].back, BackAvailability::Color);
        assert_eq!(q.cards[1].back, BackAvailability::Color);
        assert_eq!(q.cards[2].back, BackAvailability::Color);
        assert_eq!(q.cards[3].back, BackAvailability::None);
        // YMCKO bills per printed side.
        assert_eq!(q.billed_units, 2 + 2 + 2 + 1);
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
            CardContent {
                name: "no image yet",
                serial_number: None,
                back: BackSource::Image,
                back_text: None,
                image_path: "blobs/1.png",
                image_content_type: "image/png",
                back_image_path: None,
                back_image_content_type: None,
            },
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
        sqlx::query(
            "UPDATE cards SET front_raster_status = 'ready' WHERE prefix_uuid = ?1 AND id = 1",
        )
        .bind(p.uuid.to_string())
        .execute(&pool)
        .await
        .expect("mark front ready");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert!(!q.cards[0].printable);

        // With both rasters ready, the same card is printable.
        sqlx::query(
            "UPDATE cards SET back_raster_status = 'ready' WHERE prefix_uuid = ?1 AND id = 1",
        )
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

        sqlx::query(
            "UPDATE cards SET front_raster_status = 'ready' WHERE prefix_uuid = ?1 AND id = 1",
        )
        .bind(p.uuid.to_string())
        .execute(&pool)
        .await
        .expect("mark front ready");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.cards[0].back, BackAvailability::None);
        assert!(q.cards[0].printable);
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

        mark_card_printed(&pool, p.uuid, 1, None)
            .await
            .expect("mark 1");
        mark_card_printed(&pool, p.uuid, 2, None)
            .await
            .expect("mark 2");
        let q = print_queue(&pool, p.uuid).await.expect("queue");
        assert_eq!(q.unprinted_count, 0);
        assert!(!q.resume, "nothing left to print, so nothing to resume");
    }

    /// The card's RFID UID arrives with the mark that records its ejection.
    ///
    /// Two things about that write are load-bearing: it never bumps `updated_at`
    /// (nothing about a UID changes a rendered artifact, so the customer's
    /// preview must not repaint for it), and a UID that arrives later still
    /// lands — while `printed_at` keeps its first value.
    #[tokio::test]
    async fn the_uid_arrives_with_the_print_mark() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;
        let before = get_card(&pool, p.uuid, 1).await.expect("card").updated_at;

        // The bare call still records the ejection alone — the shape a spooler
        // without a reader sends.
        let c = mark_card_printed(&pool, p.uuid, 1, None)
            .await
            .expect("printed");
        assert!(c.serial_number.is_none(), "no reader, no UID");
        let first_mark = printed_at(&pool, p.uuid, 1).await;
        assert!(first_mark.is_some());

        // A UID that only becomes known later fills in, without moving
        // `printed_at` (first write wins) and without a re-render.
        let c = mark_card_printed(&pool, p.uuid, 1, Some("012345678901"))
            .await
            .expect("printed with uid");
        assert_eq!(c.serial_number.as_deref(), Some("012345678901"));
        assert_eq!(
            printed_at(&pool, p.uuid, 1).await,
            first_mark,
            "a retry must not re-stamp the ejection"
        );
        assert_eq!(
            c.updated_at, before,
            "the UID is not a material change: no re-render, no repaint"
        );
    }

    /// A UID identifies one physical card, so two cards of one prefix may not
    /// share it — and the refused write is a 409 naming the card that holds it,
    /// never a 500, so the spooler's operator can see what to do about it. The
    /// card that was refused stays unprinted, so a rerun picks it up.
    #[tokio::test]
    async fn a_uid_can_only_belong_to_one_card_of_a_prefix() {
        let pool = test_pool().await;
        let p = create_prefix(&pool, None, None).await.expect("create");
        card(&pool, p.uuid, 1, BackSource::None).await;
        card(&pool, p.uuid, 2, BackSource::None).await;
        mark_card_printed(&pool, p.uuid, 1, Some("123456789012"))
            .await
            .expect("card 1 takes the chip");

        let dup = mark_card_printed(&pool, p.uuid, 2, Some("123456789012"))
            .await
            .expect_err("the same chip in one prefix");
        assert!(
            matches!(dup, ApiError::Conflict(ref m) if m.contains("card 1")),
            "got {dup:?}"
        );
        let c = get_card(&pool, p.uuid, 2).await.expect("card 2");
        assert!(c.serial_number.is_none());
        assert!(
            printed_at(&pool, p.uuid, 2).await.is_none(),
            "a refused mark writes nothing, so a rerun still picks the card up"
        );

        // A hand-edit through PATCH hits the same constraint, and answers the
        // same way rather than as an unmapped database error.
        let patch = CardPatch {
            serial_number: Some("123456789012".into()),
            ..Default::default()
        };
        let dup = update_card_meta(&pool, p.uuid, 2, &patch)
            .await
            .expect_err("PATCH onto a taken UID");
        assert!(
            matches!(dup, ApiError::Conflict(ref m) if m.contains("card 1")),
            "got {dup:?}"
        );

        // The index is scoped to the prefix on purpose: the same chip
        // re-personalised in a second order is not a duplicate.
        let other = create_prefix(&pool, None, None).await.expect("create");
        card(&pool, other.uuid, 1, BackSource::None).await;
        mark_card_printed(&pool, other.uuid, 1, Some("123456789012"))
            .await
            .expect("another order may use the same chip");

        // And "not known yet" is not a value: any number of unread cards coexist.
        card(&pool, p.uuid, 3, BackSource::None).await;
        mark_card_printed(&pool, p.uuid, 3, None)
            .await
            .expect("a second card without a UID");
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
            NewPaymentAnomaly {
                kind: NeedsAttentionKind::PaymentForDeleted,
                message: "a settled payment for a prefix that has since been deleted",
                action: "refund_or_rebuild_by_hand",
                session_id: Some("cs_1"),
                amount_cents: Some(1750),
                currency: Some("eur"),
            },
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
