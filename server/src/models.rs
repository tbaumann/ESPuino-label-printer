use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Lifecycle of a prefix (one customer order).
///
/// `draft -> payment_pending -> paid -> queued -> printing -> printed`, with
/// `cancelled` reachable from any pre-printing state.
///
/// There is deliberately no "submitted" step. The customer's Pay button opens a
/// Stripe Checkout session, and *that* is what moves a draft to
/// `payment_pending`; paying is the only thing that makes an order real. A
/// submitted-but-unpaid state said nothing a `payment_pending` does not already
/// say, and cost the customer a click.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrefixStatus {
    Draft,
    PaymentPending,
    Paid,
    Queued,
    Printing,
    Printed,
    Cancelled,
}

impl PrefixStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::PaymentPending => "payment_pending",
            Self::Paid => "paid",
            Self::Queued => "queued",
            Self::Printing => "printing",
            Self::Printed => "printed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "draft" => Self::Draft,
            "payment_pending" => Self::PaymentPending,
            "paid" => Self::Paid,
            "queued" => Self::Queued,
            "printing" => Self::Printing,
            "printed" => Self::Printed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// The operator may still add/change cards while the prefix is in these
    /// states. Once printing starts, even the operator's writes are frozen.
    pub fn is_writable(self) -> bool {
        matches!(
            self,
            Self::Draft | Self::PaymentPending | Self::Paid | Self::Queued
        )
    }

    /// The *customer* may only write while the prefix is a draft. The moment
    /// they click Pay the project is sealed for them — from `payment_pending`
    /// onward they get the read-only view, and only the operator can still
    /// change cards. Deliberately narrower than [`Self::is_writable`], which
    /// is the operator's rule.
    pub fn is_customer_writable(self) -> bool {
        matches!(self, Self::Draft)
    }

    /// Allowed transitions.
    pub fn can_transition_to(self, next: Self) -> bool {
        use PrefixStatus::*;
        match self {
            Draft => matches!(next, PaymentPending | Cancelled),
            // A checkout can be abandoned, or fail after the status moved; both
            // put the order back where it started, with nothing paid.
            PaymentPending => matches!(next, Paid | Draft | Cancelled),
            Paid => matches!(next, Queued | Cancelled),
            Queued => matches!(next, Printing | Cancelled),
            Printing => matches!(next, Printed),
            Printed | Cancelled => false,
        }
    }
}

impl std::fmt::Display for PrefixStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a card has on its back side.
///
/// This is the card-level choice, and the only one a *card* makes: [`Self::None`]
/// is a single-sided card, every other value is a double-sided one. There is
/// deliberately no separate "single or double sided" flag beside it — a flag
/// could disagree with this enum, and the two would then have to be reconciled
/// on every write.
///
/// The prefix-level choice is the ribbon (see [`Ribbon`]), which fixes the
/// colour depth the back is rendered in. This enum only says what the back is
/// made *of*.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackSource {
    /// No back side at all: a single-sided card, one ribbon panel per card.
    #[default]
    None,
    /// Print the same panel on the back as on the front — the front image a
    /// second time. No extra upload, but it does cost a second panel.
    Front,
    /// A second, uploaded image printed on the back.
    Image,
    /// Markdown, rendered to an image and printed on the back. Markdown is just
    /// another image *source*: the colour depth it is rendered in comes from the
    /// ribbon, not from here.
    Markdown,
}

impl BackSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Front => "front",
            Self::Image => "image",
            Self::Markdown => "markdown",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "none" => Self::None,
            "front" => Self::Front,
            "image" => Self::Image,
            "markdown" => Self::Markdown,
            _ => return None,
        })
    }

    /// Whether this card is printed on both sides — i.e. whether a second ribbon
    /// panel is spent on it.
    pub fn is_double_sided(self) -> bool {
        !matches!(self, Self::None)
    }
}

