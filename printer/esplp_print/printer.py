"""Thin wrapper over the Evolis SDK for the ESPuino label spooler.

Hides the connection-string details and the per-card raster feeding from the
spooler loop. The SDK import is deferred so the spooler can run a dry run, or
report the plan, on a machine without the wheel installed.

Supported connection strings (passed straight through as `--ip`, which the user
gives as `socket://192.168.2.101?type=evolis`): see the SDK's own
`Connection.open`. Everything here takes a single `ip` string.

Ribbons: the server speaks two families — `ymcko` (full-colour front and back)
and `ymckok` (full-colour front, second black panel for the back). The SDK's
`RibbonType` enum has no single member named for each; it lists YMCKO variants
and YMCKOK / YMCKOKOS variants. The ribbon check here is a **sanity check only**
(skippable), so it compares the *family* by name prefix rather than pinning a
specific enum member that a given printer's firmware might report differently.

The printer does not care what a back is *made of*, only what depth its raster
is: both ribbons print the same set of card backs, and the server's raster is
what differs — a 24-bit colour BMP on YMCKO, a 1-bit one on YMCKOK. Whichever
it sends is fed verbatim (see `print_card`).

A card's RFID UID is read before it is printed: the printer feeds the card and
parks it at the contactless station (`read_card_uid`), where the reader in the
printer's encoder slot can see it, and that reader is then asked for the UID
over HTTP (a separate device — see `reader.py`). A `Printer` is handed its
reader rather than having that step optional, so the read runs for every card;
what can degrade is the answer, never whether the read was attempted.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    import evolis  # deferred import; only for type checking
    from .reader import NfcReader

RIBBON_YMCKO = "ymcko"
RIBBON_YMCKOK = "ymckok"

BACK_NONE = "none"
BACK_COLOR = "color"
BACK_BLACK = "black"

# The RFID UID as the reader reports it (`uid_decimal`): the UID's four bytes,
# each formatted `%03d`, so exactly 12 decimal digits and often with a leading
# zero. The server stores it in the card's `serial_number`; an external client
# (MediaHub) promotes its own card per UID, so a card without one is a card that
# never gets promoted.
UID_DIGITS = 12


class PrinterError(Exception):
    pass


@dataclass
class RibbonStatus:
    """What the printer reports about its loaded ribbon."""

    ribbon: str  # "ymcko" | "ymckok" | "unknown"
    product_code: str
    remaining: int | None
    capacity: int | None


def _family_of_sdk_name(name: str) -> str:
    """Normalise an SDK RibbonType member name to one of our two families.

    YMCKO / YMCKOS / YMCKOS13 etc → ymcko; YMCKOK / YMCKOKOS / YMCKOKS13 etc →
    ymckok. Anything else (KO, KBLACK, HOLO, ...) → "unknown" so the spooler
    can say "I don't recognise this ribbon" rather than assume.
    """
    n = name.lower()
    if n == "ymcko" or n.startswith("ymckos"):
        return RIBBON_YMCKO
    if n.startswith("ymckok"):
        return RIBBON_YMCKOK
    return "unknown"


class Printer:
    def __init__(self, ip: str, reader: NfcReader):
        """`ip` is the SDK connection string; `reader` the encoder-slot reader.

        The reader is a required half of this object, not an option: a card's
        UID is part of what a printed card *is*, and a `Printer` that cannot ask
        for it has no honest use. A reader that is unplugged or unreachable
        fails at the read (naming itself) — it does not remove the step.
        """
        self._ip = ip
        self._reader = reader
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

    # -- the card's RFID UID ------------------------------------------------

    def read_card_uid(self) -> str | None:
        """Park the card under the contactless reader and read its RFID UID.

        The sequence is the point of this method: the card is pulled from the
        feeder and moved to the contactless station
        (`evolis.CardPos.CONTACTLESS`), where the reader in the encoder slot can
        see it, and it stays there — the print job then takes it from that
        position to the print station itself. Feeding the card HERE rather than
        letting the print session do it is what makes the UID the id of *this*
        card.

        The read itself is the reader's HTTP API (`reader.py`); the UID comes
        back as the reader spells it, `uid_decimal` — already the 12 decimal
        digits the server stores as `serial_number` (see `UID_DIGITS`). It is
        passed through verbatim: reformatting it here would be a chance to lose
        the leading zero, and the reader's spelling is the shop's.

        Returns None when there is nothing to report: the reader polling its coil
        and finding no card. That means "not known yet": the card prints without
        one, the server's `serial_number` stays NULL, and a client that promotes
        cards by UID will report this one as printed with an unusable id rather
        than silently assigning it.

        Raises PrinterError when the printer cannot be driven to the reader at
        all, and ReaderError when the reader cannot be asked. A caller may treat
        either as "this card has no UID" — the spooler does exactly that: the
        card prints either way, and the run names the cards left without one.
        """
        if self._co is None:
            raise PrinterError("printer not open")
        evolis = self._evolis
        if not self._co.insert_card():
            raise PrinterError(
                f"cannot feed a card for the UID read: {self._co.get_last_error()}"
            )
        if not self._co.set_card_pos(evolis.CardPos.CONTACTLESS):
            raise PrinterError(
                "cannot park the card under the contactless station "
                f"({self._co.get_last_error()}); is the reader installed?"
            )
        return self._reader.read_uid()

    # -- printing -----------------------------------------------------------

    def print_card(
        self, front_bmp: bytes, back: str, back_bmp: bytes | None, send: bool = True
    ) -> None:
        """Feed and print one card — or stage it and put it back out.

        `back` is the queue's per-card back availability (`none|color|black`);
        `back_bmp` is the raster for that back, or None for a single-sided card.
        The right SDK call depends on the raster's depth, which is the ribbon's
        (the server says which in `back`): a colour back is a 24-bit image
        (`set_image_buffer`), a black back is 1-bit (`set_black_buffer`). Both
        must be BMP bytes, the server's raster verbatim.

        `init()` is called first and is load-bearing: a `PrintSession` does not
        create a context by itself, and the underlying printer context survives
        across sessions. Without an explicit reset, card 1's back image and
        duplex flag leak into every later card — a single-sided card 2 after a
        double-sided card 1 prints double-sided with card 1's stale back. init()
        clears the graphics and resets settings to default (including duplex).

        `send=False` stages the job — the same `init()` and both buffers, so the
        raster path the printer would be handed is exercised for real — and then
        ejects the card without submitting it. Nothing reaches the print engine,
        so no ribbon panel is spent and the card comes out blank and reusable
        (`eject_card()` is just `set_card_pos(EJECT)`; the card still has to be
        moved out because no print job will take it from the contactless
        station).
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
        if not send:
            if not self._co.eject_card():
                raise PrinterError(
                    f"cannot eject the staged card: {self._co.get_last_error()}"
                )
            return
        rc = ps.print()
        if rc != evolis.ReturnCode.OK:
            raise PrinterError(f"print failed: {rc}")


__all__ = [
    "Printer",
    "PrinterError",
    "RIBBON_YMCKO",
    "RIBBON_YMCKOK",
    "BACK_NONE",
    "BACK_COLOR",
    "BACK_BLACK",
    "UID_DIGITS",
    "RibbonStatus",
]
