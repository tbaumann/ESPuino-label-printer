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

    /// The ribbon this mode is printed with, and therefore what has to be in
    /// stock for it to be offered.
    ///
    /// Single-sided and double-sided colour both run on YMCKO. Double-sided
    /// colour consumes two panel sets per card and so drains the stock twice as
    /// fast, but it is the same ribbon: the two disappear together, which is
    /// exactly why availability cannot be a per-mode flag.
    pub fn ribbon(self) -> Ribbon {
        match self {
            Self::SingleSided | Self::DoubleSidedColor => Ribbon::Ymcko,
            Self::ColorFrontBlackBack => Ribbon::Ymckoko,
        }
    }
}

/// A ribbon the shop prints with. Availability is a property of the ribbon, not
/// of the print mode — see [`PrintMode::ribbon`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Ribbon {
    /// Full colour, plus a black panel and overlay.
    Ymcko,
    /// YMCKO plus a second black panel for the back side.
    Ymckoko,
}

impl Ribbon {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ymcko => "ymcko",
            Self::Ymckoko => "ymckoko",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ymcko" => Self::Ymcko,
            "ymckoko" => Self::Ymckoko,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Ribbon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
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

/// One print mode as the shop offers it: its price, and whether it can be
/// ordered right now.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModePrice {
    pub print_mode: PrintMode,
    /// Price of one card in this mode, in the currency's minor unit (cents).
    pub price_cents: i64,
    /// Whether the mode's ribbon is in stock. An unavailable mode is still
    /// served, and rendered disabled rather than dropped, so the customer can
    /// see that the option exists and is out.
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
    /// Price of one card per print mode, in the currency's minor unit (cents),
    /// each carrying whether that mode can be ordered right now.
    pub prices: Vec<ModePrice>,
    /// The two stock flags behind those `available` answers. Availability is a
    /// property of the ribbon — single-sided and double-sided colour share
    /// YMCKO — so the flags are served as they are stored, and a mode's
    /// `available` is the derived consequence.
    pub ribbons: Vec<RibbonStock>,
    /// Flat shipping charge in cents; 0 means shipping is free and no shipping
    /// line is added to the checkout.
    pub shipping_cents: i64,
    /// Countries the shipping address may be in.
    pub countries: Vec<String>,
}