impl std::fmt::Display for BackSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The ribbon-pass scheme a job is printed with.
///
/// This is the one choice a user makes per *job*, and the only thing that cannot
/// be mixed in one batch: a YMCKO job and a YMCKOK job are different ribbon
/// loads, so they are different orders. Everything else about a card (single or
/// double sided, and what the back is made of) is a per-card choice, carried by
/// [`BackSource`].
///
/// Availability is a property of the ribbon, not of any card: it is the ribbon
/// that is in or out of stock, so the shop gates on the flag stored per ribbon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Ribbon {
    /// Full colour front, plus a black panel and overlay. The back of a
    /// double-sided card gets a panel set of its own — in full colour.
    Ymcko,
    /// YMCKO plus a second black panel for the back side. The back is 1-bit
    /// black whatever it is made of.
    Ymckok,
}

impl Ribbon {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ymcko => "ymcko",
            Self::Ymckok => "ymckok",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ymcko" => Self::Ymcko,
            "ymckok" => Self::Ymckok,
            _ => return None,
        })
    }

    /// The colour depth a back side is rendered in. A consequence of the ribbon,
    /// never a separate choice — the same back is a 24-bit raster on YMCKO and a
    /// 1-bit one on YMCKOK.
    ///
    /// Every back source is allowed on either ribbon. What the ribbon changes is
    /// only how the print data is generated; a picture on the second black panel
    /// is a poor choice, not an illegal one, so it is the customer's to make.
    pub fn back_depth(self) -> BackDepth {
        match self {
            Self::Ymcko => BackDepth::Color,
            Self::Ymckok => BackDepth::Black,
        }
    }

    /// What one unit of this ribbon's price *is*, for the customer-facing price
    /// display and the Stripe line item: one printed side (YMCKO) or one whole
    /// card (YMCKOK).
    pub fn billing_unit(self) -> &'static str {
        match self {
            Self::Ymcko => "side",
            Self::Ymckok => "card",
        }
    }

    /// How many billing units one card on this ribbon costs, given its back.
    ///
    /// The unit differs per ribbon ([`Self::billing_unit`]), so this is the
    /// multiplier applied to that ribbon's own price — not a count of panels.
    /// YMCKO bills per printed side: a single-sided card is one, a double-sided
    /// one two. YMCKOK bills per *card*, whatever its back: its panel set is
    /// spent whether or not the second black panel is used, so there is nothing
    /// for a blank back to save.
    pub fn billed_units(self, back: BackSource) -> i64 {
        match self {
            Self::Ymcko => 1 + i64::from(back.is_double_sided()),
            Self::Ymckok => 1,
        }
    }
}

impl std::fmt::Display for Ribbon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The colour depth a back side is rendered in. Derived from the ribbon
/// ([`Ribbon::back_depth`]); it is a rendering decision, not a stored one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackDepth {
    /// 24-bit colour — YMCKO's back panel set.
    Color,
    /// 1-bit black — YMCKOK's second K panel.
    Black,
}

/// A prefix groups one customer's set of cards (one order / label batch).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Prefix {
    pub uuid: Uuid,
    pub created_at: String,
    pub status: PrefixStatus,
    /// When the status last changed.
    pub status_changed_at: Option<String>,
    /// The ribbon this order is printed with — the one choice that has to be
    /// made per job, because a batch cannot mix ribbon loads. Defaults to
    /// `ymcko`. Changing it re-renders the cards' back sides; it is the only
    /// thing that decides what the printer is fed.
    pub ribbon: Ribbon,
    /// External shop/order reference, typically set when the order is paid.
    pub order_ref: Option<String>,
    /// Number of cards currently stored under this prefix.
    pub card_count: i64,
    /// Number of cards not yet printed. Operator-only state for the print
    /// spooler: `None` in the public view (omitted from the JSON), `Some(n)`
    /// in the admin list, so the print queue UI can say whether a `printing`
    /// prefix is a resume (still has unprinted cards).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unprinted_count: Option<i64>,
}

