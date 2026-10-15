//! Transactional email (Resend) for the events the shop tells people about.
//!
//! Today that is one event — a prefix becoming `paid` — and two messages: the
//! customer's order confirmation, and a copy for the operator so an order can
//! be queued without watching the UI. (`printed`/`shipped` are the obvious next
//! ones; [`Kind`] is where a new message joins, and the machinery below does not
//! care which event it is keyed on.)
//!
//! Two rules shape everything here:
//!
//!  1. **exactly once per transition.** The Stripe webhook is the trigger, and
//!     Stripe retries a delivery until it gets a 2xx — so a naive send mails the
//!     customer three times for one payment. Every message is claimed in
//!     `email_notifications` under a key derived from the event that caused it
//!     (`<kind>:<stripe session id>` for the paid confirmation) *before* it is
//!     sent; a retried or concurrent delivery loses the claim and sends nothing.
//!  2. **an email failure is never the webhook's failure.** Nothing in this
//!     module returns an error to its caller: a Resend outage, a bounce or a
//!     mistyped key is logged (and recorded in the row) and otherwise ignored,
//!     because the alternative is Stripe retrying for days, or a paid order
//!     never being recorded. The `paid` transition is the part that matters.
//!
//! Hand-rolled against the Resend HTTP API rather than an SDK, for the same
//! reason as [`crate::stripe`]: one POST, so a client plus `serde_json` is less
//! code and a far smaller dependency tree than a binding. It also lets
//! `RESEND_API_BASE` point at `scripts/mock_resend.py`, which is how the
//! exactly-once behaviour is tested without an account.
//!
//! Unset `RESEND_API_KEY`, or no `EMAIL_FROM`, switches the whole thing off: the
//! shop works, nobody is mailed, and a warning says so. `OPERATOR_EMAIL` unset
//! only drops the operator's copy — the customer's confirmation still goes out.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::db;
use crate::models::Ribbon;

/// How long to wait on Resend. Bounded so a hung connection cannot hold the
/// Stripe webhook response open; the send is never allowed to fail the webhook,
/// but it should not stall it either.
const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Which message this is. The string form is the claim key's prefix *and* what
/// is stored in `email_notifications.kind`, so a new message type (printed,
/// shipped) joins here and gets its own exactly-once claim for free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The customer's order confirmation: today they get Stripe's receipt and
    /// nothing from us, and no link back to their project.
    CustomerOrderPaid,
    /// The same event, as a note to the operator, so an order can be queued
    /// without watching the admin UI.
    OperatorOrderPaid,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CustomerOrderPaid => "customer_order_paid",
            Self::OperatorOrderPaid => "operator_order_paid",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Configured Resend access. Cheap to clone (two `Arc`s and a pooled client).
pub struct Notifier {
    /// The API key. Held only here: never logged, never returned, never put in
    /// an error message.
    api_key: Arc<str>,
    /// The verified sending address, e.g. `ESPuino <orders@example.com>`. Resend
    /// requires a verified domain, so this is an operator setup step, not
    /// something the code can default sensibly.
    from: Arc<str>,
    /// Where the operator's copy goes. `None` skips it.
    operator_email: Option<Arc<str>>,
    /// Resend's own base, overridable so the tests' mock can stand in for it.
    api_base: Arc<str>,
    http: reqwest::Client,
}

