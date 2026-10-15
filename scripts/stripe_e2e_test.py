#!/usr/bin/env python3
"""End-to-end test of the Stripe checkout and webhook, with no Stripe account.

It drives the real HTTP API: creates a prefix, uploads cards, asks for a checkout
session, reads back exactly what was sent to "Stripe" from the mock's recorder
file, and then delivers signed webhook events to the real endpoint. Signatures
are computed here the same way Stripe computes them, so the webhook path is
exercised for real rather than stubbed.

    python3 scripts/mock_stripe.py 12111 &
    python3 scripts/mock_resend.py 12112 &
    STRIPE_SECRET_KEY=sk_test_x STRIPE_WEBHOOK_SECRET=whsec_test_secret \
      STRIPE_API_BASE=http://127.0.0.1:12111 \
      PUBLIC_BASE_URL=http://127.0.0.1:8080 \
      STRIPE_SHIPPING_CENTS=250 \
      RESEND_API_KEY=re_test_x RESEND_API_BASE=http://127.0.0.1:12112 \
      EMAIL_FROM='ESPuino <orders@example.test>' OPERATOR_EMAIL=shop@example.test \
      ./target/debug/espuino-label-printer &
    python3 scripts/stripe_e2e_test.py http://127.0.0.1:8080

The order-email section (5b) runs only when `RESEND_API_KEY` and `EMAIL_FROM`
are set for both the server and this test; otherwise it is skipped with a note,
the same way the smoke test skips its Pay scenario. Its assertions are the two
that really matter for that feature: the customer's confirmation is addressed to
the address on the Stripe session and links back to their project, and a
*replayed* webhook delivery sends no second email.

The exit code is the number of failed checks.
"""

import hashlib
import hmac
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid
from typing import Any

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8080"
WEBHOOK_SECRET = os.environ.get("STRIPE_WEBHOOK_SECRET", "whsec_test_secret")
STRIPE_SECRET_KEY = os.environ.get("STRIPE_SECRET_KEY", "sk_test_x")
MOCK_OUT = os.environ.get("MOCK_STRIPE_OUT", "/tmp/mock_stripe_forms.jsonl")
MOCK_RESEND_OUT = os.environ.get("MOCK_RESEND_OUT", "/tmp/mock_resend_emails.jsonl")
PUBLIC_BASE = os.environ.get("PUBLIC_BASE_URL", BASE)
# The customer's address comes from the Stripe session, which this test builds
# itself, so the expectation is a constant rather than the server's environment.
CUSTOMER_EMAIL = "ada@example.test"
# The order emails are only sent when the server was given both a key and a
# from-address; without them the checks are skipped with a note, the same way
# the smoke test skips its Pay scenario when payment is not configured.
EMAILS_ON = bool(os.environ.get("RESEND_API_KEY")) and bool(os.environ.get("EMAIL_FROM"))
OPERATOR_EMAIL = os.environ.get("OPERATOR_EMAIL")
CARD_PNG = os.path.join(os.path.dirname(os.path.abspath(__file__)), "test-card.png")

checks = 0
failed: list[str] = []


def check(label: str, ok: Any, detail: str = "") -> None:
    global checks
    checks += 1
    if ok:
        print(f"  ok   {label}")
    else:
        failed.append(label)
        print(f"  FAIL {label}" + (f"   [{detail}]" if detail else ""))


def request(method: str, path: str, data: Any = None, headers: Any = None):
    req = urllib.request.Request(BASE + path, data=data, method=method)
    for key, value in (headers or {}).items():
        req.add_header(key, value)
    try:
        with urllib.request.urlopen(req) as res:
            return res.status, res.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()


def json_request(method: str, path: str, payload: Any = None, headers: Any = None) -> tuple[int, Any]:
    head = {"content-type": "application/json"}
    head.update(headers or {})
    body = json.dumps(payload if payload is not None else {}).encode()
    status, raw = request(method, path, data=body, headers=head)
    try:
        return status, json.loads(raw)
    except ValueError:
        return status, None