/// Where a paid order ships, as Stripe Checkout collected it.
///
/// Admin-only data. It is the customer's own address, so it is served by its
/// own endpoint rather than as a field of [`Prefix`] — the public prefix JSON
/// is readable by anyone holding the uuid, and the customer does not need their
/// address read back to them.
///
/// Every field is optional because the address Stripe collected is: a field the
/// customer left empty comes back as an empty string, which is dropped here
/// rather than stored as one. An address where *nothing* survived is no address
/// at all — that is what `is_empty` answers, and it is the webhook's guard
/// against a blank payload overwriting a stored address.
///
/// Nothing here is validated or normalized. It is copied from Stripe as it was
/// confirmed there, because that is the copy the parcel is addressed from —
/// re-formatting a country or dropping a province would be us guessing at an
/// address a courier has to be able to read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ShippingAddress {
    /// The recipient's name, as typed at Checkout. This is Poste Restante-style
    /// handwritten text, not a field of `address` — Stripe keeps it beside them.
    pub name: Option<String>,
    pub line1: Option<String>,
    pub line2: Option<String>,
    pub postal_code: Option<String>,
    pub city: Option<String>,
    /// Region/province. Frequently absent: it is not part of a German address,
    /// and Stripe only collects it from the countries that have one.
    pub state: Option<String>,
    /// Two-letter ISO country code, as `STRIPE_COUNTRIES` allowed it.
    pub country: Option<String>,
}

impl ShippingAddress {
    /// True when no field carries any text: Stripe collected no address, or
    /// every part of the one it collected was blank.
    ///
    /// This is the difference between "nothing to store" and a real address,
    /// so it is what keeps an addressless event from wiping the address a
    /// previous delivery of the same order stored.
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.line1.is_none()
            && self.line2.is_none()
            && self.postal_code.is_none()
            && self.city.is_none()
            && self.state.is_none()
            && self.country.is_none()
    }
}

/// Availability state of a server-side derived artifact (display image or print
/// raster). Same value set the card JSON and the print endpoints report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RenderStatus {
    /// Not produced yet (just uploaded / invalidated).
    Pending,
    /// Bytes are stored and served.
    Ready,
    /// Generation errored — evidence for an admin, not silently retried.
    Failed,
}

impl RenderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "pending" => Self::Pending,
            "ready" => Self::Ready,
            "failed" => Self::Failed,
            _ => return None,
        })
    }
}

/// What (if anything) a card has on its back side, as the print spooler needs
/// to know before it fetches rasters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackAvailability {
    /// No back side at all: a single-sided card, or a double-sided one still
    /// missing the material its back is made of.
    None,
    /// A 24-bit colour back will be printed — the ribbon's back is a colour
    /// panel set, and its source has no upload to wait for.
    Color,
    /// A 1-bit black back will be printed — the ribbon's back is a black panel.
    Black,
}

/// One printable card. `id` is an index within its prefix, not global.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Card {
    pub id: i64,
    pub name: String,
    /// The card's identity number: the physical RFID UID the print process read
    /// from the card's chip — four UID bytes as `%03d` each, so exactly 12
    /// decimal digits and often with a leading zero — or, on a card no reader
    /// has seen, an operator-assigned serial (the CLI's `--serial-number` /
    /// `--serial-prefix`, `SN-001`). NULL means "not known yet".
    ///
    /// Written by the print process (`POST .../cards/{id}/printed`), never by
    /// the customer UI, which deliberately does not surface it: a value another
    /// system keys on must not be hand-editable by whoever holds the prefix
    /// uuid. A UID is unique per prefix (migration `0015`), because it is one
    /// physical card.
    pub serial_number: Option<String>,
    /// What this card has on its back: the per-card single/double-sided choice.
    /// [`BackSource::None`] is a single-sided card.
    pub back: BackSource,
    /// Markdown source for a [`BackSource::Markdown`] back — the text the
    /// server renders, at the ribbon's colour depth, into the back image.
    /// `None` for every other source, and for a markdown card whose text has
    /// been cleared.
    pub back_text: Option<String>,
    pub image_content_type: String,
    /// Non-None only when this card's back is [`BackSource::Image`] and one has
    /// actually been uploaded: a card exists from its front image alone, and
    /// the back image is added per card afterwards. Never serialized from the
    /// path; this holds the stored content-type purely as the signal that a
    /// back image exists — which is also what the checkout reads to decide
    /// whether the card is complete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub back_image_content_type: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Server-side render availability. The front is always present; the back
    /// sides are `None` (absent) when this card has no back to render. The
    /// frontend uses these to show the processed image or a placeholder.
    pub display_front: RenderStatus,
    pub display_back: Option<RenderStatus>,
    pub print_front: RenderStatus,
    pub print_back: Option<RenderStatus>,
}

