//! Stripe Checkout + webhook handling.
//!
//! Hand-rolled rather than built on an SDK: the surface we need is one API call
//! (create a Checkout Session) and one signature check, so an HTTP client plus
//! an HMAC is less code and a far smaller dependency tree than a full API
//! binding — and it lets `STRIPE_API_BASE` point the client at a local mock,
//! which is how the request shape is verified without a Stripe account.
//!
//! The flow it supports:
//!
//! 1. the customer clicks Pay on a draft prefix in the web UI — there is no
//!    submit step, so nothing pre-marks the order;
//! 2. the UI calls `POST /api/prefixes/{prefix}/checkout`, which moves the
//!    prefix to `payment_pending`, calls [`Stripe::create_checkout_session`],
//!    and redirects the browser to the returned `url`, where Stripe collects the
//!    shipping address and the money;
//! 3. Stripe calls `POST /api/webhooks/stripe`, whose signature is checked by
//!    [`Stripe::verify_webhook`], and the prefix becomes `paid` — with the
//!    shipping address Checkout collected stored on it, so shipping the order
//!    never means reading the Stripe dashboard.
//!
//! The redirect in step 2 is *not* proof of payment: it can arrive before the
//! webhook does, and a customer can forge it by hand. Only step 3 changes state.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::ApiError;
use crate::models::ShippingAddress;

/// How old a webhook signature may be before we reject it, so a captured
/// request cannot be replayed indefinitely. Stripe's own default is 5 minutes.
const SIGNATURE_TOLERANCE_SECS: i64 = 300;

/// What the shop charges, and how the order is priced.
///
/// Amounts are in the currency's *minor* unit (cents), the way Stripe wants
/// them, so `500` with `eur` is €5.00. Per-ribbon prices — the price of one
/// billing unit, a printed side or a whole card depending on the ribbon — live
/// in the database (`ribbon_prices`), not here; this struct only carries the
/// shop-wide, deployment-level settings that are not per-ribbon.
#[derive(Debug, Clone)]
pub struct Pricing {
    pub currency: String,
    /// Flat shipping charge added as its own line. Zero omits the line.
    pub shipping_cents: i64,
    /// Country codes the shipping address is allowed to be in.
    pub countries: Vec<String>,
}

impl Pricing {
    /// Order total in cents, from the price of one billing unit and how many
    /// units the order costs. Only used to render a price before paying — the
    /// authoritative figure is the one Stripe computes and reports back.
    pub fn total_cents(&self, unit_price_cents: i64, units: i64) -> i64 {
        unit_price_cents.max(0) * units.max(0) + self.shipping_cents.max(0)
    }
}

/// Configured Stripe access. Cheap to clone (three `Arc`s and a pooled client).
#[derive(Clone)]
pub struct Stripe {
    secret_key: Arc<str>,
    webhook_secret: Option<Arc<str>>,
    api_base: Arc<str>,
    public_base_url: Option<Arc<str>>,
    /// Methods to EXCLUDE from a session. Empty by default: with dynamic
    /// payment methods the offered list lives in the Stripe Dashboard, not in
    /// code, and this can only narrow it per session.
    excluded_payment_methods: Vec<String>,
    /// An optional `pmc_...` payment method configuration to pin this session
    /// to, for shops that need more than one set of methods.
    payment_method_configuration: Option<String>,
    pricing: Pricing,
    http: reqwest::Client,
}