def multipart(fields: dict, filename: str, content: bytes, content_type: str = "image/png"):
    boundary = "----esplp" + uuid.uuid4().hex
    chunks = [
        f'--{boundary}\r\nContent-Disposition: form-data; name="{key}"\r\n\r\n{value}\r\n'.encode()
        for key, value in fields.items()
    ]
    chunks.append(
        f'--{boundary}\r\nContent-Disposition: form-data; name="image"; '
        f'filename="{filename}"\r\nContent-Type: {content_type}\r\n\r\n'.encode()
    )
    chunks.append(content)
    chunks.append(b"\r\n")
    chunks.append(f"--{boundary}--\r\n".encode())
    return b"".join(chunks), f"multipart/form-data; boundary={boundary}"


def sign(payload: bytes, secret: str | None = None, timestamp: int | None = None) -> str:
    stamp = int(time.time()) if timestamp is None else timestamp
    digest = hmac.new(
        (secret or WEBHOOK_SECRET).encode(), f"{stamp}.".encode() + payload, hashlib.sha256
    ).hexdigest()
    return f"t={stamp},v1={digest}"


def deliver(event: dict, signature: str | None = None, secret: str | None = None):
    body = json.dumps(event).encode()
    sig = signature if signature is not None else sign(body, secret)
    return request(
        "POST",
        "/api/webhooks/stripe",
        data=body,
        headers={"content-type": "application/json", "stripe-signature": sig},
    )


def payment_event(
    kind: str = "checkout.session.completed",
    prefix: Any = None,
    session: str = "cs_test_mock_1",
    payment_status: str = "paid",
    use_metadata: bool = False,
    customer_email: Any = CUSTOMER_EMAIL,
    amount_total: int = 4 * 500 + 250,
    currency: str = "eur",
) -> dict:
    obj: dict = {"id": session, "object": "checkout.session", "payment_status": payment_status}
    if prefix is not None:
        if use_metadata:
            obj["metadata"] = {"prefix": prefix}
        else:
            obj["client_reference_id"] = prefix
    # What section 3's session is priced at — two cards on the default YMCKOKO
    # ribbon is four printed sides, plus shipping. Stripe always reports both the
    # amount and the currency, and the order confirmation states the amount.
    obj["amount_total"] = amount_total
    obj["currency"] = currency
    if customer_email is not None:
        # What Checkout collects and the order confirmation is addressed to.
        obj["customer_details"] = {"email": customer_email, "name": "Ada Lovelace"}
    return {
        "id": "evt_" + uuid.uuid4().hex[:16],
        "object": "event",
        "type": kind,
        "livemode": False,
        "data": {"object": obj},
    }


def prefix_state(prefix: str) -> Any:
    _, body = json_request("GET", f"/api/prefixes/{prefix}")
    return (body or {}).get("status")


def prefix_order_ref(prefix: str) -> Any:
    _, body = json_request("GET", f"/api/prefixes/{prefix}")
    return (body or {}).get("order_ref")


def new_prefix(cards: int = 0, start_checkout: bool = False) -> str:
    status, created = json_request("POST", "/api/prefixes")
    assert status == 201, (status, created)
    prefix = created["uuid"]
    png = open(CARD_PNG, "rb").read()
    for index in range(cards):
        data, content_type = multipart(
            {
                "name": f"Card {index + 1}",
                "serial_number": f"S{index + 1}",
                "back": "markdown",
                "back_text": "01 Song",
            },
            "card.png",
            png,
        )
        code, _ = request(
            "POST", f"/api/prefixes/{prefix}/cards", data=data, headers={"content-type": content_type}
        )
        assert code == 201, code
    if start_checkout:
        # Opening a checkout is what moves a draft to payment_pending now; there
        # is no separate submit step to call first.
        code, _ = json_request("POST", f"/api/prefixes/{prefix}/checkout")
        assert code == 200, code
    return prefix


