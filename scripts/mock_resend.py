#!/usr/bin/env python3
"""A stand-in for api.resend.com, for testing the order emails.

Point the server at it with `RESEND_API_BASE=http://127.0.0.1:<port>` and it will
answer `POST /emails` with a plausible message id, recording the raw JSON body it
received so a test can assert on exactly what was sent — and how many times.

Deliberately dumb: no API-key checking, no state. Set `MOCK_RESEND_FAIL=1` to have
it reject every message the way Resend rejects an unverified sending domain, so
the "an email failure must never fail the webhook" path can be exercised against
a live server too.

    python3 scripts/mock_resend.py 12112
"""

import json
import os
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

OUT = os.environ.get("MOCK_RESEND_OUT", "/tmp/mock_resend_emails.jsonl")
FAIL = os.environ.get("MOCK_RESEND_FAIL", "") not in ("", "0", "false")

# Requests are threaded, so recorder writes must be serialized: two appends
# racing would interleave into one unparseable line.
_WRITE_LOCK = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):  # noqa: N802 - the name is fixed by BaseHTTPRequestHandler
        length = int(self.headers.get("content-length") or 0)
        raw = self.rfile.read(length).decode("utf-8", "replace")

        try:
            message = json.loads(raw)
        except ValueError:
            message = None

        with _WRITE_LOCK, open(OUT, "a", encoding="utf-8") as fh:
            fh.write(
                json.dumps(
                    {
                        "path": self.path,
                        "authorization": self.headers.get("authorization"),
                        "content_type": self.headers.get("content-type"),
                        "message": message,
                        "body": raw,
                    }
                )
                + "\n"
            )

        if self.path != "/emails":
            self._respond(404, {"statusCode": 404, "name": "not_found", "message": "no such route"})
            return

        if FAIL:
            # The shape Resend uses for a rejected message (unverified domain,
            # bad from address, invalid key): a 422 with the reason in the body.
            self._respond(
                422,
                {
                    "statusCode": 422,
                    "name": "validation_error",
                    "message": "The domain is not verified. Verify a domain at resend.com/domains.",
                },
            )
            return

        self._respond(200, {"id": "re_mock_1"})

    def _respond(self, status: int, payload: dict) -> None:
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
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 12112
    print(
        f"mock resend on http://127.0.0.1:{port} (recording to {OUT}"
        f"{', rejecting every message' if FAIL else ''})",
        flush=True,
    )
    # Threaded for the same reason as mock_stripe: a single-threaded HTTP/1.1
    # server serves one keep-alive connection at a time, so a second client just
    # hangs until it times out.
    ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