impl Stripe {
    /// Read the configuration from the environment.
    ///
    /// Returns `None` when `STRIPE_SECRET_KEY` is unset or blank, which
    /// switches the payment endpoints off (they answer 503) rather than leaving
    /// them open — the same convention as `ADMIN_TOKEN`.
    pub fn from_env() -> Option<Self> {
        let secret_key = env("STRIPE_SECRET_KEY")?;

        let webhook_secret = env("STRIPE_WEBHOOK_SECRET");
        if webhook_secret.is_none() {
            tracing::warn!(
                "STRIPE_WEBHOOK_SECRET is not set: checkouts will work, but every webhook \
                 will be rejected and no prefix will ever become 'paid'. Copy the signing \
                 secret from your Stripe webhook endpoint (or `stripe listen`)."
            );
        }

        let mut countries: Vec<String> = env("STRIPE_COUNTRIES")
            .unwrap_or_else(|| "DE,AT,CH".to_string())
            .split(',')
            .map(|c| c.trim().to_ascii_uppercase())
            .filter(|c| c.len() == 2)
            .collect();
        if countries.is_empty() {
            countries = vec!["DE".to_string()];
        }

        // Stripe removed `payment_method_types` when creating a Checkout
        // Session: the offered methods are now dynamic, configured in the
        // Dashboard (Settings -> Payment methods), and chosen per session by
        // Stripe's own models. A session may only EXCLUDE methods, or be pinned
        // to a saved configuration. Sending the old parameter is a hard 400 —
        // which is why the stale variable gets a loud warning rather than
        // being silently ignored.
        if env("STRIPE_PAYMENT_METHODS").is_some() {
            tracing::warn!(
                "STRIPE_PAYMENT_METHODS is set but does nothing: Stripe removed the \
                 payment_method_types parameter from Checkout Sessions, and sending it \
                 fails the request with 400. Choose the methods you accept in the \
                 Dashboard (Settings -> Payment methods) instead, or use \
                 STRIPE_EXCLUDED_PAYMENT_METHODS to narrow them for this shop."
            );
        }

        let excluded_payment_methods: Vec<String> = env("STRIPE_EXCLUDED_PAYMENT_METHODS")
            .unwrap_or_default()
            .split(',')
            .map(|m| m.trim().to_ascii_lowercase())
            .filter(|m| !m.is_empty())
            .collect();

        let payment_method_configuration = env("STRIPE_PAYMENT_METHOD_CONFIGURATION");
        if let Some(pmc) = &payment_method_configuration {
            if !pmc.starts_with("pmc_") {
                tracing::warn!(
                    "STRIPE_PAYMENT_METHOD_CONFIGURATION is '{pmc}': expected a pmc_... id \
                     from the Dashboard. Stripe will reject every checkout."
                );
            }
        }

        let pricing = Pricing {
            currency: env("STRIPE_CURRENCY")
                .unwrap_or_else(|| "eur".to_string())
                .to_ascii_lowercase(),
            shipping_cents: env_i64("STRIPE_SHIPPING_CENTS").unwrap_or(0),
            countries,
        };

        let api_base = env("STRIPE_API_BASE")
            .unwrap_or_else(|| "https://api.stripe.com".to_string())
            .trim_end_matches('/')
            .to_string();
        if !api_base.starts_with("https://") {
            tracing::warn!(
                "STRIPE_API_BASE is '{api_base}': not https. Fine for a test mock, never \
                 for the real thing."
            );
        }
        if secret_key.starts_with("sk_live_") && api_base != "https://api.stripe.com" {
            tracing::warn!(
                "a live Stripe key is pointed at a non-production STRIPE_API_BASE \
                 ('{api_base}') - double-check that"
            );
        }

        let public_base_url = env("PUBLIC_BASE_URL").map(|u| u.trim_end_matches('/').to_string());
        if public_base_url.is_none() {
            tracing::warn!(
                "PUBLIC_BASE_URL is not set: the post-payment redirect will be built from \
                 the request's Host header, so a client can choose where Stripe sends the \
                 customer back to. Set it."
            );
        }

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("building the reqwest client cannot fail with these options");

        tracing::info!(
            "stripe payment enabled: currency {}, shipping {:.2}, excluded methods [{}], countries [{}]",
            pricing.currency.to_uppercase(),
            pricing.shipping_cents as f64 / 100.0,
            excluded_payment_methods.join(","),
            // Read through `pricing`, which already owns it.
            pricing.countries.join(",")
        );

        Some(Self {
            secret_key: secret_key.into(),
            webhook_secret: webhook_secret.map(Into::into),
            api_base: api_base.into(),
            public_base_url: public_base_url.map(Into::into),
            excluded_payment_methods,
            payment_method_configuration,
            pricing,
            http,
        })
    }

    pub fn pricing(&self) -> &Pricing {
        &self.pricing
    }

    pub fn excluded_payment_methods(&self) -> &[String] {
        &self.excluded_payment_methods
    }

    pub fn payment_method_configuration(&self) -> Option<&str> {
        self.payment_method_configuration.as_deref()
    }

