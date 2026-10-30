"""Smoke test of the spooler's API layer against a mock server.

Exercises Client.get_print_queue / fetch_bytes / mark_printed / list_prefixes /
set_prefix_status and the spooler's queue->todo selection and batch filtering,
WITHOUT a printer (stops at the printer check). The card's UID step is exercised
against a second mock — the nfc-reader's `/nfc/card` — with the printer's side
of it faked. Run from the Nix-built env so `esplp_print` imports from the store.
"""
import os
import sys
import threading
import json
import http.server
import socketserver

from esplp_print.api import Client
from esplp_print.printer import Printer, PrinterError
from esplp_print.reader import (
    DEFAULT_READER_URL,
    DEFAULT_WAIT_MS,
    NfcReader,
    ReaderError,
)

LOG = {"GET": [], "POST": [], "PATCH": []}
# The parsed JSON body of each POST (None when the call sent none).
POST_BODIES: list = []
# Prefixes for the batch mode: two ymcko jobs (one paid, one queued), one
# ymckok job (should be excluded when filtering ymcko), one draft (excluded).
PREFIXES = [
    {"uuid": "A", "status": "paid", "ribbon": "ymcko", "unprinted_count": 2},
    {"uuid": "B", "status": "queued", "ribbon": "ymcko", "unprinted_count": 1},
    {"uuid": "C", "status": "paid", "ribbon": "ymckok", "unprinted_count": 3},
    {"uuid": "D", "status": "draft", "ribbon": "ymcko", "unprinted_count": 0},
]
QUEUE = {
    "prefix": "P",
    "status": "queued",
    "ribbon": "ymckok",
    "unprinted_count": 2,
    "billed_units": 2,
    "resume": False,
    "cards": [
        {"id": 1, "name": "Conni", "printed_at": None, "back": "black",
         "billed_units": 1, "printable": True},
        {"id": 2, "name": "Bob", "printed_at": None, "back": "none",
         "billed_units": 1, "printable": True},
        {"id": 3, "name": "Ana", "printed_at": "2026-10-15T..", "back": "none",
         "billed_units": 1, "printable": True},
        {"id": 4, "name": "Unprintable", "printed_at": None, "back": "none",
         "billed_units": 1, "printable": False},
    ],
}


class JsonHandler(http.server.BaseHTTPRequestHandler):
    """A mock that answers JSON; both mocks below share this much."""

    def log_message(self, *a):
        pass

    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class H(JsonHandler):
    def do_GET(self):
        LOG["GET"].append(self.path)
        if self.path == "/api/prefixes":
            self._send(200, PREFIXES)
        elif self.path.startswith("/api/prefixes?status="):
            status = self.path.split("=", 1)[1]
            self._send(200, [p for p in PREFIXES if p["status"] == status])
        elif self.path.endswith("/print-queue"):
            self._send(200, QUEUE)
        elif "print-raster" in self.path:
            # accept any raster path; return tiny fake BMP header for 1x1
            fake = bytes.fromhex(
                "424d3c000000000000360000002800000001000000"
                "01000000010018000000000006000000"
            ).ljust(60, b"\x00")
            self.send_response(200)
            self.send_header("Content-Type", "image/bmp")
            self.send_header("Content-Length", str(len(fake)))
            self.end_headers()
            self.wfile.write(fake)
        else:
            self._send(404, {"error": "nope"})

    def do_POST(self):
        LOG["POST"].append(self.path)
        n = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(n) if n else b""
        POST_BODIES.append(json.loads(raw) if raw else None)
        self._send(200, {"id": int(self.path.rstrip("/").split("/")[-2])})

    def do_PATCH(self):
        LOG["PATCH"].append(self.path)
        self._send(200, {"uuid": "P", "status": "printed"})


class S(socketserver.ThreadingMixIn, socketserver.TCPServer):
    allow_reuse_address = True
    daemon_threads = True


srv = S(("127.0.0.1", 0), H)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()
base = f"http://127.0.0.1:{port}"
print("mock on", base)

ok = True


def check(label, cond):
    global ok
    print(("PASS " if cond else "FAIL ") + label)
    ok = ok and cond


# 1. queue pull
c = Client(base, "tok")
q = c.get_print_queue("P")
check("queue ribbon ymckok", q["ribbon"] == "ymckok")
check("queue has 4 cards", len(q["cards"]) == 4)
check("print-queue called with bearer", LOG["GET"] == [f"/api/prefixes/P/print-queue"])

