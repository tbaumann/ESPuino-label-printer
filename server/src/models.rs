use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Lifecycle of a prefix (one customer order).
///
/// `draft -> submitted -> paid -> queued -> printing -> printed`, with
/// `cancelled` reachable from any pre-printing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PrefixStatus {
    Draft,
    Submitted,
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
            Self::Submitted => "submitted",
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
            "submitted" => Self::Submitted,
            "paid" => Self::Paid,
            "queued" => Self::Queued,
            "printing" => Self::Printing,
            "printed" => Self::Printed,
            "cancelled" => Self::Cancelled,
            _ => return None,
        })
    }

    /// Cards may still be added/changed while the prefix is in these states.
    /// Once printing starts, the prefix is frozen.
    pub fn is_writable(self) -> bool {
        matches!(
            self,
            Self::Draft | Self::Submitted | Self::Paid | Self::Queued
        )
    }

    /// Allowed transitions.
    pub fn can_transition_to(self, next: Self) -> bool {
        use PrefixStatus::*;
        match self {
            Draft => matches!(next, Submitted | Cancelled),
            // A submission can be withdrawn back to draft before payment.
            Submitted => matches!(next, Paid | Draft | Cancelled),
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

/// A prefix groups one customer's set of cards (one order / label batch).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Prefix {
    pub uuid: Uuid,
    pub created_at: String,
    pub status: PrefixStatus,
    /// When the status last changed.
    pub status_changed_at: Option<String>,
    /// External shop/order reference, typically set when the order is paid.
    pub order_ref: Option<String>,
    /// Number of cards currently stored under this prefix.
    pub card_count: i64,
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
    pub created_at: String,
    pub updated_at: String,
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
#[derive(Debug, Clone, Deserialize, ToSchema)]
#[allow(dead_code)]
pub struct CardInput {
    pub name: String,
    pub serial_number: Option<String>,
    pub back_text: Option<String>,
}

/// Body for creating a prefix. If `uuid` is omitted the server assigns one.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PrefixInput {
    pub uuid: Option<Uuid>,
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