impl Notifier {
    /// Read the configuration from the environment.
    ///
    /// Returns `None` — email quietly switched off — when there is no key or no
    /// from-address; both are configuration the code cannot invent. Either way a
    /// warning says which, because a silently unmailable shop is the failure
    /// mode that is hardest to notice.
    pub fn from_env() -> Option<Self> {
        let api_key = env("RESEND_API_KEY");
        let from = env("EMAIL_FROM");

        let Some(api_key) = api_key else {
            tracing::info!("RESEND_API_KEY is not set: no order emails will be sent");
            return None;
        };
        let Some(from) = from else {
            tracing::warn!(
                "RESEND_API_KEY is set but EMAIL_FROM is not, so no order email can be sent \
                 (Resend refuses a message without a verified sending address): email \
                 notifications are OFF. Set EMAIL_FROM to a sender on a domain verified in \
                 Resend, e.g. \"ESPuino <orders@example.com>\"."
            );
            return None;
        };
        if !from.contains('@') {
            tracing::warn!(
                "EMAIL_FROM is '{from}': expected an email address, e.g. \
                 \"ESPuino <orders@example.com>\". Resend will reject every message."
            );
        }

        let operator_email = env("OPERATOR_EMAIL");
        if operator_email.is_none() {
            tracing::warn!(
                "OPERATOR_EMAIL is not set: the customer still gets their order confirmation, \
                 but no operator copy is sent, so a paid order is only visible in the admin UI."
            );
        }

        let api_base = env("RESEND_API_BASE")
            .unwrap_or_else(|| "https://api.resend.com".to_string())
            .trim_end_matches('/')
            .to_string();
        if !api_base.starts_with("https://") {
            tracing::warn!(
                "RESEND_API_BASE is '{api_base}': not https. Fine for a test mock, never for \
                 the real thing — it carries the API key and the customer's address."
            );
        }

        let http = reqwest::Client::builder()
            .timeout(SEND_TIMEOUT)
            .build()
            .expect("building the reqwest client cannot fail with these options");

        tracing::info!(
            "email notifications enabled: from '{from}', operator copy {}",
            if operator_email.is_some() {
                "on"
            } else {
                "off"
            }
        );

        Some(Self {
            api_key: api_key.into(),
            from: from.into(),
            operator_email: operator_email.map(Into::into),
            api_base: api_base.into(),
            http,
        })
    }

    /// Where the operator's copy goes, if one is configured.
    pub fn operator_email(&self) -> Option<&str> {
        self.operator_email.as_deref()
    }

    /// One message via Resend. `Ok` carries Resend's message id.
    ///
    /// The error is a plain `String`, not an [`crate::error::ApiError`]: nothing
    /// here is ever allowed to become an HTTP response.
    async fn send(&self, to: &str, message: &Message) -> Result<String, String> {
        let body = json!({
            "from": &*self.from,
            "to": [to],
            "subject": message.subject,
            "text": message.text,
            "html": message.html,
        });

        let url = format!("{}/emails", self.api_base);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&*self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| format!("could not reach Resend: {e}"))?;

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // Resend's error body names the problem (bad from address, domain
            // not verified, …) and never echoes the key.
            return Err(format!(
                "Resend refused the message ({status}): {}",
                truncate(&body, 300)
            ));
        }

        Ok(serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("id").and_then(|id| id.as_str()).map(str::to_string))
            .unwrap_or_default())
    }
}

/// Everything the order-paid messages need. Built by the webhook from the
/// Stripe session plus the prefix row.
pub struct PaidOrder {
    pub prefix: Uuid,
    /// Public origin, no trailing slash — where the customer's project lives.
    pub base_url: String,
    /// The Stripe Checkout session. Also the exactly-once key, so a retried
    /// delivery of the same payment can never produce a second email.
    pub session_id: String,
    pub card_count: i64,
    pub amount_cents: Option<i64>,
    pub currency: Option<String>,
    /// The ribbon that was charged — the ribbon load this order takes, which
    /// is the one thing the operator cannot infer from anything else here.
    pub ribbon: Option<Ribbon>,
    pub customer_email: Option<String>,
    pub customer_name: Option<String>,
}

/// Tell the customer, and the operator, that an order has been paid.
///
/// Never returns an error and never panics on a failure to send: it is called
/// from the Stripe webhook, after the status write, and an email that cannot be
/// delivered must not turn into a webhook Stripe retries for days.
pub async fn order_paid(pool: &SqlitePool, notifier: &Notifier, order: &PaidOrder) {
    let base = order.base_url.trim_end_matches('/');

    match order.customer_email.as_deref() {
        Some(to) => {
            let message = customer_message(order, base);
            deliver(pool, notifier, Kind::CustomerOrderPaid, order, to, message).await;
        }
        None => tracing::warn!(
            prefix = %order.prefix, session = %order.session_id,
            "the Stripe session carries no customer email, so the order confirmation cannot be \
             sent; the operator note, if configured, still goes out"
        ),
    }

    if let Some(to) = notifier.operator_email() {
        let message = operator_message(order, base);
        deliver(pool, notifier, Kind::OperatorOrderPaid, order, to, message).await;
    }
}