    pub fn public_base_url(&self) -> Option<&str> {
        self.public_base_url.as_deref()
    }

    /// Create a Checkout Session for one prefix and return it.
    ///
    /// `base_url` is where Stripe sends the customer back to; it is used
    /// verbatim for both the success and the cancel URL, with `?paid=1`
    /// appended to the success one purely so the UI can say "processing".
    /// `units` is how many billing units the order costs — the session's line
    /// item quantity — and `price_cents` the price of one of them. What a unit
    /// *is* differs per ribbon, so `unit` names it for the line item: one
    /// printed side on YMCKO (which is what lets such an order mix single- and
    /// double-sided cards), or one whole card on YMCKOK (whose panel set is
    /// spent whether or not the second black panel is used).
    pub async fn create_checkout_session(
        &self,
        prefix: Uuid,
        units: i64,
        price_cents: i64,
        unit: &str,
        base_url: &str,
    ) -> Result<CheckoutSession, ApiError> {
        let page = format!("{base_url}/p/{prefix}");
        let p = &self.pricing;

        let mut form: Vec<(String, String)> = vec![
            ("mode".into(), "payment".into()),
            ("success_url".into(), format!("{page}?paid=1")),
            ("cancel_url".into(), page),
            // Both are set: client_reference_id is the documented field for
            // exactly this, metadata is the belt to its braces — and metadata
            // is what survives if this ever moves to Payment Links.
            ("client_reference_id".into(), prefix.to_string()),
            ("metadata[prefix]".into(), prefix.to_string()),
        ];

        // NOT `payment_method_types`: Stripe removed that parameter from
        // Checkout Session creation and answers 400 if it is sent. Which methods
        // are offered is dynamic and Dashboard-configured now; a session can
        // only exclude methods, or be pinned to a saved pmc_... configuration.
        if let Some(pmc) = &self.payment_method_configuration {
            form.push(("payment_method_configuration".into(), pmc.clone()));
        }
        for (i, method) in self.excluded_payment_methods.iter().enumerate() {
            form.push((
                format!("excluded_payment_method_types[{i}]"),
                method.clone(),
            ));
        }

        form.push(("line_items[0][quantity]".into(), units.to_string()));
        form.push((
            "line_items[0][price_data][currency]".into(),
            p.currency.clone(),
        ));
        form.push((
            "line_items[0][price_data][unit_amount]".into(),
            price_cents.to_string(),
        ));
        form.push((
            "line_items[0][price_data][product_data][name]".into(),
            // The unit is named, not assumed: "printed side" on YMCKO, "card"
            // on YMCKOK. A receipt reading "printed side × 2" for a one-card
            // YMCKOK order would be a lie about what was bought.
            format!("RFID/NFC card label ({unit})"),
        ));

        for (i, country) in p.countries.iter().enumerate() {
            form.push((
                format!("shipping_address_collection[allowed_countries][{i}]"),
                country.clone(),
            ));
        }

        if p.shipping_cents > 0 {
            form.push((
                "shipping_options[0][shipping_rate_data][type]".into(),
                "fixed_amount".into(),
            ));
            form.push((
                "shipping_options[0][shipping_rate_data][display_name]".into(),
                "Shipping".into(),
            ));
            form.push((
                "shipping_options[0][shipping_rate_data][fixed_amount][amount]".into(),
                p.shipping_cents.to_string(),
            ));
            form.push((
                "shipping_options[0][shipping_rate_data][fixed_amount][currency]".into(),
                p.currency.clone(),
            ));
        }

        let url = format!("{}/v1/checkout/sessions", self.api_base);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&*self.secret_key)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(encode_form(&form))
            .send()
            .await
            .map_err(|e| ApiError::Upstream(format!("could not reach Stripe: {e}")))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ApiError::Upstream(format!(
                "Stripe rejected the checkout session ({status}): {}",
                truncate(&body, 500)
            )));
        }

        serde_json::from_str(&body).map_err(|e| {
            ApiError::Upstream(format!(
                "unexpected Stripe response: {e}: {}",
                truncate(&body, 300)
            ))
        })
    }

    /// Verify a `Stripe-Signature` header against the raw request body.
    ///
    /// The header looks like `t=1700000000,v1=<hex>,v1=<hex>`; the signed
    /// payload is `"{t}.{body}"`, HMAC-SHA256 with the endpoint's signing
    /// secret. Several `v1` values may be present while a secret is being
    /// rotated, so any match is accepted.
    pub fn verify_webhook(&self, signature_header: &str, payload: &[u8]) -> Result<(), ApiError> {
        let Some(secret) = self.webhook_secret.as_deref() else {
            return Err(ApiError::Unavailable(
                "webhooks are disabled: STRIPE_WEBHOOK_SECRET is not set".into(),
            ));
        };
        verify_signature(secret, signature_header, payload, SIGNATURE_TOLERANCE_SECS)
    }
}