def recorded_forms() -> list[dict]:
    if not os.path.exists(MOCK_OUT):
        return []
    with open(MOCK_OUT, encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def recorded_emails() -> list[dict]:
    if not os.path.exists(MOCK_RESEND_OUT):
        return []
    with open(MOCK_RESEND_OUT, encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


def email_to(recipient: str) -> dict:
    """The single recorded message addressed to `recipient`, or {}."""
    for record in recorded_emails():
        message = record.get("message") or {}
        if (message.get("to") or [None])[0] == recipient:
            return message
    return {}


print(f"testing {BASE}   (mock Stripe recorder: {MOCK_OUT})")

# --- 1. shop config ---------------------------------------------------------
print("\n1. shop config")
status, config = json_request("GET", "/api/shop/config")
config = config or {}
check("GET /api/shop/config is 200", status == 200, str(status))
check("payment reports as enabled", config.get("enabled") is True, str(config))
check("currency is eur", config.get("currency") == "eur", str(config.get("currency")))
prices = {p.get("ribbon"): p.get("price_cents") for p in config.get("prices", [])}
check(
    "one price per ribbon, the price of one printed side",
    set(prices) == {"ymcko", "ymckoko"},
    str(prices),
)
check(
    "a printed side on the default ribbon is 500",
    prices.get("ymckoko") == 500,
    str(prices),
)
check("shipping is 250", config.get("shipping_cents") == 250, str(config.get("shipping_cents")))
check("shipping countries include DE", "DE" in config.get("countries", []), str(config.get("countries")))

# --- 2. guards before a session is created ----------------------------------
print("\n2. guards")
empty = new_prefix(cards=0)
status, _ = json_request("POST", f"/api/prefixes/{empty}/checkout")
check("a prefix with no cards cannot be paid for (409)", status == 409, str(status))
check(
    "and a refused checkout leaves it a draft",
    prefix_state(empty) == "draft",
    str(prefix_state(empty)),
)

gone = new_prefix(cards=1)
status, _ = json_request("POST", f"/api/prefixes/{gone}/submit")
check("the submit endpoint is gone (410)", status == 410, str(status))

status, _ = json_request("POST", "/api/prefixes/00000000-0000-0000-0000-000000000000/checkout")
check("an unknown prefix is a 404", status == 404, str(status))

# --- 3. the checkout session, and what we actually sent ---------------------
print("\n3. checkout session")
if os.path.exists(MOCK_OUT):
    os.truncate(MOCK_OUT, 0)

prefix = new_prefix(cards=2)
check(
    "a fresh prefix starts as a draft",
    prefix_state(prefix) == "draft",
    str(prefix_state(prefix)),
)
status, session = json_request("POST", f"/api/prefixes/{prefix}/checkout")
session = session or {}
check("checkout returns 200", status == 200, f"{status} {session}")
check("it returns a checkout URL", bool(session.get("url")), str(session))
check(
    "the amount is 4 printed sides plus shipping",
    session.get("amount_total") == 4 * 500 + 250,
    str(session.get("amount_total")),
)
check(
    "the checkout moved it to payment_pending, not paid",
    prefix_state(prefix) == "payment_pending",
    str(prefix_state(prefix)),
)

forms = recorded_forms()
check(
    "the mock recorded exactly one Stripe call",
    len(forms) == 1,
    f"recorded {len(forms)} in {MOCK_OUT}",
)

if not forms:
    # Without this, a path mismatch between the mock and this test would silently
    # skip every request-shape assertion below and look like a pass.
    check(
        "the request-shape checks could run (mock and test must share MOCK_STRIPE_OUT)",
        False,
        f"nothing recorded in {MOCK_OUT}",
    )
else:
    record = forms[0]
    fields: dict[str, str] = {}
    for pair in record["body"].split("&"):
        key, _, value = pair.partition("=")
        fields[key] = urllib.parse.unquote_plus(value)

    def field(name: str) -> Any:
        return fields.get(name)

    check("it posted to /v1/checkout/sessions", record["path"] == "/v1/checkout/sessions", record["path"])
    check(
        "it authenticated with the Stripe secret key",
        record["authorization"] == f"Bearer {STRIPE_SECRET_KEY}",
        str(record["authorization"]),
    )
    check(
        "it sent a form content type",
        (record["content_type"] or "").startswith("application/x-www-form-urlencoded"),
        str(record["content_type"]),
    )
    # Stripe REMOVED payment_method_types from Checkout Session creation and
    # answers 400 to a session that still sends it. Regression guard: the
    # parameter must never reappear in this body.
    check(
        "it does NOT send the removed payment_method_types parameter",
        not any(k.startswith("payment_method_types") for k in fields),
        ", ".join(k for k in fields if "payment_method" in k) or "(absent)",
    )
    check("mode is payment", field("mode") == "payment", str(field("mode")))
    check(
        "client_reference_id is the prefix uuid",
        field("client_reference_id") == prefix,
        str(field("client_reference_id")),
    )
    check(
        "metadata[prefix] is the prefix uuid too",
        field("metadata[prefix]") == prefix,
        str(field("metadata[prefix]")),
    )
    # Two cards on the default YMCKOKO ribbon: it always burns a full
    # double-sided card, so the order is billed for 4 printed sides however its
    # backs are filled in.
    check(
        "quantity is the number of printed sides",
        field("line_items[0][quantity]") == "4",
        str(field("line_items[0][quantity]")),
    )
    check(
        "unit_amount is the price of one printed side",
        field("line_items[0][price_data][unit_amount]") == "500",
        str(field("line_items[0][price_data][unit_amount]")),
    )
    check("currency is eur", field("line_items[0][price_data][currency]") == "eur")
    check(
        "success_url returns to the prefix with ?paid=1",
        field("success_url") == f"{PUBLIC_BASE}/p/{prefix}?paid=1",
        str(field("success_url")),
    )
    check(
        "cancel_url returns to the bare prefix page",
        field("cancel_url") == f"{PUBLIC_BASE}/p/{prefix}",
        str(field("cancel_url")),
    )
    check(
        "a shipping address is collected, for DE",
        field("shipping_address_collection[allowed_countries][0]") == "DE",
        str(field("shipping_address_collection[allowed_countries][0]")),
    )
    check(
        "a flat shipping rate is offered",
        field("shipping_options[0][shipping_rate_data][fixed_amount][amount]") == "250",
        str(field("shipping_options[0][shipping_rate_data][fixed_amount][amount]")),
    )
    # Payment methods are Dashboard-managed now: a session may only EXCLUDE
    # methods, never name the ones to offer. One variable configures the server
    # and this same variable sets the expectation here, so the two can't drift.
    expected_excluded = [
        m.strip().lower()
        for m in os.environ.get("STRIPE_EXCLUDED_PAYMENT_METHODS", "").split(",")
        if m.strip()
    ]
    sent_excluded = [
        fields[k]
        for k in sorted(
            (k for k in fields if k.startswith("excluded_payment_method_types[")),
            key=lambda k: int(k.split("[")[1].rstrip("]")),
        )
    ]
    check(
        "it excludes exactly the configured payment methods",
        sent_excluded == expected_excluded,
        f"sent {sent_excluded}, expected {expected_excluded}",
    )
    check(
        "it pins a payment method configuration only when one is configured",
        ("payment_method_configuration" in fields)
        == bool(os.environ.get("STRIPE_PAYMENT_METHOD_CONFIGURATION")),
        str(fields.get("payment_method_configuration")),
    )

# --- 4. webhook signatures --------------------------------------------------
print("\n4. webhook signatures")
status, _ = deliver(payment_event(prefix=prefix), signature="t=1,v1=deadbeef")
check("a wrong signature is rejected (400)", status == 400, str(status))
check("and the prefix was not touched", prefix_state(prefix) == "payment_pending", str(prefix_state(prefix)))

raw = json.dumps(payment_event(prefix=prefix)).encode()
status, _ = request(
    "POST",
    "/api/webhooks/stripe",
    data=raw,
    headers={"content-type": "application/json", "stripe-signature": sign(b"a different body")},
)
check("a signature over a different body is rejected (400)", status == 400, str(status))

status, _ = deliver(
    payment_event(prefix=prefix), signature=sign(raw, timestamp=int(time.time()) - 3600)
)
check("a stale timestamp is rejected (400)", status == 400, str(status))

status, _ = request(
    "POST", "/api/webhooks/stripe", data=raw, headers={"content-type": "application/json"}
)
check("a missing signature header is a 400", status == 400, str(status))

status, _ = deliver(payment_event(prefix=prefix), secret="whsec_the_wrong_secret")
check("a signature from the wrong secret is rejected (400)", status == 400, str(status))

check("still not paid after all that", prefix_state(prefix) == "payment_pending", str(prefix_state(prefix)))

# --- 5. the payment lands ---------------------------------------------------
print("\n5. recording the payment")
# Start the email recorder from empty, so the count below is this order's mail
# and not something an earlier section left behind.
if EMAILS_ON and os.path.exists(MOCK_RESEND_OUT):
    os.truncate(MOCK_RESEND_OUT, 0)

status, _ = deliver(payment_event(prefix=prefix))
check("a correctly signed completed event is accepted (200)", status == 200, str(status))
check("the prefix is now paid", prefix_state(prefix) == "paid", str(prefix_state(prefix)))
check(
    "the Stripe session id is the order reference",
    prefix_order_ref(prefix) == "cs_test_mock_1",
    str(prefix_order_ref(prefix)),
)

# --- 5b. the order emails ---------------------------------------------------
# Runs only when the server was started with RESEND_API_KEY and EMAIL_FROM, and
# pointed at the mock via RESEND_API_BASE.
print("\n5b. order emails")
if not EMAILS_ON:
    print("  note no RESEND_API_KEY/EMAIL_FROM set, so the email checks are skipped")
    print(
        "       (start the server with RESEND_API_KEY=re_test_x "
        "EMAIL_FROM='ESPuino <orders@example.test>' "
        "RESEND_API_BASE=http://127.0.0.1:12112 to cover them)"
    )
else:
    emails = recorded_emails()
    expected = 2 if OPERATOR_EMAIL else 1
    check(
        f"a paid order sends {expected} email(s), one per recipient",
        len(emails) == expected,
        f"recorded {len(emails)} in {MOCK_RESEND_OUT}",
    )
    if not emails:
        # Without this, a mismatch between the mock and this test would make
        # every check below trivially pass on empty dicts.
        check(
            "the email checks could run (server and test must share MOCK_RESEND_OUT)",
            False,
            f"nothing recorded in {MOCK_RESEND_OUT}",
        )
    else:
        customer = email_to(CUSTOMER_EMAIL)
        check(
            "the customer's confirmation is addressed to the address on the Stripe session",
            bool(customer),
            f"recipients: {[r['message']['to'] for r in emails if r.get('message')]}",
        )
        check(
            "it is sent from the configured from-address",
            customer.get("from") == os.environ.get("EMAIL_FROM"),
            str(customer.get("from")),
        )
        check(
            "it carries a link back to the customer's project",
            f"{PUBLIC_BASE}/p/{prefix}" in (customer.get("text") or ""),
            str(customer.get("subject")),
        )
        check(
            "it has both a text and an html part",
            bool(customer.get("text")) and bool(customer.get("html")),
            f"text={bool(customer.get('text'))} html={bool(customer.get('html'))}",
        )
        check(
            "it states the amount that was paid",
            "22.50 EUR" in (customer.get("text") or ""),
            str(customer.get("text"))[:200],
        )
        if OPERATOR_EMAIL:
            operator = email_to(OPERATOR_EMAIL)
            check(
                "the operator gets a copy, linking to the admin view",
                f"{PUBLIC_BASE}/admin/p/{prefix}" in (operator.get("text") or ""),
                str(operator.get("subject")),
            )

status, _ = deliver(payment_event(prefix=prefix))
check("a replayed event is accepted (200)", status == 200, str(status))
check("the replay changed nothing", prefix_state(prefix) == "paid", str(prefix_state(prefix)))
check("the order reference survived the replay", prefix_order_ref(prefix) == "cs_test_mock_1")
if EMAILS_ON:
    # The trap this whole feature is designed around: Stripe retries until it
    # gets a 2xx, so a second delivery must not mail the customer again.
    expected = 2 if OPERATOR_EMAIL else 1
    check(
        f"the replay sent no second email (still {expected})",
        len(recorded_emails()) == expected,
        f"recorded {len(recorded_emails())} in {MOCK_RESEND_OUT}",
    )

# --- 6. events that must be handled or ignored safely -----------------------
print("\n6. events we ignore")
status, _ = deliver(payment_event(prefix=prefix, payment_status="unpaid"))
check("an unsettled completed event is accepted (200)", status == 200, str(status))

status, _ = deliver(payment_event(prefix=prefix, kind="payment_intent.created"))
check("an unrelated event type is accepted (200)", status == 200, str(status))

status, _ = deliver(payment_event(prefix=None))
check("an event with no prefix reference is accepted (200)", status == 200, str(status))

status, _ = deliver(payment_event(prefix="not-a-uuid"))
check("an event with a junk reference is accepted (200)", status == 200, str(status))

status, _ = deliver(payment_event(prefix=str(uuid.uuid4())))
check("an event for a vanished prefix is accepted (200)", status == 200, str(status))

unsettled = new_prefix(cards=1, start_checkout=True)
deliver(payment_event(prefix=unsettled, session="cs_unsettled", payment_status="unpaid"))
check(
    "a session that completed unpaid is left payment_pending",
    prefix_state(unsettled) == "payment_pending",
    str(prefix_state(unsettled)),
)
deliver(
    payment_event(
        prefix=unsettled, kind="checkout.session.async_payment_succeeded", session="cs_unsettled"
    )
)
check("async_payment_succeeded then settles it", prefix_state(unsettled) == "paid", str(prefix_state(unsettled)))

via_meta = new_prefix(cards=1, start_checkout=True)
deliver(payment_event(prefix=via_meta, session="cs_meta", use_metadata=True))
check(
    "a session identified only by metadata is attributed",
    prefix_state(via_meta) == "paid",
    str(prefix_state(via_meta)),
)

# --- 6b. an abandoned checkout expires and reverts to draft -----------------
print("\n6b. checkout.session.expired")
abandoned = new_prefix(cards=1, start_checkout=True)
check(
    "the prefix opened for payment is payment_pending",
    prefix_state(abandoned) == "payment_pending",
    str(prefix_state(abandoned)),
)
deliver(payment_event(prefix=abandoned, kind="checkout.session.expired", session="cs_abandoned"))
check(
    "an expired (abandoned or declined) checkout reverts the prefix to draft",
    prefix_state(abandoned) == "draft",
    str(prefix_state(abandoned)),
)
deliver(payment_event(prefix=abandoned, kind="checkout.session.expired", session="cs_abandoned"))
check(
    "a replayed expired event leaves the draft alone",
    prefix_state(abandoned) == "draft",
    str(prefix_state(abandoned)),
)
# The unsealed prefix is writable again: the customer can edit cards once more.
data, content_type = multipart(
    {"name": "Edited", "serial_number": "S1", "back": "markdown", "back_text": "01 Song"},
    "card.png",
    open(CARD_PNG, "rb").read(),
)
code, _ = request(
    "PUT", f"/api/prefixes/{abandoned}/cards/1", data=data, headers={"content-type": content_type}
)
check(
    "and the customer's write window is open again",
    code == 200,
    str(code),
)

# Cancelling needs the operator token, so this one check is opt-in.
admin_token = os.environ.get("ADMIN_TOKEN")
if not admin_token:
    print("  note no ADMIN_TOKEN set, so the cancelled-prefix branch is not covered here")
else:
    cancelled = new_prefix(cards=1, start_checkout=True)
    status, _ = json_request(
        "PATCH",
        f"/api/prefixes/{cancelled}/status",
        {"status": "cancelled"},
        headers={"authorization": f"Bearer {admin_token}"},
    )
    check("the operator can cancel a prefix", status == 200, str(status))
    deliver(payment_event(prefix=cancelled, session="cs_cancelled"))
    check(
        "a paid-but-cancelled prefix is left cancelled, for a human to resolve",
        prefix_state(cancelled) == "cancelled",
        str(prefix_state(cancelled)),
    )

# --- summary ----------------------------------------------------------------
print(f"\n{checks - len(failed)}/{checks} checks passed")
if failed:
    print("failed:")
    for name in failed:
        print(f"  - {name}")
sys.exit(len(failed))