/// One card as the print spooler sees it: the operator-only `printed_at` is
/// included here, unlike in the public `Card`. The spooler prints the raster, so
/// this carries no rendering of the back text — only what its
/// `print-raster/front` and `print-raster/back` endpoints answer.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PrintQueueCard {
    pub id: i64,
    pub name: String,
    pub printed_at: Option<String>,
    /// Actual back availability for this card: `none` (single-sided, or a
    /// double-sided card still missing the material its back is made of),
    /// `color`, or `black` — whichever the ribbon's colour depth makes it. The
    /// spooler fetches `print-raster/back` exactly when this is not `none`.
    pub back: BackAvailability,
    /// Billing units this card costs on its ribbon, so the spooler's accounting
    /// matches the checkout's without re-deriving the rule. A unit is one
    /// printed side or one whole card, whichever the ribbon prices by.
    pub billed_units: i64,
    /// Whether every raster this card needs to print is `ready`. The spooler
    /// prints only `printable` cards; a card with a `pending` or `failed`
    /// raster is reported here rather than silently printed from the original.
    pub printable: bool,
}

/// The print queue for one prefix: its cards ordered explicitly by id, with
/// the operator's per-card print state.
///
/// `resume` is true when the prefix is `printing` and still has unprinted
/// cards — i.e. a half-finished order that a spooler can pick up where it left
/// off after a fault, without reprinting the cards already marked.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PrintQueue {
    pub prefix: Uuid,
    pub status: PrefixStatus,
    /// The ribbon the spooler must print with: the prefix's own `ribbon`. Not a
    /// snapshot of what was charged — a ribbon change is a production decision
    /// (which load goes in the printer, and therefore the colour depth of every
    /// back), so the queue and the renderer follow it.
    pub ribbon: Ribbon,
    pub cards: Vec<PrintQueueCard>,
    pub unprinted_count: i64,
    /// Total billing units the whole queue is billed for — the sum of the
    /// cards' `billed_units`, in the ribbon's own unit (sides or cards).
    pub billed_units: i64,
    pub resume: bool,
}

/// Index of all cards under a prefix.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CardIndex {
    pub prefix: Uuid,
    pub cards: Vec<Card>,
}

/// Fields accepted when creating/updating a card (image is sent separately
/// as a multipart file part named "image"). Documents the multipart form for
/// OpenAPI; the handler parses the fields directly.
///
/// `back` is the per-card single/double-sided choice. `back_text` is only
/// meaningful for a `markdown` back and is refused otherwise; a `back_image`
/// file part is the `image` back's material. Every back source is allowed on
/// either ribbon — the ribbon only decides how the back is rendered — so the
/// handler rejects a form that contradicts itself, not a back the ribbon
/// "cannot print".
///
/// `back_image` is optional even for an `image` back: cards are created by the
/// folder-at-a-time front-image upload, and the per-card back is filled in
/// afterwards. A card without one is simply not complete, and an order
/// containing one is refused at the checkout.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[allow(dead_code)]
pub struct CardInput {
    pub name: String,
    pub serial_number: Option<String>,
    /// The card's back side. Omitted means single-sided ([`BackSource::None`]),
    /// unless a `back_image` part is sent, which implies [`BackSource::Image`].
    pub back: Option<BackSource>,
    pub back_text: Option<String>,
    pub back_image: Option<String>,
}