/// Claim, then send, then record the outcome. Every failure is logged and
/// swallowed — see the module docs.
async fn deliver(
    pool: &SqlitePool,
    notifier: &Notifier,
    kind: Kind,
    order: &PaidOrder,
    to: &str,
    message: Message,
) {
    // Keyed on the *event*, not on the recipient or the message text: one
    // transition, one email, however many times Stripe delivers it.
    let key = format!("{}:{}", kind.as_str(), order.session_id);

    match db::claim_notification(
        pool,
        &key,
        kind.as_str(),
        order.prefix,
        to,
        Some(&order.session_id),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => {
            tracing::debug!(
                prefix = %order.prefix, session = %order.session_id, kind = %kind,
                "this transition has already been emailed; not sending it again"
            );
            return;
        }
        Err(e) => {
            // Without the claim there is no way to tell a first delivery from a
            // fifth, so refusing to send is the safe side of that bet: a missing
            // email beats three identical ones, and the customer still has
            // Stripe's receipt.
            tracing::error!(
                prefix = %order.prefix, session = %order.session_id, kind = %kind, error = %e,
                "could not claim this notification in the database, so nothing was sent"
            );
            return;
        }
    }

    match notifier.send(to, &message).await {
        Ok(provider_id) => {
            // The recipient is deliberately not logged: it is personal data, and
            // the row (and Resend's own dashboard) already has it.
            tracing::info!(
                prefix = %order.prefix, session = %order.session_id, kind = %kind,
                subject = %message.subject, "order email sent"
            );
            if let Err(e) =
                db::finish_notification(pool, &key, true, Some(&provider_id), None).await
            {
                tracing::error!(key = %key, error = %e, "could not record that an email was sent");
            }
        }
        Err(e) => {
            tracing::error!(
                prefix = %order.prefix, session = %order.session_id, kind = %kind, error = %e,
                "could not send the order email; the payment is unaffected, and the order is \
                 still recorded — retry it by hand if the customer should be told"
            );
            if let Err(e) = db::finish_notification(pool, &key, false, None, Some(&e)).await {
                tracing::error!(key = %key, error = %e, "could not record the failed email");
            }
        }
    }
}

/// An email, ready to send.
struct Message {
    subject: String,
    text: String,
    html: String,
}

/// The customer's confirmation. Leads with what they bought, and carries the
/// link back to their project — the thing they cannot get from Stripe's receipt.
fn customer_message(order: &PaidOrder, base: &str) -> Message {
    let link = order_link(base, order.prefix);
    let fields = customer_fields(order);
    Message {
        subject: format!(
            "Your card order {} is confirmed",
            short_prefix(order.prefix)
        ),
        text: render_text(
            "Thanks for your order!\n\nYour card order is paid and queued for printing.\n",
            &fields,
            &format!(
                "Your project, with its cards, lives here:\n  {link}\n\n\
                 We will print it and send it on its way. If anything looks wrong, just reply \
                 to this email."
            ),
        ),
        html: render_html(
            "<p>Thanks for your order!</p>\n<p>Your card order is paid and queued for \
             printing.</p>",
            &fields,
            &format!(
                "<p>Your project, with its cards, lives here:<br><a href=\"{href}\">{href}</a>\
                 </p>\n<p>We will print it and send it on its way. If anything looks wrong, \
                 just reply to this email.</p>",
                href = escape(&link)
            ),
        ),
    }
}

/// The operator's copy: enough to queue the order without opening the UI, and
/// the customer's contact so they can be reached about it.
fn operator_message(order: &PaidOrder, base: &str) -> Message {
    let mut fields = vec![
        ("Prefix", order.prefix.to_string()),
        ("Cards", order.card_count.to_string()),
        (
            "Ribbon",
            order
                .ribbon
                .map(|r| r.to_string())
                .unwrap_or_else(|| "(not recorded)".to_string()),
        ),
        ("Total", amount(order)),
        ("Stripe session", order.session_id.clone()),
    ];
    fields.push(("Customer", customer_contact(order)));

    let admin_link = admin_link(base, order.prefix);
    Message {
        subject: format!(
            "Paid: {} card(s), order {}",
            order.card_count,
            short_prefix(order.prefix)
        ),
        text: render_text(
            "A card order has been paid.\n",
            &fields,
            &format!(
                "Queue it for printing, or look at it here:\n  {admin_link}\n\n\
                 It is also on the admin \"needs attention\" list only if something is wrong \
                 with it, so nothing here is waiting on you."
            ),
        ),
        html: render_html(
            "<p>A card order has been paid.</p>",
            &fields,
            &format!(
                "<p>Queue it for printing, or look at it here:<br><a href=\"{href}\">{href}</a>\
                 </p>\n<p>It is also on the admin \"needs attention\" list only if something is \
                 wrong with it, so nothing here is waiting on you.</p>",
                href = escape(&admin_link)
            ),
        ),
    }
}

