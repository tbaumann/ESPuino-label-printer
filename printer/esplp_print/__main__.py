"""ESPuino label printer spooler — the local CLI that prints paid card orders.

Pulls a prefix's print queue from the server, prints each unprinted card on the
attached Evolis printer, and marks each card printed once it is in the output
hopper. Prefix status transitions are deliberately NOT handled here — they
belong to the operator/backend; this tool only prints and marks.

`--dry-run` does everything a real run does — fetch the queue, open the printer,
check the ribbon, fetch every raster — except the two irreversible actions: it
does not send a print job and does not mark any card printed.

Exit codes:
  0  complete (all unprinted printable cards printed, or a dry run)
  1  an error stopped the run (cards before it were printed and marked; rerun
     to resume — the server's `printed_at` skips them)
  2  usage error
"""

from __future__ import annotations

import argparse
import sys
from typing import Any

from .api import ApiError, Client
from .printer import (
    BACK_BLACK,
    BACK_COLOR,
    BACK_NONE,
    Printer,
    PrinterError,
    RibbonStatus,
)

RASTER_FRONT = "/api/prefixes/{prefix}/cards/{id}/print-raster"
RASTER_BACK = "/api/prefixes/{prefix}/cards/{id}/print-raster/back"


def _raster(client: Client, path: str, desc: str) -> bytes:
    status, body = client.fetch_bytes(path)
    if status == 200:
        return body
    if status == 404:
        raise RuntimeError(f"{desc}: no such side (404)")
    if status == 423:
        raise RuntimeError(f"{desc}: still rendering (423); retry shortly")
    if status == 500:
        raise RuntimeError(f"{desc}: render failed (500); see the card render status")
    raise RuntimeError(f"{desc}: unexpected HTTP {status}")


def _ribbon_check(loaded: RibbonStatus, expected: str, ignore: bool) -> bool:
    """Sanity-check the loaded ribbon against what the queue was priced for.

    Returns True if the run may proceed (either matched, or mismatch ignored).
    The check is informational per design: capacity is a warning, and a mismatched
    ribbon is refuse-by-default but skippable with --ignore-ribbon (the operator
    may deliberately load something and press on, or the enum mapping be imperfect).
    """
    family = loaded.ribbon
    if family == expected:
        return True
    if ignore:
        print(
            f"[ribbon] WARNING: loaded ribbon reports '{family}' but the job "
            f"expects '{expected}'; continuing due to --ignore-ribbon",
            file=sys.stderr,
        )
        return True
    raise PrinterError(
        f"loaded ribbon '{family}' does not match the job's '{expected}'. "
        "Refusing to print. Load the right ribbon, or pass --ignore-ribbon to "
        "press on regardless."
    )


def _ribbon_capacity_warning(loaded: RibbonStatus) -> None:
    if loaded.remaining is None or not loaded.capacity:
        return
    remaining = loaded.remaining
    if remaining <= 0:
        print(
            f"[ribbon] WARNING: ribbon is depleted ({remaining} panels left); "
            "cards printed until it runs out, then reload and rerun",
            file=sys.stderr,
        )
    elif remaining < loaded.capacity * 0.25:
        print(
            f"[ribbon] WARNING: ribbon low ({remaining}/{loaded.capacity} panels "
            "remaining)",
            file=sys.stderr,
        )


def _is_printed(card: dict[str, Any]) -> bool:
    return card.get("printed_at") is not None


