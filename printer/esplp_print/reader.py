"""HTTP client for the nfc-reader — the RFID UID of the card at the antenna.

The reader is NOT the Evolis SDK's PC/SC path: it is a separate device in the
printer's encoder slot (`~/git/embassy-test/nfc-reader`, an ESP32 + PN532 that
polls the coil and serves what it sees over plain HTTP on the LAN, no auth). So
a UID read is two halves of two different things — the printer moves the card to
the antenna (`Printer.read_card_uid`), this module asks the reader what is there.

The endpoint used is `GET /nfc/card`, which answers

    {"present":true,"uid":"04-A2-B3-C4","uid_decimal":"004162179196", ...}

or `404 {"present":false}` when nothing is on the coil. `?wait_ms=N` holds the
request open until a card appears (the firmware caps it at 30 s), which covers
the gap between the printer reporting the card at the station and the reader's
next poll — the field is only on while the reader polls.

`uid_decimal` is the value the shop stores as a card's `serial_number`: each UID
byte as `%03d`, so decimal, never hex, leading zeros kept (`004162179196` is the
4-byte UID `04-A2-B3-C4`).

Standard library only (urllib), like `api.py`: the spooler's other dependency is
the vendor SDK and it should not grow an HTTP framework.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request

CARD_PATH = "/nfc/card"

# How long the read may wait for a card, and the firmware's cap on it. Only the
# reader's poll cycle is being waited out (the printer has already reported the
# card at the station when this is called), so a few seconds is a safety net
# rather than a travel time.
DEFAULT_WAIT_MS = 3000
MAX_WAIT_MS = 30_000

# The device's mDNS name, which its firmware publishes (A record + DNS-SD).
DEFAULT_READER_URL = "http://nfc-reader.local"


class ReaderError(Exception):
    """The reader could not be asked, or answered something unusable."""


class NfcReader:
    def __init__(
        self,
        base: str,
        wait_ms: int = DEFAULT_WAIT_MS,
        timeout_s: float | None = None,
    ):
        self.base = base.rstrip("/")
        self.wait_ms = max(0, min(wait_ms, MAX_WAIT_MS))
        # A read may legitimately block for the whole wait window, so the HTTP
        # timeout has to sit above it — otherwise a slow card reads as a dead
        # reader.
        self.timeout_s = (
            timeout_s if timeout_s is not None else self.wait_ms / 1000 + 5.0
        )

    def read_uid(self) -> str | None:
        """The UID of the card under the reader, or None when there is none.

        None is the reader's own "nothing on the coil" (HTTP 404 / `present`
        false) — a definitive answer, unlike ReaderError, which means the UID is
        unknown because the reader was unreachable or answered junk. Both leave
        the card without a serial number; neither is fatal to a print run.
        """
        query = urllib.parse.urlencode({"wait_ms": self.wait_ms})
        url = f"{self.base}{CARD_PATH}?{query}"
        try:
            with urllib.request.urlopen(url, timeout=self.timeout_s) as resp:  # noqa: S310
                raw = resp.read()
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None  # present:false — the card is not on the coil
            raise ReaderError(f"{url} answered HTTP {e.code}") from e
        except OSError as e:
            # URLError, a timeout or a dropped connection: all "could not ask".
            raise ReaderError(f"{url} unreachable: {e}") from e

        try:
            card = json.loads(raw)
        except ValueError as e:
            raise ReaderError(f"{url} answered unparseable JSON: {raw[:120]!r}") from e
        if not isinstance(card, dict):
            raise ReaderError(f"{url} answered {type(card).__name__}, not an object")
        if not card.get("present"):
            return None
        uid = card.get("uid_decimal")
        if not isinstance(uid, str) or not uid.strip():
            raise ReaderError(f"{url} reports a card with no uid_decimal: {card!r}")
        return uid.strip()


__all__ = [
    "CARD_PATH",
    "DEFAULT_READER_URL",
    "DEFAULT_WAIT_MS",
    "MAX_WAIT_MS",
    "NfcReader",
    "ReaderError",
]
