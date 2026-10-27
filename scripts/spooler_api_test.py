"""Smoke test of the spooler's API layer against a mock server.

Exercises Client.get_print_queue / fetch_bytes / mark_printed / list_prefixes /
set_prefix_status and the spooler's queue->todo selection and batch filtering,
WITHOUT a printer (stops at the printer check). Run from the Nix-built env so
`esplp_print` imports from the store.
"""
import sys
import threading
import json
import http.server
import socketserver

from esplp_print.api import Client
from esplp_print.printer import Printer, PrinterError

LOG = {"GET": [], "POST": [], "PATCH": []}
# The parsed JSON body of each POST (None when the call sent none).
POST_BODIES: list = []
# Prefixes for the batch mode: two ymcko jobs (one paid, one queued), one
# ymckoko job (should be excluded when filtering ymcko), one draft (excluded).
PREFIXES = [
    {"uuid": "A", "status": "paid", "ribbon": "ymcko", "unprinted_count": 2},
    {"uuid": "B", "status": "queued", "ribbon": "ymcko", "unprinted_count": 1},
    {"uuid": "C", "status": "paid", "ribbon": "ymckoko", "unprinted_count": 3},
    {"uuid": "D", "status": "draft", "ribbon": "ymcko", "unprinted_count": 0},
]
QUEUE = {
    "prefix": "P",
    "status": "queued",
    "ribbon": "ymckoko",
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


class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

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
check("queue ribbon ymckoko", q["ribbon"] == "ymckoko")
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
check("YMCKOKOS->ymckoko", _family_of_sdk_name("YMCKOKOS") == "ymckoko")
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
check("batch ymcko picks A,B (not C ymckoko, not D draft)",
      [p["uuid"] for p in jobs] == ["A", "B"])
jobs_ck = [
    p for p in prefixes
    if mm._ribbon(p) == "ymckoko" and p.get("status") in mm.PRINTABLE_STATUSES
]
check("batch ymckoko picks C", [p["uuid"] for p in jobs_ck] == ["C"])

# 9. the UID step. The card is fed and parked at the contactless station before
#    it is printed — that is what makes the UID the id of THIS card — and the
#    read at that station is a stub today (no reader installed), so it reports no
#    UID rather than inventing one. A connection is faked here: no printer, no
#    SDK, no wheel.
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

    def get_last_error(self):
        return "none"


pr = Printer("socket://127.0.0.1?type=evolis")
pr._evolis = FakeEv
pr._co = FakeCo()
uid = pr.read_card_uid()
check(
    "the card is fed and parked at the contactless station before printing",
    pr._co.calls == ["insert_card", ("set_card_pos", FakeEv.CardPos.CONTACTLESS)],
)
check("the UID read is stubbed: no UID is invented", uid is None)

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


class StubPrinter:
    def __init__(self, uid=None, boom=False):
        self.uid = uid
        self.boom = boom

    def read_card_uid(self):
        if self.boom:
            raise PrinterError("cannot park the card under the contactless station")
        return self.uid


check("a card with no UID still prints (the helper never raises)",
      mm._card_uid(StubPrinter(boom=True), 7) is None)
check("and a card WITH a UID hands it over", mm._card_uid(StubPrinter("012345678901"), 7)
      == "012345678901")

srv.shutdown()
print("RESULT:", "OK" if ok else "FAILED")
sys.exit(0 if ok else 1)