# 2. raster fetch (front + back for the black-back card, front only for none)
front1 = c.fetch_bytes("/api/prefixes/P/cards/1/print-raster/front")
back1 = c.fetch_bytes("/api/prefixes/P/cards/1/print-raster/back")
front2 = c.fetch_bytes("/api/prefixes/P/cards/2/print-raster/front")
check("front raster 200", front1[0] == 200 and front1[1].startswith(b"BM"))
check("back raster 200", back1[0] == 200 and back1[1].startswith(b"BM"))
check("single-sided still fetches front", front2[0] == 200)

# 3. mark printed — carrying the card's RFID UID when the reader read one, and
#    with no body at all when it did not (the shape a spooler without a reader
#    sends, which the server has to keep accepting).
c.mark_printed("P", 1, "123456789012")
check("mark printed POST", LOG["POST"][-1].endswith("/cards/1/printed"))
check(
    "mark printed carries the uid",
    POST_BODIES[-1] == {"serial_number": "123456789012"},
)
c.mark_printed("P", 2)
check("mark printed without a uid sends no body", POST_BODIES[-1] is None)

# 4. exercise run() queue selection via the module (import __main__ lazily)
import importlib
mm = importlib.import_module("esplp_print.__main__")
cards = q["cards"]
todo = [x for x in cards if mm._is_printed(x) is False and x.get("printable")]
check("todo skips printed(3) and non-printable(4)", [x["id"] for x in todo] == [1, 2])

# 5. ribbon family mapping
from esplp_print.printer import _family_of_sdk_name
check("YMCKO->ymcko", _family_of_sdk_name("YMCKO") == "ymcko")
check("YMCKOKOS->ymckok", _family_of_sdk_name("YMCKOKOS") == "ymckok")
check("KO->unknown", _family_of_sdk_name("KO") == "unknown")

# 6. prefix status transition (the spooler's final flush to printed)
c.set_prefix_status("P", "printed")
check("set_prefix_status PATCH", LOG["PATCH"][-1] == "/api/prefixes/P/status")

# 7. spooler helpers: _mark_printed flushes via its own client; _maybe_mark_printing
#    only advances queued -> printing (idempotent no-op for other states).
import esplp_print.__main__ as mm
mm._mark_printed(c, "P")
check("helper _mark_printed sends printed", LOG["PATCH"][-1] == "/api/prefixes/P/status")
mm._maybe_mark_printing(c, "P", "printing")
check("helper _maybe_mark_printing no-op on non-queued", LOG["PATCH"][-1] == "/api/prefixes/P/status")
before = len(LOG["PATCH"])
mm._maybe_mark_printing(c, "P", "queued")
check("helper _maybe_mark_printing advances queued", len(LOG["PATCH"]) == before + 1)

# 8. batch: list prefixes, filter by effective ribbon + printable status.
prefixes = c.list_prefixes()
check("list_prefixes returns all", len(prefixes) == 4)
jobs = [
    p for p in prefixes
    if mm._ribbon(p) == "ymcko" and p.get("status") in mm.PRINTABLE_STATUSES
]
check("batch ymcko picks A,B (not C ymckok, not D draft)",
      [p["uuid"] for p in jobs] == ["A", "B"])
jobs_ck = [
    p for p in prefixes
    if mm._ribbon(p) == "ymckok" and p.get("status") in mm.PRINTABLE_STATUSES
]
check("batch ymckok picks C", [p["uuid"] for p in jobs_ck] == ["C"])

# 9. the UID step, which is two halves of two devices. The printer feeds the card
#    and parks it at the contactless station — that is what makes the UID the id
#    of THIS card — and the reader in the encoder slot is then asked over HTTP
#    for the UID of what is on its coil. The printer half is faked (no SDK, no
#    wheel); the reader half is a second mock server, so the request it is sent
#    and the JSON it answers are both real.
READER = {"state": "present", "uid": "004162179196", "query": None}


class R(JsonHandler):
    def do_GET(self):
        READER["query"] = self.path
        if READER["state"] == "absent":
            self._send(404, {"present": False})
        elif READER["state"] == "error":
            self._send(500, {"error": "boom"})
        elif READER["state"] == "junk":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", "1")
            self.end_headers()
            self.wfile.write(b"{")
        else:
            self._send(200, {
                "present": True,
                "uid": "04-A2-B3-C4",
                "uid_decimal": READER["uid"],
                "kind": "mifare-classic",
                "sak": 8,
                "atqa": "0004",
                "last_seen_ms": 7,
            })