/// A Checkout Session, both as Stripe returns it and as we hand it to the UI.
/// Extra fields Stripe sends are ignored.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CheckoutSession {
    pub id: String,
    /// Where to send the browser. `None` only for sessions created through
    /// other Stripe flows.
    pub url: Option<String>,
    pub amount_total: Option<i64>,
    pub currency: Option<String>,
    pub payment_status: Option<String>,
}

/// The subset of a Stripe event we act on.
#[derive(Debug, Deserialize)]
pub struct Event {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: EventData,
    /// Whether this came from the live account or from test mode. Logged, not
    /// enforced — the signing secret already decides which account this is.
    #[serde(default)]
    pub livemode: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct EventData {
    pub object: SessionObject,
}

/// The session object nested in a `checkout.session.*` event.
#[derive(Debug, Deserialize)]
pub struct SessionObject {
    pub id: String,
    pub client_reference_id: Option<String>,
    pub payment_status: Option<String>,
    pub amount_total: Option<i64>,
    pub currency: Option<String>,
    /// Present on a Checkout Session that collected an address, which is where
    /// the customer's email actually is. This is what the order confirmation is
    /// addressed to.
    #[serde(default)]
    pub customer_details: Option<CustomerDetails>,
    /// The email Stripe was *prefilled* with, if any. Often absent, and only a
    /// fallback: `customer_details.email` is the one the customer confirmed.
    #[serde(default)]
    pub customer_email: Option<String>,
    /// The address Checkout collected — at the top level up to API version
    /// 2025-03-27, and under `collected_information` from 2025-03-31.basil on.
    /// Both are read, because which one arrives is a property of the webhook
    /// endpoint's configured API version rather than of this code. See
    /// [`SessionObject::shipping_address`].
    #[serde(default)]
    pub shipping_details: Option<ShippingDetails>,
    #[serde(default)]
    pub collected_information: Option<CollectedInformation>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

/// The shipping details block, in the shape Stripe sends it: the recipient's
/// name sitting beside the address rather than inside it.
#[derive(Debug, Clone, Deserialize)]
pub struct ShippingDetails {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub address: Option<PostalAddress>,
}

/// A postal address, every field nullable exactly as Stripe has it.
#[derive(Debug, Clone, Deserialize)]
pub struct PostalAddress {
    #[serde(default)]
    pub line1: Option<String>,
    #[serde(default)]
    pub line2: Option<String>,
    #[serde(default)]
    pub postal_code: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
}

/// The `collected_information` block, which is where the shipping details moved
/// in API version 2025-03-31.basil.
#[derive(Debug, Clone, Deserialize)]
pub struct CollectedInformation {
    #[serde(default)]
    pub shipping_details: Option<ShippingDetails>,
}

/// Who paid. Checkout collects the email, so this is how the shop learns where
/// to send an order confirmation — there are no accounts here.
#[derive(Debug, Deserialize)]
pub struct CustomerDetails {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

impl SessionObject {
    /// Where to send the customer's confirmation.
    ///
    /// `customer_details.email` is the address the customer entered at Stripe;
    /// `customer_email` is only the prefilled one, so it is a fallback rather
    /// than an equal. Blank values are treated as absent, so an empty
    /// `customer_details.email` still lets the fallback through.
    pub fn payer_email(&self) -> Option<&str> {
        self.customer_details
            .as_ref()
            .and_then(|d| d.email.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.customer_email
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
    }

    /// The name on the payment, for the operator's copy. Never part of an
    /// address — it is customer-written text.
    pub fn payer_name(&self) -> Option<&str> {
        self.customer_details
            .as_ref()
            .and_then(|d| d.name.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// Which prefix this session was paid for.
    ///
    /// `client_reference_id` is the documented field and wins, but a *blank*
    /// one is treated as absent so the `metadata.prefix` fallback still gets a
    /// chance — otherwise a session with an empty reference would be silently
    /// unattributable while a perfectly good one sat right next to it.
    pub fn prefix_uuid(&self) -> Option<String> {
        let reference = self
            .client_reference_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        // `and_then`, not `?`: a `?` here would return early with None and skip
        // the primary path whenever a session carried no metadata at all.
        let from_metadata = self
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("prefix"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        reference.or(from_metadata).map(str::to_string)
    }

    /// Where to ship this order: the address Checkout collected, or `None` when
    /// it collected none.
    ///
    /// Two payload shapes are read, and that is deliberate. Up to API version
    /// 2025-03-27 the address is `shipping_details` at the top level of the
    /// session; from 2025-03-31.basil on it is
    /// `collected_information.shipping_details` (the top-level field was
    /// removed). Which one arrives depends on the API version configured on the
    /// *webhook endpoint*, which is not something this code sets — so reading
    /// only one of them means a version bump in the Dashboard silently stops
    /// stored addresses with no error anywhere. The top-level shape is looked
    /// at first, then the nested one.
    ///
    /// `None`, not an all-`None` struct, when Stripe sent nothing usable: a
    /// blank address is not a shippable order, and the caller must be able to
    /// tell it apart from a real one without re-deriving that check.
    pub fn shipping_address(&self) -> Option<ShippingAddress> {
        let details = self.shipping_details.as_ref().or_else(|| {
            self.collected_information
                .as_ref()
                .and_then(|c| c.shipping_details.as_ref())
        })?;
        let address = details.address.as_ref();
        let (line1, line2, postal_code, city, state, country) = match address {
            Some(a) => (
                clean(a.line1.as_deref()),
                clean(a.line2.as_deref()),
                clean(a.postal_code.as_deref()),
                clean(a.city.as_deref()),
                clean(a.state.as_deref()),
                clean(a.country.as_deref()),
            ),
            None => (None, None, None, None, None, None),
        };
        let address = ShippingAddress {
            name: clean(details.name.as_deref()),
            line1,
            line2,
            postal_code,
            city,
            state,
            country,
        };
        (!address.is_empty()).then_some(address)
    }
}

/// A field as Stripe sent it, with blank treated as absent: a field the
/// customer left empty comes back as `""`, and an empty string is not an
/// address line, a name, or a country code.
fn clean(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Check a Stripe signature header. Split out from [`Stripe`] so it can be
/// tested without any configuration or network.
pub fn verify_signature(
    secret: &str,
    header: &str,
    payload: &[u8],
    tolerance_secs: i64,
) -> Result<(), ApiError> {
    let mut timestamp: Option<i64> = None;
    let mut candidates: Vec<&str> = Vec::new();

    for part in header.split(',') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("t=") {
            timestamp = v.trim().parse().ok();
        } else if let Some(v) = part.strip_prefix("v1=") {
            candidates.push(v.trim());
        }
    }

    let timestamp = timestamp
        .ok_or_else(|| ApiError::BadRequest("Stripe-Signature: no usable t= field".into()))?;
    if candidates.is_empty() {
        return Err(ApiError::BadRequest(
            "Stripe-Signature: no v1 signature to check".into(),
        ));
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if (now - timestamp).abs() > tolerance_secs {
        return Err(ApiError::BadRequest(format!(
            "Stripe-Signature: timestamp {timestamp} is outside the {tolerance_secs}s tolerance \
             (replay, or a badly skewed clock)"
        )));
    }

    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| ApiError::Internal("webhook secret is unusable as an HMAC key".into()))?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(payload);
    let expected = mac.finalize().into_bytes();

    if candidates
        .iter()
        .any(|candidate| constant_time_eq_hex(&expected, candidate))
    {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "Stripe-Signature: no v1 signature matched".into(),
        ))
    }
}

/// Render form pairs the way Stripe's API expects: `a[b][0]=x&c=y`, with the
/// keys' brackets left literal and only the values escaped.
fn encode_form(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(key);
        out.push('=');
        out.push_str(&encode_component(value));
    }
    out
}

fn encode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Compare a raw digest against a hex string without an early exit.
fn constant_time_eq_hex(expected: &[u8], hex_candidate: &str) -> bool {
    let Some(decoded) = decode_hex(hex_candidate) else {
        return false;
    };
    if decoded.len() != expected.len() {
        return false;
    }
    decoded
        .iter()
        .zip(expected)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes = s.as_bytes();
    (0..bytes.len() / 2)
        .map(|i| {
            let hi = (bytes[i * 2] as char).to_digit(16)?;
            let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
            Some((hi * 16 + lo) as u8)
        })
        .collect()
}

/// Keep upstream error bodies from flooding the logs or the response.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}… ({} bytes total)", &s[..end], s.len())
    }
}

fn env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_i64(key: &str) -> Option<i64> {
    env(key).and_then(|v| match v.parse() {
        Ok(n) => Some(n),
        Err(_) => {
            tracing::warn!("{key} is not a whole number ('{v}'); ignoring it");
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_test_secret";

    /// Sign the way Stripe does, so the tests exercise the real construction
    /// rather than a hand-copied digest.
    fn sign(secret: &str, timestamp: i64, payload: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(timestamp.to_string().as_bytes());
        mac.update(b".");
        mac.update(payload);
        let digest = mac.finalize().into_bytes();
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn now() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    #[test]
    fn accepts_a_correctly_signed_payload() {
        let body = br#"{"id":"evt_1","type":"checkout.session.completed"}"#;
        // One `t` for both, or a clock tick between the two calls would make
        // the signature disagree with the timestamp it was sent with.
        let t = now();
        let header = format!("t={t},v1={}", sign(SECRET, t, body));
        assert!(verify_signature(SECRET, &header, body, 300).is_ok());
    }

    #[test]
    fn rejects_a_tampered_body() {
        let body = br#"{"id":"evt_1"}"#;
        let header = format!("t={},v1={}", now(), sign(SECRET, now(), body));
        let tampered = br#"{"id":"evt_2"}"#;
        assert!(verify_signature(SECRET, &header, tampered, 300).is_err());
    }

    #[test]
    fn rejects_a_wrong_secret() {
        let body = br#"{"id":"evt_1"}"#;
        let header = format!("t={},v1={}", now(), sign("whsec_other", now(), body));
        assert!(verify_signature(SECRET, &header, body, 300).is_err());
    }

    #[test]
    fn rejects_a_stale_timestamp() {
        let body = br#"{"id":"evt_1"}"#;
        let old = now() - 3600;
        let header = format!("t={old},v1={}", sign(SECRET, old, body));
        assert!(verify_signature(SECRET, &header, body, 300).is_err());
    }

    #[test]
    fn accepts_any_match_while_a_secret_rotates() {
        let body = br#"{"id":"evt_1"}"#;
        let t = now();
        let header = format!(
            "t={t},v1={},v1={}",
            sign("whsec_old", t, body),
            sign(SECRET, t, body)
        );
        assert!(verify_signature(SECRET, &header, body, 300).is_ok());
    }

    #[test]
    fn rejects_malformed_headers() {
        let body = br#"{}"#;
        assert!(verify_signature(SECRET, "", body, 300).is_err());
        assert!(verify_signature(SECRET, "v1=deadbeef", body, 300).is_err());
        assert!(verify_signature(SECRET, "t=notanumber,v1=deadbeef", body, 300).is_err());
        assert!(verify_signature(SECRET, &format!("t={}", now()), body, 300).is_err());
        assert!(verify_signature(SECRET, "t=,v1=", body, 300).is_err());
    }

    #[test]
    fn accepts_an_uppercase_hex_digest() {
        // Stripe always sends lower case, but a correct HMAC rendered in upper
        // case is still a correct HMAC, so there is nothing to gain by refusing
        // it. The test exists to pin that choice rather than leave it implied.
        let body = br#"{"id":"evt_1"}"#;
        let t = now();
        let upper = sign(SECRET, t, body).to_uppercase();
        assert!(verify_signature(SECRET, &format!("t={t},v1={upper}"), body, 300).is_ok());
    }

    #[test]
    fn hex_helpers_behave() {
        assert_eq!(decode_hex("00ff10").unwrap(), vec![0x00, 0xff, 0x10]);
        assert_eq!(decode_hex(""), Some(vec![]));
        assert!(decode_hex("abc").is_none());
        assert!(decode_hex("zz").is_none());
        assert!(constant_time_eq_hex(&[0xab, 0xcd], "abcd"));
        assert!(!constant_time_eq_hex(&[0xab, 0xcd], "abce"));
        assert!(!constant_time_eq_hex(&[0xab], "abcd"));
    }

    #[test]
    fn form_encoding_matches_stripe_bracket_notation() {
        let pairs = vec![
            ("mode".to_string(), "payment".to_string()),
            (
                "line_items[0][price_data][currency]".to_string(),
                "eur".to_string(),
            ),
            (
                "success_url".to_string(),
                "https://h/p/abc?paid=1".to_string(),
            ),
        ];
        let encoded = encode_form(&pairs);
        assert_eq!(
            encoded,
            "mode=payment&line_items[0][price_data][currency]=eur&\
             success_url=https%3A%2F%2Fh%2Fp%2Fabc%3Fpaid%3D1"
        );
        assert_eq!(encode_component("a b"), "a+b");
        assert_eq!(encode_component("50%"), "50%25");
    }

    #[test]
    fn pricing_totals_and_clamps() {
        let p = Pricing {
            currency: "eur".into(),
            shipping_cents: 250,
            countries: vec!["DE".into()],
        };
        assert_eq!(p.total_cents(500, 3), 1750); // three printed sides
        assert_eq!(p.total_cents(500, 0), 250);
        assert_eq!(p.total_cents(500, -5), 250);

        let free = Pricing {
            shipping_cents: 0,
            ..p
        };
        assert_eq!(free.total_cents(500, 2), 1000);
    }

    #[test]
    fn session_lookup_falls_back_to_metadata() {
        let with_ref = SessionObject {
            id: "cs_1".into(),
            client_reference_id: Some("abc".into()),
            payment_status: Some("paid".into()),
            amount_total: None,
            currency: None,
            customer_details: None,
            customer_email: None,
            shipping_details: None,
            collected_information: None,
            metadata: None,
        };
        assert_eq!(with_ref.prefix_uuid().as_deref(), Some("abc"));

        let mut from_meta = SessionObject {
            client_reference_id: Some("  ".into()),
            metadata: Some(serde_json::json!({"prefix": "def"})),
            ..with_ref
        };
        assert_eq!(from_meta.prefix_uuid().as_deref(), Some("def"));

        from_meta.metadata = Some(serde_json::json!({"other": "x"}));
        assert_eq!(from_meta.prefix_uuid(), None);
    }

    #[test]
    fn the_payer_email_comes_from_the_details_block() {
        // Exactly the shape Stripe sends: the email the customer confirmed at
        // Checkout lives in customer_details.
        let event: Event = serde_json::from_str(
            r#"{
                "id": "evt_1",
                "type": "checkout.session.completed",
                "data": { "object": {
                    "id": "cs_1",
                    "payment_status": "paid",
                    "client_reference_id": "00000000-0000-0000-0000-000000000000",
                    "customer_details": { "email": "ada@example.com", "name": "Ada Lovelace" },
                    "customer_email": "prefilled@example.com"
                }}
            }"#,
        )
        .expect("a plausible Stripe event");
        let session = &event.data.object;
        assert_eq!(session.payer_email(), Some("ada@example.com"));
        assert_eq!(session.payer_name(), Some("Ada Lovelace"));
    }

    #[test]
    fn the_payer_email_falls_back_to_the_prefilled_one_and_never_to_blank() {
        let mut session = SessionObject {
            id: "cs_1".into(),
            client_reference_id: None,
            payment_status: Some("paid".into()),
            amount_total: None,
            currency: None,
            customer_details: Some(CustomerDetails {
                email: Some("   ".into()),
                name: None,
            }),
            customer_email: Some("prefilled@example.com".into()),
            shipping_details: None,
            collected_information: None,
            metadata: None,
        };
        // A blank details email must not shadow a usable prefilled one.
        assert_eq!(session.payer_email(), Some("prefilled@example.com"));

        session.customer_email = None;
        assert_eq!(session.payer_email(), None);
        session.customer_details = None;
        assert_eq!(session.payer_email(), None);
        assert_eq!(session.payer_name(), None);
    }

    fn shipping_of(json: &str) -> Option<ShippingAddress> {
        let session: SessionObject =
            serde_json::from_str(json).expect("a plausible Stripe session object");
        session.shipping_address()
    }

    #[test]
    fn the_shipping_address_comes_from_the_top_level_field() {
        // Exactly the shape Stripe sent before 2025-03-31: the name beside the
        // address, the address with nullable fields, `state` absent on a German
        // one.
        let address = shipping_of(
            r#"{
                "id": "cs_1",
                "payment_status": "paid",
                "shipping_details": {
                    "name": "  Ada Lovelace  ",
                    "address": {
                        "line1": "Analytical Engine Way 1",
                        "line2": null,
                        "postal_code": "10115",
                        "city": "Berlin",
                        "state": null,
                        "country": "DE"
                    }
                }
            }"#,
        )
        .expect("the address Stripe collected");
        assert_eq!(address.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(address.line1.as_deref(), Some("Analytical Engine Way 1"));
        assert_eq!(address.line2, None);
        assert_eq!(address.postal_code.as_deref(), Some("10115"));
        assert_eq!(address.city.as_deref(), Some("Berlin"));
        assert_eq!(address.state, None);
        assert_eq!(address.country.as_deref(), Some("DE"));
    }

    #[test]
    fn the_shipping_address_is_read_from_the_nested_basil_shape_too() {
        // From API version 2025-03-31.basil the top-level `shipping_details` is
        // gone and the block lives under `collected_information`. Which shape
        // arrives is the webhook endpoint's configured version, so both are
        // read — a Dashboard version bump must not silently stop storing
        // addresses.
        let address = shipping_of(
            r#"{
                "id": "cs_1",
                "payment_status": "paid",
                "collected_information": {
                    "shipping_details": {
                        "name": "Ada Lovelace",
                        "address": { "line1": "Analytical Engine Way 1", "country": "DE" }
                    }
                }
            }"#,
        )
        .expect("the address Stripe collected");
        assert_eq!(address.name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(address.line1.as_deref(), Some("Analytical Engine Way 1"));
        assert_eq!(address.country.as_deref(), Some("DE"));
        assert_eq!(address.city, None);
    }

    #[test]
    fn the_top_level_shape_wins_when_both_are_present() {
        // Should not happen, but the precedence is documented, so pin it rather
        // than leave it to whichever order the code happens to be written in.
        let address = shipping_of(
            r#"{
                "id": "cs_1",
                "shipping_details": {
                    "name": "Top Level",
                    "address": { "country": "DE" }
                },
                "collected_information": {
                    "shipping_details": {
                        "name": "Nested",
                        "address": { "country": "FR" }
                    }
                }
            }"#,
        )
        .expect("an address");
        assert_eq!(address.name.as_deref(), Some("Top Level"));
        assert_eq!(address.country.as_deref(), Some("DE"));
    }

    #[test]
    fn an_address_stripe_did_not_collect_is_no_address() {
        // The three ways there is nothing to store: no block at all, an explicit
        // null, and a block whose every field the customer left blank. All must
        // read as `None`, because an all-blank address must never overwrite a
        // real one.
        for json in [
            r#"{"id": "cs_1", "payment_status": "paid"}"#,
            r#"{"id": "cs_1", "shipping_details": null}"#,
            r#"{"id": "cs_1", "shipping_details": {"name": "   ", "address": null}}"#,
            r#"{"id": "cs_1", "shipping_details": {"name": "", "address": {"line1": " ", "city": "", "country": "  "}}}"#,
            r#"{"id": "cs_1", "collected_information": {"shipping_details": null}}"#,
        ] {
            assert_eq!(shipping_of(json), None, "should be no address: {json}");
        }
    }

    #[test]
    fn a_partial_address_keeps_the_fields_it_has() {
        // A country-only address is thin but real, and dropping it would lose
        // the one part of the order we cannot infer from anywhere else.
        let address =
            shipping_of(r#"{"id": "cs_1", "shipping_details": {"address": {"country": "de"}}}"#)
                .expect("a thin but real address");
        assert_eq!(address.country.as_deref(), Some("de"));
        assert_eq!(address.name, None);
        assert!(!address.is_empty());
    }
}