/// Body for creating a prefix. If `uuid` is omitted the server assigns one.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PrefixInput {
    pub uuid: Option<Uuid>,
    /// The ribbon this job is printed with. Omitted (or null) means the default
    /// `ymcko`.
    pub ribbon: Option<Ribbon>,
}

/// Body for changing a prefix's ribbon while it is still writable.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PrefixRibbonPatch {
    pub ribbon: Ribbon,
}

/// Metadata-only card update (no image re-upload). Omitted fields are left
/// unchanged; an empty string clears an optional field.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct CardPatch {
    pub name: Option<String>,
    /// The card's serial / RFID UID — see [`Card::serial_number`]. A blank value
    /// clears it to NULL ("not known yet").
    pub serial_number: Option<String>,
    /// The card's back side. Changing this is how a card becomes (or stops
    /// being) double-sided, and how its back material is chosen.
    pub back: Option<BackSource>,
    pub back_text: Option<String>,
}

/// Body for a status transition (shop / print-queue side).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct StatusInput {
    pub status: PrefixStatus,
    /// Optional external order reference (typically set when marking paid).
    pub order_ref: Option<String>,
}

/// Body for marking a card printed. Optional: the print spooler records the
/// ejection alone when it has no UID to report, and that call carries no body.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PrintedInput {
    /// The physical RFID UID the print process read from the card's chip, as the
    /// 12 decimal digits an ESPuino reports (see [`Card::serial_number`]).
    ///
    /// Absent or blank means "not known yet": the stored UID is left alone, and
    /// so is a card that never had one. The value is written as read — the
    /// writer is the print process reading a real chip, so the server does not
    /// second-guess its shape, and a refused write here would strand a card that
    /// is already physically printed.
    pub serial_number: Option<String>,
}

/// Operator token, exchanged for a session cookie. Sent once, over the wire,
/// at login — after that the browser only carries the cookie.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AdminLogin {
    pub token: String,
}

/// Result of `GET /api/admin/session`, used by the admin UI to decide whether
/// it is already logged in.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AdminSession {
    pub authenticated: bool,
}

/// One ribbon as the shop offers it: the price of one billing unit on it, and
/// whether it can be ordered right now.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RibbonPrice {
    pub ribbon: Ribbon,
    /// Price of one billing unit on this ribbon, in the currency's minor unit
    /// (cents). What that unit is differs per ribbon ([`Ribbon::billing_unit`]):
    /// one printed side on YMCKO — a card costs [`Ribbon::billed_units`] of them
    /// — or one whole card on YMCKOK, whatever the card's back is.
    pub price_cents: i64,
    /// The unit `price_cents` is for: `side` or `card`. Served so the price can
    /// be shown honestly ("€2.50 per card") instead of the UI guessing.
    pub unit: String,
    /// Whether the ribbon is in stock. An unavailable ribbon is still served,
    /// and rendered disabled rather than dropped, so the customer can see that
    /// the option exists and is out.
    pub available: bool,
}

/// One ribbon's stock flag, as the shop config serves it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema)]
pub struct RibbonStock {
    pub ribbon: Ribbon,
    pub in_stock: bool,
}

/// Operator input for flipping a ribbon's stock flag.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct RibbonStockPatch {
    pub in_stock: bool,
}

/// Operator input for setting one ribbon's price.
///
/// The unit is the ribbon's own ([`Ribbon::billing_unit`]) and is not part of
/// the request: a price is only ever "one unit on this ribbon", so asking the
/// operator to restate the unit would let the two disagree.
#[derive(Debug, Clone, Copy, Deserialize, ToSchema)]
pub struct RibbonPricePatch {
    /// Price of one billing unit, in the currency's minor unit (cents).
    /// Negatives are refused; zero is allowed but pointless.
    pub price_cents: i64,
}