rsrv = S(("127.0.0.1", 0), R)
rport = rsrv.server_address[1]
threading.Thread(target=rsrv.serve_forever, daemon=True).start()
reader = NfcReader(f"http://127.0.0.1:{rport}")
print("mock reader on", reader.base)


class FakeEv:
    class CardPos:
        CONTACTLESS = 6


class FakeCo:
    def __init__(self, ok=True):
        self.calls = []
        self.ok = ok

    def insert_card(self):
        self.calls.append("insert_card")
        return self.ok

    def set_card_pos(self, pos):
        self.calls.append(("set_card_pos", pos))
        return self.ok

    def eject_card(self):
        self.calls.append("eject_card")
        return self.ok

    def get_last_error(self):
        return "none"


pr = Printer("socket://127.0.0.1?type=evolis", reader)
pr._evolis = FakeEv
pr._co = FakeCo()
uid = pr.read_card_uid()
check(
    "the card is fed and parked at the contactless station before printing",
    pr._co.calls == ["insert_card", ("set_card_pos", FakeEv.CardPos.CONTACTLESS)],
)
check("the read asks the reader for the card and waits for it to appear",
      READER["query"] == f"/nfc/card?wait_ms={DEFAULT_WAIT_MS}")
check("the reader's uid_decimal is handed over as the card's serial_number",
      uid == "004162179196")

# A card the reader cannot see is a definitive "no UID", not a failure.
READER["state"] = "absent"
pr._co = FakeCo()
check("a coil with no card on it reads as no UID", pr.read_card_uid() is None)

# A reader that cannot be asked is a ReaderError — a distinct answer from the
# one above, but the spooler degrades both to "no UID" the same way.
READER["state"] = "error"
pr._co = FakeCo()
failed = None
try:
    pr.read_card_uid()
except ReaderError as e:
    failed = str(e)
check("a reader answering an HTTP error is a ReaderError", failed is not None
      and "500" in failed)

READER["state"] = "junk"
pr._co = FakeCo()
failed = None
try:
    pr.read_card_uid()
except ReaderError as e:
    failed = str(e)
check("so is a reader answering junk", failed is not None and "JSON" in failed)
READER["state"] = "present"

# A reader that is not on the network at all: nothing listening on that port.
dead = Printer("socket://127.0.0.1?type=evolis", NfcReader("http://127.0.0.1:1", 0))
dead._evolis = FakeEv
dead._co = FakeCo()
failed = None
try:
    dead.read_card_uid()
except ReaderError as e:
    failed = str(e)
check("so is a reader that is not there",
      failed is not None and "unreachable" in failed)
check("the wait is clamped to what the firmware honours",
      NfcReader("http://x", 90_000).wait_ms == 30_000)

# The printer half is the ordering guarantee, so both of its failures are tested
# — and neither may reach the reader (no card at the antenna, nothing to read).
READER["query"] = None
pr._co = FakeCo(ok=False)
failed = None
try:
    pr.read_card_uid()
except PrinterError as e:
    failed = str(e)
check(
    "a card that cannot be fed is a PrinterError",
    failed is not None and "feed" in failed,
)
check("and the reader is never asked", READER["query"] is None)


class NoStationCo(FakeCo):
    """The card feeds fine; the printer has no contactless station to move it to."""

    def set_card_pos(self, pos):
        self.calls.append(("set_card_pos", pos))
        return False


pr._co = NoStationCo()
failed = None
try:
    pr.read_card_uid()
except PrinterError as e:
    failed = str(e)
check(
    "a station that is not there is a PrinterError naming the reader",
    failed is not None and "contactless" in failed,
)
check("and the reader is never asked either", READER["query"] is None)


class StubPrinter:
    def __init__(self, uid=None, boom=None):
        self.uid = uid
        self.boom = boom

    def read_card_uid(self):
        if self.boom is not None:
            raise self.boom
        return self.uid


check("a card with no UID still prints (the helper never raises)",
      mm._card_uid(StubPrinter(boom=PrinterError("cannot park the card")), 7) is None)
unreachable = StubPrinter(boom=ReaderError("http://nfc-reader.local unreachable"))
check("so does a card whose reader could not be asked at all",
      mm._card_uid(unreachable, 7) is None)
check("and a card WITH a UID hands it over", mm._card_uid(StubPrinter("012345678901"), 7)
      == "012345678901")

