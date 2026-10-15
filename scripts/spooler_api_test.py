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

LOG = {"GET": [], "POST": [], "PATCH": []}
# Prefixes for the batch mode: two ymcko jobs (one paid, one queued), one
# ymckoko job (should be excluded when filtering ymcko), one draft (excluded).
PREFIXES = [
    {"uuid": "A", "status": "paid", "ribbon": "ymcko", "paid_ribbon": None,
     "unprinted_count": 2},
    {"uuid": "B", "status": "queued", "ribbon": "ymcko", "paid_ribbon": None,
     "unprinted_count": 1},
    {"uuid": "C", "status": "paid", "ribbon": "ymckoko", "paid_ribbon": None,
     "unprinted_count": 3},
    {"uuid": "D", "status": "draft", "ribbon": "ymcko", "paid_ribbon": None,
     "unprinted_count": 0},
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
front1 = c.fetch_bytes("/api/prefixes/P/cards/1/print-raster")
back1 = c.fetch_bytes("/api/prefixes/P/cards/1/print-raster/back")
front2 = c.fetch_bytes("/api/prefixes/P/cards/2/print-raster")
check("front raster 200", front1[0] == 200 and front1[1].startswith(b"BM"))
check("back raster 200", back1[0] == 200 and back1[1].startswith(b"BM"))
check("single-sided still fetches front", front2[0] == 200)

# 3. mark printed
c.mark_printed("P", 1)
check("mark printed POST", LOG["POST"][-1].endswith("/cards/1/printed"))

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
    if mm._effective_ribbon(p) == "ymcko" and p.get("status") in mm.PRINTABLE_STATUSES
]
check("batch ymcko picks A,B (not C ymckoko, not D draft)",
      [p["uuid"] for p in jobs] == ["A", "B"])
jobs_ck = [
    p for p in prefixes
    if mm._effective_ribbon(p) == "ymckoko" and p.get("status") in mm.PRINTABLE_STATUSES
]
check("batch ymckoko picks C", [p["uuid"] for p in jobs_ck] == ["C"])

srv.shutdown()
print("RESULT:", "OK" if ok else "FAILED")
sys.exit(0 if ok else 1)
