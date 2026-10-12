#!/usr/bin/env python3
"""A stand-in for api.stripe.com, for testing the checkout request.

Point the server at it with `STRIPE_API_BASE=http://127.0.0.1:<port>` and it will
answer `POST /v1/checkout/sessions` with a plausible session, recording the raw
form body it received so a test can assert on exactly what we sent.

Deliberately dumb: no signature checking, no state. It does reproduce one piece
of real API behaviour — rejecting the removed `payment_method_types` parameter —
because sending that shape is easy to reintroduce and the live API fails it hard.

    python3 scripts/mock_stripe.py 12111
"""

import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs

OUT = os.environ.get("MOCK_STRIPE_OUT", "/tmp/mock_stripe_forms.jsonl")

# Requests are threaded, so recorder writes must be serialized: two appends
# racing would interleave into one unparseable line.
_WRITE_LOCK = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):  # noqa: N802 - the name is fixed by BaseHTTPRequestHandler
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length).decode("utf-8", "replace")

        with _WRITE_LOCK, open(OUT, "a", encoding="utf-8") as fh:
            fh.write(
                json.dumps(
                    {
                        "path": self.path,
                        "authorization": self.headers.get("authorization"),
                        "content_type": self.headers.get("content-type"),
                        "body": raw,
                    }
                )
                + "\n"
            )

        if self.path != "/v1/checkout/sessions":
            self._respond(404, {"error": {"message": "no such route"}})
            return

        q = parse_qs(raw, keep_blank_values=True)

        # Stripe removed `payment_method_types` from Checkout Session creation and
        # answers 400 with this message. Reproduced here so the mock fails the way
        # the live API does, rather than quietly accepting a request that would be
        # rejected for real.
        if any(k.startswith("payment_method_types") for k in q):
            self._respond(
                400,
                {
                    "error": {
                        "message": (
                            "The `payment_method_types` parameter is no longer supported when "
                            "creating a Checkout Session. Payment methods are now managed from "
                            "your Dashboard settings."
                        ),
                        "param": "payment_method_types",
                        "type": "invalid_request_error",
                    }
                },
            )
            return

        def one(key, default=None):
            return q.get(key, [default])[0]

        try:
            quantity = int(one("line_items[0][quantity]", "0"))
            unit = int(one("line_items[0][price_data][unit_amount]", "0"))
        except ValueError:
            self._respond(400, {"error": {"message": "bad quantity/unit_amount"}})
            return

        shipping = 0
        ship_key = "shipping_options[0][shipping_rate_data][fixed_amount][amount]"
        if ship_key in q:
            shipping = int(one(ship_key, "0"))

        self._respond(
            200,
            {
                "id": "cs_test_mock_1",
                "object": "checkout.session",
                "url": "https://checkout.stripe.com/c/pay/cs_test_mock_1",
                "status": "open",
                "payment_status": "unpaid",
                "amount_total": quantity * unit + shipping,
                "currency": one("line_items[0][price_data][currency]", "eur"),
                "client_reference_id": one("client_reference_id"),
                "mode": one("mode"),
            },
        )

    def _respond(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    # The name must match the stdlib signature exactly, builtin shadowing and
    # all, or type checkers flag the override.
    def log_message(self, format, *args):  # noqa: A002
        pass  # quiet; the recorder file is the log


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 12111
    print(f"mock stripe on http://127.0.0.1:{port} (recording to {OUT})", flush=True)
    # Threaded on purpose. A single-threaded server with HTTP/1.1 keep-alive
    # serves ONE connection until it closes, so the second client to connect
    # (a second app instance under test, or the same app after a restart) just
    # hangs until that connection times out — which reads as a checkout failure
    # and sends you hunting in the wrong place.
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