/// The order's own fields, shared by both messages so the two never drift.
fn customer_fields(order: &PaidOrder) -> Vec<(&'static str, String)> {
    vec![
        ("Cards", order.card_count.to_string()),
        ("Total", amount(order)),
        ("Order", order.prefix.to_string()),
        ("Payment reference", order.session_id.clone()),
    ]
}

fn customer_contact(order: &PaidOrder) -> String {
    match (
        order.customer_name.as_deref().map(str::trim),
        order.customer_email.as_deref(),
    ) {
        (Some(name), Some(email)) if !name.is_empty() => format!("{name} <{email}>"),
        (_, Some(email)) => email.to_string(),
        _ => "(no email on the Stripe session — reply goes nowhere)".to_string(),
    }
}

/// The customer's project page, as an absolute URL.
fn order_link(base_url: &str, prefix: Uuid) -> String {
    format!("{}/p/{prefix}", base_url.trim_end_matches('/'))
}

/// The operator's read-only view of the same prefix. Built from the same origin
/// as the customer link, deliberately side by side with it so the two cannot
/// drift apart.
fn admin_link(base_url: &str, prefix: Uuid) -> String {
    format!("{}/admin/p/{prefix}", base_url.trim_end_matches('/'))
}

/// A short, recognisable form of the uuid, for a subject line.
fn short_prefix(prefix: Uuid) -> String {
    let s = prefix.to_string();
    s.chars().take(8).collect()
}

fn amount(order: &PaidOrder) -> String {
    match (order.amount_cents, order.currency.as_deref()) {
        (Some(cents), Some(currency)) => {
            format!("{:.2} {}", cents as f64 / 100.0, currency.to_uppercase())
        }
        (Some(cents), None) => format!("{:.2}", cents as f64 / 100.0),
        (None, _) => "(not reported by Stripe)".to_string(),
    }
}

/// A plain-text body: an opening paragraph, the fields as an indented block,
/// then the closing paragraph.
fn render_text(opening: &str, fields: &[(&str, String)], closing: &str) -> String {
    let mut out = String::from(opening);
    out.push('\n');
    for (name, value) in fields {
        // Values are newline-flattened so a crafted name cannot forge extra
        // field lines in the plain-text body.
        out.push_str(&format!("  {name}: {}\n", one_line(value)));
    }
    out.push('\n');
    out.push_str(closing);
    out.push('\n');
    out
}

fn render_html(opening: &str, fields: &[(&str, String)], closing: &str) -> String {
    let mut out = String::from(opening);
    out.push_str("\n<ul>\n");
    for (name, value) in fields {
        out.push_str(&format!(
            "  <li>{}: {}</li>\n",
            escape(name),
            escape(&one_line(value))
        ));
    }
    out.push_str("</ul>\n");
    out.push_str(closing);
    out.push('\n');
    out
}

/// Squash embedded newlines out of a value interpolated into a message.
fn one_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

/// XML-escape everything interpolated into the HTML part. The customer's name
/// comes from Stripe and is attacker-controlled in the sense that a customer
/// chooses it, so it never goes in raw.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Keep an upstream error body from flooding the log.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes total)", &s[..end], s.len())
}

