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

/// The ribbon-pass scheme a prefix is printed with, and therefore what a card
/// is allowed to carry on its back side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrintMode {
    /// YMCKO, one panel set per card. Front image only — a card in this mode
    /// has no back at all (no back_text, no back image).
    SingleSided,
    /// YMCKO, two panel sets per card. Colour front AND colour back; the back
    /// is an uploaded image, not text.
    DoubleSidedColor,
    /// YMCKOKO. Colour front, black-only back rendered from markdown
    /// (`back_text`). The default, matching every pre-existing row.
    ColorFrontBlackBack,
}

impl PrintMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SingleSided => "single_sided",
            Self::DoubleSidedColor => "double_sided_color",
            Self::ColorFrontBlackBack => "color_front_black_back",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "single_sided" => Self::SingleSided,
            "double_sided_color" => Self::DoubleSidedColor,
            "color_front_black_back" => Self::ColorFrontBlackBack,
            _ => return None,
        })
    }

    /// Whether the black-only `back_text` (markdown) is a valid field for this
    /// mode. Only the black-back mode renders it; everywhere else it would be
    /// silently meaningless, so the API rejects it.
    pub fn allows_back_text(self) -> bool {
        matches!(self, Self::ColorFrontBlackBack)
    }

    /// Whether an uploaded colour back image is a valid (and, in double-sided
    /// colour, required) field for this mode.
    pub fn allows_back_image(self) -> bool {
        matches!(self, Self::DoubleSidedColor)
    }
}

impl std::fmt::Display for PrintMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A prefix groups one customer's set of cards (one order / label batch).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Prefix {
    pub uuid: Uuid,
    pub created_at: String,
    pub status: PrefixStatus,
    /// When the status last changed.
    pub status_changed_at: Option<String>,
    /// The ribbon-pass scheme this order is printed with; governs what a card
    /// may carry on its back. Defaults to `color_front_black_back`.
    pub print_mode: PrintMode,
    /// The mode that was actually charged, snapshotted when the checkout
    /// session was created. The spooler prints this, not `print_mode`, so an
    /// operator changing the mode after payment cannot change what gets
    /// printed. `None` for prefixes never priced through checkout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paid_print_mode: Option<PrintMode>,
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

/// One printable card. `id` is an index within its prefix, not global.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Card {
    pub id: i64,
    pub name: String,
    pub serial_number: Option<String>,
    /// Black & white only back-side text (half panel, saves ribbon).
    pub back_text: Option<String>,
    pub image_content_type: String,
    /// Non-None only when this card has an uploaded colour back image
    /// (`double_sided_color` mode). Never serialized from the path; this holds
    /// the stored content-type purely as the signal that a back image exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub back_image_content_type: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// When the printer confirmed this card was ejected. Operator-only state
    /// for the print spooler; deliberately not serialized into the public Card
    /// JSON, so neither the customer view nor the admin print preview sees it.
    #[serde(skip)]
    pub printed_at: Option<String>,
}

/// One card as the print spooler sees it: the operator-only `printed_at` is
/// included here, unlike in the public `Card`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PrintQueueCard {
    pub id: i64,
    pub name: String,
    pub printed_at: Option<String>,
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
    /// The mode the spooler must print with: the one that was paid for
    /// (`paid_print_mode`), falling back to the prefix's current mode for
    /// prefixes never priced through checkout.
    pub print_mode: PrintMode,
    pub cards: Vec<PrintQueueCard>,
    pub unprinted_count: i64,
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
/// `back_text` is only valid in `color_front_black_back` mode; a `back_image`
/// file part is only valid (and, there, required) in `double_sided_color` mode.
/// The handler rejects a back field that does not fit the prefix's mode rather
/// than silently storing it.
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[allow(dead_code)]
pub struct CardInput {
    pub name: String,
    pub serial_number: Option<String>,
    pub back_text: Option<String>,
    pub back_image: Option<String>,
}

/// Body for creating a prefix. If `uuid` is omitted the server assigns one.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PrefixInput {
    pub uuid: Option<Uuid>,
    /// Ribbon-pass scheme. Omitted (or null) means the default
    /// `color_front_black_back`.
    pub print_mode: Option<PrintMode>,
}

/// Body for changing a prefix's print mode while it is still writable.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PrefixModePatch {
    pub print_mode: PrintMode,
}

/// Metadata-only card update (no image re-upload). Omitted fields are left
/// unchanged; an empty string clears an optional field.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct CardPatch {
    pub name: Option<String>,
    pub serial_number: Option<String>,
    pub back_text: Option<String>,
}

/// Body for a status transition (shop / print-queue side).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct StatusInput {
    pub status: PrefixStatus,
    /// Optional external order reference (typically set when marking paid).
    pub order_ref: Option<String>,
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

/// One print mode's price, as the shop config serves it.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModePrice {
    pub print_mode: PrintMode,
    /// Price of one card in this mode, in the currency's minor unit (cents).
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
    /// Price of one card per print mode, in the currency's minor unit (cents).
    pub prices: Vec<ModePrice>,
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
