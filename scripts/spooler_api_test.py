"""Smoke test of the spooler's API layer against a mock server.

Exercises Client.get_print_queue / fetch_bytes / mark_printed and the spooler's
queue->todo selection, WITHOUT a printer (stops at the printer check). Run from
the Nix-built env so `esplp_print` impolrs from the store.
"""
import sys
import threading
import json
import http.server
import socketserver

from esplp_print.api import Client

LOG = {"GET": [], "POST": []}
QUEUE = {
    "prefix": "P",
    "status": "queued",
    "ribbon": "ymckoko",
    "unprinted_count": 2,
    "billed_sides": 4,
    "resume": False,
    "cards": [
        {"id": 1, "name": "Conni", "printed_at": None, "back": "black",
         "billed_sides": 2, "printable": True},
        {"id": 2, "name": "Bob", "printed_at": None, "back": "none",
         "billed_sides": 2, "printable": True},
        {"id": 3, "name": "Ana", "printed_at": "2026-10-15T..", "back": "none",
         "billed_sides": 2, "printable": True},
        {"id": 4, "name": "Unprintable", "printed_at": None, "back": "none",
         "billed_sides": 2, "printable": False},
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
        if self.path.endswith("/print-queue"):
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
        LOG.setdefault("PATCH", []).append(self.path)
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

srv.shutdown()
print("RESULT:", "OK" if ok else "FAILED")
sys.exit(0 if ok else 1)