def run(args: argparse.Namespace) -> int:
    client = Client(args.server, args.token)
    try:
        queue = client.get_print_queue(args.prefix)
    except ApiError as e:
        print(f"error: fetching print queue: {e}", file=sys.stderr)
        return 1

    ribbon = queue.get("ribbon")
    cards = queue.get("cards", [])
    print(
        f"prefix {args.prefix}: status={queue.get('status')} ribbon={ribbon} "
        f"unprinted={queue.get('unprinted_count')} billed_sides={queue.get('billed_sides')}"
    )

    todo = [c for c in cards if not _is_printed(c) and c.get("printable")]
    not_printable = [
        c for c in cards if not _is_printed(c) and not c.get("printable")
    ]
    if not_printable:
        ids = ", ".join(str(c["id"]) for c in not_printable)
        print(
            f"[queue] NOTE: {len(not_printable)} unprinted card(s) are not printable "
            f"(raster pending/failed) and will be skipped this run: {ids}",
            file=sys.stderr,
        )
    if not todo:
        print(
            f"nothing to print (queue of {len(cards)} cards, "
            f"{len(not_printable)} not printable)"
        )
        return 0

    verb = "would print" if args.dry_run else "printing"
    print(
        f"{verb} {len(todo)} card(s)"
        + ("" if not args.dry_run else " (dry run: no jobs sent, nothing marked)")
    )

    # A dry run never changes status.
    if not args.dry_run:
        _maybe_mark_printing(client, args.prefix, queue.get("status"))

    # Open the printer and check the ribbon in BOTH modes — a dry run must
    # exercise the same path, so a run that prints is already proven to connect.
    with Printer(args.ip) as printer:
        printer.require_ready()
        loaded = printer.ribbon_status()
        print(
            f"[ribbon] loaded '{loaded.ribbon}' (product {loaded.product_code!r}, "
            f"remaining={loaded.remaining})"
        )
        _ribbon_capacity_warning(loaded)
        if ribbon:
            _ribbon_check(loaded, ribbon, args.ignore_ribbon)

        for i, card in enumerate(todo, 1):
            cid = card["id"]
            back = card.get("back", BACK_NONE)
            print(
                f"[{i}/{len(todo)}] {verb} card {cid} '{card.get('name')}'",
                flush=True,
            )
            try:
                front = _raster(
                    client,
                    RASTER_FRONT.format(prefix=args.prefix, id=cid),
                    f"front raster of card {cid}",
                )
                back_bmp = None
                if back in (BACK_COLOR, BACK_BLACK):
                    back_bmp = _raster(
                        client,
                        RASTER_BACK.format(prefix=args.prefix, id=cid),
                        f"back raster of card {cid}",
                    )
                if args.dry_run:
                    # Everything fetched and verified; do not send or mark.
                    print(
                        f"  (ok to print: front {len(front)}B"
                        + (f", back {len(back_bmp)}B" if back_bmp else ", single-sided")
                        + ")",
                        flush=True,
                    )
                    continue
                printer.print_card(front, back, back_bmp)
                # The print call returned OK: the card is ejected to the hopper.
                # Mark it NOW so a crash cannot reprint it, and so a parallel
                # run does not pull it. The server endpoint is idempotent.
                client.mark_printed(args.prefix, cid)
                print(f"  -> card {cid} printed and marked", flush=True)
            except (PrinterError, ApiError, RuntimeError) as e:
                print(f"error: card {cid} failed: {e}", file=sys.stderr)
                print(
                    f"stopping. {i - 1} card(s) printed this run; rerun to resume.",
                    file=sys.stderr,
                )
                return 1

    print(f"done: {len(todo)} card(s) {'inspected' if args.dry_run else 'printed'}")

    # The whole run succeeded: flush the work list to `printed`. Skip a prefix
    # that was left in a state the transition does not allow (e.g. a draft left
    # over from a test) — that is an operator decision, not something to paper
    # over, but it must not look like a spooler failure either.
    if not args.dry_run and not not_printable:
        _mark_printed(client, args.prefix)
    return 0


def _maybe_mark_printing(client: Client, prefix: str, status: Any) -> None:
    """Advance a `queued` prefix to `printing` before the run, so the server's
    `resume` logic (printing + still-unprinted) reflects this is an in-progress
    order. No-op for any other status — the transition is queued→printing only.
    """
    if status != "queued":
        return
    _set_status_tolerant(client, prefix, "printing", "queued -> printing")


def _mark_printed(client: Client, prefix: str) -> None:
    """Flip the prefix to `printed` once every card is printed. Tolerates the
    409 an already-`printed` prefix or a non-printable-status state would give
    (idempotent / operator's call), and any transient API failure."""
    _set_status_tolerant(client, prefix, "printed", "-> printed")


def _set_status_tolerant(client: Client, prefix: str, status: str, desc: str) -> None:
    try:
        client.set_prefix_status(prefix, status)
        print(f"[status] prefix marked {status}")
    except ApiError as e:
        if e.status == 409:
            print(
                f"[status] note: prefix could not be marked {status} "
                f"({desc} not a valid transition from its current state): {e}",
                file=sys.stderr,
            )
        else:
            print(
                f"[status] warning: failed to mark prefix {status}: {e}",
                file=sys.stderr,
            )


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="esplp-print",
        description="Print a paid order (prefix) on the attached Evolis printer.",
    )
    p.add_argument("--server", default="http://127.0.0.1:8080", help="API base URL")
    p.add_argument(
        "--token",
        default=None,
        help="operator bearer token (or set ESPLP_TOKEN)",
    )
    p.add_argument("--prefix", required=True, help="prefix uuid to print")
    p.add_argument(
        "--ip",
        required=True,
        help="printer connection string, e.g. socket://192.168.2.101?type=evolis",
    )
    p.add_argument(
        "--ignore-ribbon",
        action="store_true",
        help="print even when the loaded ribbon does not match the job's ribbon",
    )
    p.add_argument(
        "--dry-run",
        action="store_true",
        help="do everything except send print jobs and mark cards printed",
    )
    return p


def main(argv: list[str] | None = None) -> int:
    import os

    args = build_parser().parse_args(argv)
    if not args.token:
        args.token = os.environ.get("ESPLP_TOKEN")
    if not args.token and not args.dry_run:
        # print-queue and mark-printed are admin-only; so is the queue fetch.
        print("error: --token (or ESPLP_TOKEN) is required to print", file=sys.stderr)
        return 2
    try:
        return run(args)
    except ApiError as e:
        print(f"error: API: {e}", file=sys.stderr)
        return 1
    except PrinterError as e:
        print(f"error: printer: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
