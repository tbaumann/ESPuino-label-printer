"""Thin wrapper over the Evolis SDK for the ESPuino label spooler.

Hides the connection-string details and the per-card raster feeding from the
spooler loop. The SDK import is deferred so the spooler can run a dry run, or
report the plan, on a machine without the wheel installed.

Supported connection strings (passed straight through as `--ip`, which the user
gives as `socket://192.168.2.101?type=evolis`): see the SDK's own
`Connection.open`. Everything here takes a single `ip` string.

Ribbons: the server speaks two families — `ymcko` (full-colour front, and a
colour back when the card is double-sided) and `ymckoko` (full-colour front,
black-only back). The SDK's `RibbonType` enum has no single member named for
each; it lists YMCKO variants and YMCKOK / YMCKOKOS variants. The ribbon check
here is a **sanity check only** (skippable), so it compares the *family* by name
prefix rather than pinning a specific enum member that a given printer's
firmware might report differently.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import evolis  # deferred import; only for type checking

RIBBON_YMCKO = "ymcko"
RIBBON_YMCKOKO = "ymckoko"

BACK_NONE = "none"
BACK_COLOR = "color"
BACK_BLACK = "black"


class PrinterError(Exception):
    pass


@dataclass
class RibbonStatus:
    """What the printer reports about its loaded ribbon."""

    ribbon: str  # "ymcko" | "ymckoko" | "unknown"
    product_code: str
    remaining: int | None
    capacity: int | None


def _family_of_sdk_name(name: str) -> str:
    """Normalise an SDK RibbonType member name to one of our two families.

    YMCKO / YMCKOS / YMCKOS13 etc → ymcko; YMCKOK / YMCKOKOS / YMCKOKS13 etc →
    ymckoko. Anything else (KO, KBLACK, HOLO, ...) → "unknown" so the spooler
    can say "I don't recognise this ribbon" rather than assume.
    """
    n = name.lower()
    if n == "ymcko" or n.startswith("ymckos"):
        return RIBBON_YMCKO
    if n.startswith("ymckok"):
        return RIBBON_YMCKOKO
    return "unknown"


class Printer:
    def __init__(self, ip: str):
        self._ip = ip
        self._evolis: Any = None  # imported lazily in open()
        self._co: Any = None

    # -- connection ---------------------------------------------------------

    def open(self) -> None:
        # Deferred so constructing a Printer never needs the wheel installed,
        # and a dry run / plan-only mode works without it.
        import evolis  # deferred

        self._evolis = evolis
        co = evolis.Connection()
        if not co.open(self._ip):
            raise PrinterError(
                f"cannot open printer connection to {self._ip!r}: {co.get_last_error()}"
            )
        co.enable_status()  # required before get_status() returns data
        self._co = co

    def close(self) -> None:
        if self._co is not None:
            try:
                self._co.close()
            except Exception:
                pass
            self._co = None

    def __enter__(self) -> "Printer":
        self.open()
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    # -- state / ribbon -----------------------------------------------------

    def require_ready(self) -> None:
        if self._co is None:
            raise PrinterError("printer not open")
        evolis = self._evolis
        state = self._co.get_state()
        if state is None:
            raise PrinterError(f"no state from printer: {self._co.get_last_error()}")
        if state.major != evolis.State.Major.READY:
            raise PrinterError(
                f"printer not READY (major={state.major}, minor={state.minor}); "
                "clear the fault and retry"
            )

    def ribbon_status(self) -> RibbonStatus:
        if self._co is None:
            raise PrinterError("printer not open")
        info = self._co.get_ribbon_info()
        if info is None:
            raise PrinterError(f"cannot read ribbon info: {self._co.get_last_error()}")
        # `str(info.type)` renders like "RibbonType.YMCKOK"; keep the member name
        # (or the bare enum value string) and drop any `RibbonType.`/`: N` noise.
        raw = str(info.type)
        name = raw.rsplit(".", 1)[-1].rsplit(":", 1)[0].strip()
        return RibbonStatus(
            ribbon=_family_of_sdk_name(name),
            product_code=getattr(info, "productCode", "") or "",
            remaining=getattr(info, "remaining", None),
            capacity=getattr(info, "capacity", None),
        )

    # -- printing -----------------------------------------------------------

    def print_card(self, front_bmp: bytes, back: str, back_bmp: bytes | None) -> None:
        """Feed and print one card.

        `back` is the queue's per-card back availability (`none|color|black`);
        `back_bmp` is the raster for that back, or None for a single-sided card.
        The right SDK call depends on the back *material*: a colour back is a
        24-bit image (`set_image_buffer`), a black back is 1-bit
        (`set_black_buffer`). Both must be BMP bytes, the server's raster verbatim.

        `init()` is called first and is load-bearing: a `PrintSession` does not
        create a context by itself, and the underlying printer context survives
        across sessions. Without an explicit reset, card 1's back image and
        duplex flag leak into every later card — a single-sided card 2 after a
        double-sided card 1 prints double-sided with card 1's stale back. init()
        clears the graphics and resets settings to default (including duplex).
        """
        if self._co is None:
            raise PrinterError("printer not open")
        import ctypes  # for the SDK's c_char_p buffer args

        evolis = self._evolis

        ps = evolis.PrintSession(self._co)
        if not ps.init():
            raise PrinterError(f"cannot initialise print context: {ps.get_last_error()}")
        if not ps.set_image_buffer(
            evolis.CardFace.FRONT, ctypes.create_string_buffer(front_bmp)
        ):
            raise PrinterError(f"cannot set front image buffer: {ps.get_last_error()}")
        if back in (BACK_COLOR, BACK_BLACK) and back_bmp is not None:
            buf = ctypes.create_string_buffer(back_bmp)
            if back == BACK_COLOR:
                ok = ps.set_image_buffer(evolis.CardFace.BACK, buf)
            else:
                ok = ps.set_black_buffer(evolis.CardFace.BACK, buf)
            if not ok:
                raise PrinterError(f"cannot set back image buffer: {ps.get_last_error()}")
        rc = ps.print()
        if rc != evolis.ReturnCode.OK:
            raise PrinterError(f"print failed: {rc}")


__all__ = [
    "Printer",
    "PrinterError",
    "RIBBON_YMCKO",
    "RIBBON_YMCKOKO",
    "BACK_NONE",
    "BACK_COLOR",
    "BACK_BLACK",
    "RibbonStatus",
]