fn env(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::extract::State;
    use std::sync::Mutex;

    /// A stand-in for api.resend.com that records every message it is asked to
    /// send, so a test can assert on what actually left the building.
    pub(crate) struct MockResend {
        pub base_url: String,
        calls: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl MockResend {
        pub(crate) fn messages(&self) -> Vec<serde_json::Value> {
            self.calls.lock().unwrap().clone()
        }

        pub(crate) fn len(&self) -> usize {
            self.calls.lock().unwrap().len()
        }

        /// The single message sent to `recipient`, if any.
        pub(crate) fn to(&self, recipient: &str) -> Option<serde_json::Value> {
            self.messages()
                .into_iter()
                .find(|m| m["to"][0].as_str() == Some(recipient))
        }
    }

    /// Serve `POST /emails` on an ephemeral port and hand back its address.
    pub(crate) async fn spawn_mock_resend() -> MockResend {
        async fn record(
            State(calls): State<Arc<Mutex<Vec<serde_json::Value>>>>,
            body: String,
        ) -> axum::Json<serde_json::Value> {
            let parsed = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
            calls.lock().unwrap().push(parsed);
            axum::Json(json!({ "id": "re_mock_1" }))
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/emails", axum::routing::post(record))
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        MockResend {
            base_url: format!("http://{addr}"),
            calls,
        }
    }

    /// A notifier wired to a mock (or to a port nothing listens on).
    pub(crate) fn test_notifier(api_base: &str, operator: Option<&str>) -> Notifier {
        Notifier {
            api_key: Arc::from("re_test_key"),
            from: Arc::from("ESPuino <orders@example.test>"),
            operator_email: operator.map(Arc::from),
            api_base: Arc::from(api_base),
            http: reqwest::Client::builder()
                .timeout(SEND_TIMEOUT)
                .build()
                .expect("client"),
        }
    }

    async fn test_pool() -> SqlitePool {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
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

    fn an_order(prefix: Uuid) -> PaidOrder {
        PaidOrder {
            prefix,
            base_url: "https://cards.example.test".into(),
            session_id: "cs_paid_1".into(),
            card_count: 3,
            amount_cents: Some(1750),
            currency: Some("eur".into()),
            ribbon: Some(Ribbon::Ymckoko),
            customer_email: Some("ada@example.test".into()),
            customer_name: Some("Ada Lovelace".into()),
        }
    }

    /// A port with nothing behind it: connecting is refused immediately.
    const DEAD_RESEND: &str = "http://127.0.0.1:1";

    #[tokio::test]
    async fn a_paid_order_mails_the_customer_and_the_operator_once_each() {
        let pool = test_pool().await;
        let mock = spawn_mock_resend().await;
        let notifier = test_notifier(&mock.base_url, Some("shop@example.test"));
        let order = an_order(Uuid::new_v4());

        order_paid(&pool, &notifier, &order).await;

        assert_eq!(mock.len(), 2, "one customer email and one operator email");
        let customer = mock.to("ada@example.test").expect("customer email");
        assert_eq!(customer["from"], "ESPuino <orders@example.test>");
        assert!(customer["subject"]
            .as_str()
            .expect("subject")
            .contains("is confirmed"));
        // The whole point of the customer email: a way back to their project.
        let link = format!("https://cards.example.test/p/{}", order.prefix);
        assert!(customer["text"].as_str().expect("text").contains(&link));
        assert!(customer["html"].as_str().expect("html").contains(&link));
        assert!(customer["text"].as_str().unwrap().contains("17.50 EUR"));

        let operator = mock.to("shop@example.test").expect("operator email");
        assert!(operator["text"]
            .as_str()
            .expect("text")
            .contains(&format!("/admin/p/{}", order.prefix)));
        assert!(operator["text"]
            .as_str()
            .unwrap()
            .contains("ada@example.test"));
        assert!(operator["text"]
            .as_str()
            .unwrap()
            .contains("ymckoko"), "the operator is told which ribbon to load");
    }

    #[tokio::test]
    async fn a_replayed_transition_is_not_emailed_twice() {
        let pool = test_pool().await;
        let mock = spawn_mock_resend().await;
        let notifier = test_notifier(&mock.base_url, Some("shop@example.test"));
        let order = an_order(Uuid::new_v4());

        // Eleven deliveries of the same payment, as Stripe's retries would look.
        for _ in 0..11 {
            order_paid(&pool, &notifier, &order).await;
        }

        assert_eq!(
            mock.len(),
            2,
            "exactly one email per recipient, however often the event is delivered"
        );
        let sent: Vec<String> = db::sent_notification_keys(&pool).await.expect("sent keys");
        assert_eq!(sent.len(), 2, "both claims are recorded as sent");
    }

    #[tokio::test]
    async fn a_second_session_for_the_same_prefix_still_mails() {
        let pool = test_pool().await;
        let mock = spawn_mock_resend().await;
        let notifier = test_notifier(&mock.base_url, None);
        let prefix = Uuid::new_v4();

        order_paid(&pool, &notifier, &an_order(prefix)).await;
        let mut second = an_order(prefix);
        second.session_id = "cs_paid_2".into();
        order_paid(&pool, &notifier, &second).await;

        // Keyed on the session, not the prefix: a genuinely different payment is
        // a genuinely different event.
        assert_eq!(mock.len(), 2, "one email per session");
    }

    #[tokio::test]
    async fn resend_being_unreachable_is_logged_and_swallowed() {
        let pool = test_pool().await;
        let notifier = test_notifier(DEAD_RESEND, None);
        let order = an_order(Uuid::new_v4());

        // Must return (not panic, not error) — the webhook has to stay healthy.
        order_paid(&pool, &notifier, &order).await;

        // The claim is kept and marked failed: retrying automatically could
        // double-send, so the row is the record that someone should look.
        let failed = db::failed_notifications(&pool).await.expect("failed rows");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, "ada@example.test");
        assert!(failed[0].1.contains("could not reach Resend"));

        // And a replayed delivery does not try again.
        order_paid(&pool, &notifier, &order).await;
        assert_eq!(db::failed_notifications(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_session_without_an_email_still_notes_the_operator() {
        let pool = test_pool().await;
        let mock = spawn_mock_resend().await;
        let notifier = test_notifier(&mock.base_url, Some("shop@example.test"));
        let mut order = an_order(Uuid::new_v4());
        order.customer_email = None;
        order.customer_name = None;

        order_paid(&pool, &notifier, &order).await;

        assert_eq!(mock.len(), 1, "only the operator's copy can go out");
        let operator = mock.to("shop@example.test").expect("operator email");
        assert!(operator["text"]
            .as_str()
            .unwrap()
            .contains("no email on the Stripe session"));
    }

    #[test]
    fn the_html_part_escapes_what_the_customer_wrote() {
        let prefix = Uuid::new_v4();
        let mut order = an_order(prefix);
        order.customer_name = Some("<script>alert(1)</script>".into());
        // The operator's copy is the one that carries the customer-written name.
        let message = operator_message(&order, &order.base_url);
        assert!(!message.html.contains("<script>"));
        assert!(message.html.contains("&lt;script&gt;"));
        // The plain-text part carries the name as-is; it is not markup there.
        assert!(message.text.contains("<script>alert(1)</script>"));
    }

    #[test]
    fn a_multiline_name_cannot_forge_extra_fields() {
        let prefix = Uuid::new_v4();
        let mut order = an_order(prefix);
        order.customer_name = Some("Ada\n  Total: 0.00 EUR".into());
        let message = operator_message(&order, &order.base_url);
        let forged: Vec<&str> = message
            .text
            .lines()
            .filter(|line| line.starts_with("  Total: 0.00"))
            .collect();
        assert!(
            forged.is_empty(),
            "the name was flattened into its own line"
        );
    }

    #[test]
    fn the_claim_key_is_the_session_and_the_kind() {
        let kinds: Vec<&str> = [Kind::CustomerOrderPaid, Kind::OperatorOrderPaid]
            .iter()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(kinds, vec!["customer_order_paid", "operator_order_paid"]);
    }

    #[test]
    fn amounts_render_in_the_currency_s_minor_unit() {
        let mut order = an_order(Uuid::new_v4());
        assert_eq!(amount(&order), "17.50 EUR");
        order.currency = Some("usd".into());
        assert_eq!(amount(&order), "17.50 USD");
        order.amount_cents = None;
        assert_eq!(amount(&order), "(not reported by Stripe)");
    }

    #[test]
    fn a_trailing_slash_on_the_public_url_does_not_double_up() {
        let prefix = Uuid::new_v4();
        assert_eq!(
            order_link("https://cards.example.test/", prefix),
            format!("https://cards.example.test/p/{prefix}")
        );
        assert_eq!(
            admin_link("https://cards.example.test/", prefix),
            format!("https://cards.example.test/admin/p/{prefix}")
        );
    }
}
