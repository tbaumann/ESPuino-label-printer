"""HTTP client for the ESPuino Label Printer API, spooler-specific.

Uses only the standard library (urllib), because the spooler already depends on
the vendor SDK and should not drag in an HTTP framework. `requests` would be
another native dep for no gain on this thin surface.

Admin endpoints (print-queue, mark-printed) need the operator bearer token;
the raster endpoints are readable by anyone holding the prefix uuid, but we
send the token anyway (the server ignores it there — same convention as every
other customer-level call).
"""

from __future__ import annotations

import json
import ssl
import urllib.error
import urllib.request
from typing import Any


class ApiError(Exception):
    """A non-2xx response, or a response we cannot parse. Carries the status
    code when it was an HTTP error."""

    def __init__(self, message: str, status: int | None = None):
        super().__init__(message)
        self.status = status


class Client:
    def __init__(self, base: str, token: str | None = None):
        self.base = base.rstrip("/")
        self.token = token

    def _headers(self) -> dict[str, str]:
        h = {"Accept": "application/json"}
        if self.token:
            h["Authorization"] = f"Bearer {self.token}"
        return h

    def _request(
        self,
        method: str,
        path: str,
        *,
        body: Any = None,
        accept: str = "application/json",
    ) -> tuple[int, bytes]:
        url = f"{self.base}{path}"
        headers = self._headers()
        headers["Accept"] = accept
        data = None
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        # The default SSL context does full verification; a self-hosted server
        # behind a self-signed cert would need a configured context, but we do
        # not add an insecure escape hatch here.
        try:
            with urllib.request.urlopen(req) as resp:  # noqa: S310  (http allowed, not held-open)
                return resp.status, resp.read()
        except urllib.error.HTTPError as e:
            return e.code, e.read()
        except urllib.error.URLError as e:
            raise ApiError(f"request to {url} failed: {e.reason}") from e

    def _json(self, method: str, path: str, body: Any = None) -> Any:
        status, raw = self._request(method, path, body=body)
        if not (200 <= status < 300):
            msg = _error_message(raw) or f"HTTP {status}"
            raise ApiError(msg, status)
        if not raw:
            return None
        return json.loads(raw)

    def get_print_queue(self, prefix: str) -> dict[str, Any]:
        return self._json("GET", f"/api/prefixes/{prefix}/print-queue")

    def fetch_bytes(self, path: str) -> tuple[int, bytes]:
        """GET raw bytes (a raster). Returns (status, body) without raising, so
        the spooler can react to 404/423/500 with their distinct meaning."""
        return self._request("GET", path, accept="image/bmp")

    def mark_printed(self, prefix: str, card_id: int) -> None:
        self._json("POST", f"/api/prefixes/{prefix}/cards/{card_id}/printed")

    def set_prefix_status(self, prefix: str, status: str) -> dict[str, Any]:
        """Advance a prefix's status (admin). The transition is validated
        server-side; a prefix already in `printed` answers successfully anyway
        (re-applying the current status is a no-op there), so this is safe to
        call when the last card is marked."""
        return self._json(
            "PATCH",
            f"/api/prefixes/{prefix}/status",
            body={"status": status},
        )


def _error_message(raw: bytes) -> str | None:
    try:
        obj = json.loads(raw)
        msg = obj.get("error")
        return str(msg) if msg else None
    except Exception:
        return None


__all__ = ["ApiError", "Client"]