/// Public shop configuration, so the customer UI can show a price and a Pay
/// button — or hide both — without the browser knowing anything secret.
///
/// When payment is not configured, `enabled` is false and the amounts are zero;
/// the customer UI then simply shows no way to pay.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ShopConfig {
    pub enabled: bool,
    /// ISO 4217 code, lower case as Stripe wants it.
    pub currency: String,
    /// The price of one billing unit per ribbon, in the currency's minor unit
    /// (cents), each carrying what its unit is and whether that ribbon can be
    /// ordered right now.
    pub prices: Vec<RibbonPrice>,
    /// The same two stock flags, served as they are stored, so the UI can show
    /// ribbon availability without deriving it from a price.
    pub ribbons: Vec<RibbonStock>,
    /// Flat shipping charge in cents; 0 means shipping is free and no shipping
    /// line is added to the checkout.
    pub shipping_cents: i64,
    /// Countries the shipping address may be in.
    pub countries: Vec<String>,
}

/// Why a prefix needs the operator's attention: money and the order disagree.
///
/// A settled payment normally changes the prefix to `paid`. These are exactly
/// the dead ends where that cannot happen, and the item's `action` says what a
/// human should do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NeedsAttentionKind {
    /// A settled payment landed on a prefix that was cancelled first.
    PaymentForCancelled,
    /// A settled payment landed on a prefix that still reads as a draft.
    PaymentForDraft,
    /// A settled payment for a prefix that has since been deleted.
    PaymentForDeleted,
    /// A settled payment for a prefix that is already past printing — it must
    /// have been paid to get there, so this is most likely a duplicate charge.
    PaymentAfterPrinting,
    /// A prefix left sitting in `payment_pending`: the checkout was abandoned
    /// and nothing was ever paid, so no work queue will ever pick it up.
    AbandonedCheckout,
}

impl NeedsAttentionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PaymentForCancelled => "payment_for_cancelled",
            Self::PaymentForDraft => "payment_for_draft",
            Self::PaymentForDeleted => "payment_for_deleted",
            Self::PaymentAfterPrinting => "payment_after_printing",
            Self::AbandonedCheckout => "abandoned_checkout",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "payment_for_cancelled" => Self::PaymentForCancelled,
            "payment_for_draft" => Self::PaymentForDraft,
            "payment_for_deleted" => Self::PaymentForDeleted,
            "payment_after_printing" => Self::PaymentAfterPrinting,
            "abandoned_checkout" => Self::AbandonedCheckout,
            _ => return None,
        })
    }
}

impl std::fmt::Display for NeedsAttentionKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One dead-end event, persisted when a settled payment could not attach to a
/// prefix. Backs the admin "needs attention" list so the list is a query, not
/// a log scrape.
///
/// Unlike the `prefixes` row this deliberately survives a prefix deletion: a
/// payment for a deleted prefix is one of the cases it has to catch, and its
/// `prefix` column may name a row that no longer exists.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentAnomaly {
    pub id: i64,
    /// The prefix the payment was for. May no longer exist in `prefixes`
    /// (deleted) — the row is retained so the money is not lost.
    pub prefix: Uuid,
    pub kind: NeedsAttentionKind,
    pub message: String,
    /// Imperative: what to do about it (e.g. `refund_or_honour`).
    pub action: String,
    /// The Stripe Checkout session the payment came in on, when known.
    pub session_id: Option<String>,
    pub amount_cents: Option<i64>,
    pub currency: Option<String>,
    pub created_at: String,
}

/// One row in the admin "needs attention" list: a prefix where money and the
/// order disagree, and what to do about it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct NeedsAttentionItem {
    pub prefix: Uuid,
    pub kind: NeedsAttentionKind,
    pub action: String,
    pub message: String,
    /// When this was recorded (anomaly) or when the prefix's status last
    /// changed (abandoned checkout). ISO-ish `YYYY-MM-DD HH:MM:SS`; callers
    /// order by it, so it only has to sort lexicographically.
    pub at: String,
    pub session_id: Option<String>,
    pub amount_cents: Option<i64>,
    pub currency: Option<String>,
}