# 10. --no-print: the card walks the whole path except the print itself. The
#     buffers the printer would be handed are set for real (that is where a
#     depth or raster bug lives), and the card is then ejected instead of
#     submitted — no ribbon spent, card reusable.
class FakePs:
    """Stands in for evolis.PrintSession: records what the SDK was asked to do."""

    def __init__(self):
        self.calls = []

    def init(self):
        self.calls.append("init")
        return True

    def set_image_buffer(self, face, buf):
        self.calls.append(("set_image_buffer", face, len(buf.raw)))
        return True

    def set_black_buffer(self, face, buf):
        self.calls.append(("set_black_buffer", face, len(buf.raw)))
        return True

    def print(self):
        self.calls.append("print")
        return 0

    def get_last_error(self):
        return "none"


class Face:
    FRONT = "front"
    BACK = "back"


class Rc:
    OK = 0


class FakePsEv:
    CardPos = FakeEv.CardPos
    CardFace = Face
    ReturnCode = Rc

    def __init__(self):
        self.ps = FakePs()

    def PrintSession(self, co):  # noqa: N802 - the SDK's own name
        return self.ps


def _printer_with_session():
    printer = Printer("socket://127.0.0.1?type=evolis", reader)
    printer._evolis = ev = FakePsEv()
    printer._co = co = FakeCo()
    return printer, ev, co


FRONT, BACK = b"FRONTBMP", b"BACKBMP1"  # +1 below: create_string_buffer adds a NUL
staged, ev, co = _printer_with_session()
staged.print_card(FRONT, "black", BACK, send=False)
check(
    "a staged card goes through the same raster path as a printed one",
    ev.ps.calls == [
        "init",
        ("set_image_buffer", "front", len(FRONT) + 1),
        ("set_black_buffer", "back", len(BACK) + 1),
    ],
)
check("but is never submitted to the print engine", "print" not in ev.ps.calls)
check("and is ejected instead of printed", co.calls == ["eject_card"])

printed, ev, co = _printer_with_session()
printed.print_card(FRONT, "black", BACK)
check("a real print does submit the job", ev.ps.calls[-1] == "print")
check("and ejects nothing itself (the job takes the card)", co.calls == [])

colour, ev, _ = _printer_with_session()
colour.print_card(FRONT, "color", BACK)
check(
    "the back's DEPTH picks the call, never its source: colour is 24-bit",
    ev.ps.calls[2] == ("set_image_buffer", "back", len(BACK) + 1),
)
single, ev, _ = _printer_with_session()
single.print_card(FRONT, "none", None)
check(
    "a single-sided card sets the front and no back",
    [c[0] if isinstance(c, tuple) else c for c in ev.ps.calls]
    == ["init", "set_image_buffer", "print"],
)

stuck, _ev, _co = _printer_with_session()
stuck._co = FakeCo(ok=False)
failed = None
try:
    stuck.print_card(FRONT, "none", None, send=False)
except PrinterError as e:
    failed = str(e)
check("a card that will not eject is a PrinterError", failed is not None
      and "eject" in failed)

# 11. the CLI wiring: where the reader is, how long it may wait, and the two
#     "don't print" modes. The environment is cleared first so the defaults
#     below are the CODE's, not this shell's.
os.environ.pop("ESPLP_READER", None)
os.environ.pop("ESPLP_TOKEN", None)
args = mm.build_parser().parse_args(["--ip", "socket://x", "--prefix", "P"])
check("--reader defaults to the reader's mDNS name", args.reader == DEFAULT_READER_URL)
check("--uid-wait-ms defaults to the read's own default",
      args.uid_wait_ms == DEFAULT_WAIT_MS)
args = mm.build_parser().parse_args([
    "--ip", "socket://x", "--prefix", "P",
    "--reader", "http://reader.lan", "--uid-wait-ms", "500",
])
check("both are overridable",
      (args.reader, args.uid_wait_ms) == ("http://reader.lan", 500))
built = mm._reader(args)
check("and the printer gets the reader they describe",
      (built.base, built.wait_ms) == ("http://reader.lan", 500))

plain = mm.build_parser().parse_args(["--ip", "socket://x", "--prefix", "P"])
check("--no-print is off unless asked for", plain.no_print is False)
check("--no-print with --dry-run is refused (a dry run prints nothing anyway)",
      mm.main(["--ip", "socket://x", "--prefix", "P", "--token", "t",
               "--dry-run", "--no-print"]) == 2)
check("--no-print still needs the token, because it marks cards",
      mm.main(["--ip", "socket://x", "--prefix", "P", "--no-print"]) == 2)

rsrv.shutdown()
srv.shutdown()
print("RESULT:", "OK" if ok else "FAILED")
sys.exit(0 if ok else 1)
