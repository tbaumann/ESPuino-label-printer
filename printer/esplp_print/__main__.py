"""ESPuino label printer spooler — the local CLI that prints paid card orders.

Pulls a prefix's print queue from the server, prints each unprinted card on the
attached Evolis printer, and marks each card printed once it is in the output
hopper. Prefix status transitions are deliberately NOT handled here — they
belong to the operator/backend; this tool only prints and marks.

`--dry-run` stops short of the printer — fetch the queue, open the printer, check
the ribbon, fetch every raster — but feeds no card, sends no print job and marks
nothing.

`--no-print` runs the whole path EXCEPT the printing: the card is fed, parked at
the contactless station and its UID read, the job is staged with the same buffers
it would be printed from, and the card is then ejected blank (no ribbon spent, and
the card can go straight back in the feeder). It does mark each card and close the
order out, because that write is where the UID lands — so printing for real
afterwards means setting the order back to `queued`, which clears `printed_at`.

`--batch` prints every prefix that needs a given ribbon, one after another. The
operator loads that ribbon once, and between jobs the tool pauses and asks them
to empty the output tray before the next prefix starts.

Each card's RFID UID is read before it is printed: the card is fed, parked at the
contactless station and read there, and the print job takes it from there. The
reader is its own device (`reader.py`, reachable via `--reader`), asked over HTTP
for the UID of the card the printer has just moved to it, and the UID goes onto
the card with the print mark. A reader that cannot be asked, or a card with no
readable chip, is never fatal: the card prints with a NULL `serial_number`, and
the run names those cards at the end. `--dry-run` skips the read — feeding a card
is a physical action, and nothing would print it afterwards.

Exit codes:
  0  complete (all unprinted printable cards printed — or staged, under
     --no-print, or simply reported, under --dry-run)
  1  an error stopped the run (cards before it were printed and marked; rerun
     to resume — the server's `printed_at` skips them)
  2  usage error
"""

from __future__ import annotations

import argparse
import os
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
from .reader import DEFAULT_READER_URL, DEFAULT_WAIT_MS, NfcReader, ReaderError

RASTER_FRONT = "/api/prefixes/{prefix}/cards/{id}/print-raster/front"
RASTER_BACK = "/api/prefixes/{prefix}/cards/{id}/print-raster/back"

# Lifecycle statuses that represent work waiting to be printed. A batch run
# picks prefixes in these states; everything else (draft, cancelled, printed)
# is not a print job.
PRINTABLE_STATUSES = {"paid", "queued", "printing"}


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


def _reader(args: argparse.Namespace) -> NfcReader:
    """The encoder-slot reader, as configured on the command line."""
    return NfcReader(args.reader, args.uid_wait_ms)


def _card_uid(printer: Printer, card_id: int) -> str | None:
    """The RFID UID of the card about to be printed, or None when there is none.

    Best effort on purpose: a card the reader cannot see — or a reader that
    cannot be asked — still prints, with the server's `serial_number` left NULL,
    and the end of the run names it. Stopping the run instead would strand a paid
    order half-printed over a read nothing else depends on.
    """
    try:
        return printer.read_card_uid()
    except (PrinterError, ReaderError) as e:
        print(f"  [uid] WARNING: card {card_id} not read: {e}", file=sys.stderr)
        return None


def _ribbon(prefix: dict[str, Any]) -> str | None:
    """The ribbon a prefix prints with: its own `ribbon`.

    The server's queue prints (and renders) from this same field, so the batch
    filter must read it too or a job would be picked up that is not meant for the
    loaded ribbon.
    """
    return prefix.get("ribbon")


def print_prefix(client: Client, args: argparse.Namespace, prefix: str) -> int:
    """Print one prefix's unprinted cards. Returns 0 on success, 1 on failure.

    Opens the printer itself (so batch mode can call it per prefix and keep the
    connection fresh between jobs). A dry run never sends jobs or marks cards.
    """
    try:
        queue = client.get_print_queue(prefix)
    except ApiError as e:
        print(f"error: fetching print queue: {e}", file=sys.stderr)
        return 1

    ribbon = queue.get("ribbon")
    cards = queue.get("cards", [])
    print(
        f"prefix {prefix}: status={queue.get('status')} ribbon={ribbon} "
        f"unprinted={queue.get('unprinted_count')} billed_units={queue.get('billed_units')}"
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
        # A rerun after the final card was already printed still closes the
        # prefix out: every card is printed, so flush to `printed`.
        if not args.dry_run and not not_printable:
            _mark_printed(client, prefix)
        return 0

    if args.dry_run:
        verb, note = "would print", " (dry run: no jobs sent, nothing marked)"
    elif args.no_print:
        verb, note = "staging", " (no-print: nothing is printed, no ribbon spent)"
    else:
        verb, note = "printing", ""
    print(f"{verb} {len(todo)} card(s){note}")

    # A dry run never changes status.
    if not args.dry_run:
        _maybe_mark_printing(client, prefix, queue.get("status"))

    # Open the printer and check the ribbon in BOTH modes — a dry run must
    # exercise the same path, so a run that prints is already proven to connect.
    with Printer(args.ip, _reader(args)) as printer:
        printer.require_ready()
        loaded = printer.ribbon_status()
        print(
            f"[ribbon] loaded '{loaded.ribbon}' (product {loaded.product_code!r}, "
            f"remaining={loaded.remaining})"
        )
        _ribbon_capacity_warning(loaded)
        if ribbon:
            _ribbon_check(loaded, ribbon, args.ignore_ribbon)
        # Cards that printed with no UID read: reported together at the end, so
        # they cannot disappear into a NULL column unnoticed.
        no_uid: list[int] = []

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
                    RASTER_FRONT.format(prefix=prefix, id=cid),
                    f"front raster of card {cid}",
                )
                back_bmp = None
                if back in (BACK_COLOR, BACK_BLACK):
                    back_bmp = _raster(
                        client,
                        RASTER_BACK.format(prefix=prefix, id=cid),
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
                # The card is fed and parked under the contactless station for
                # the UID read; on the way out either the print job takes it from
                # there, or (--no-print) it is ejected, staged but unprinted. A
                # dry run reaches neither: feeding a card is a physical action.
                uid = _card_uid(printer, cid)
                printer.print_card(front, back, back_bmp, send=not args.no_print)
                # The card is out of the printer either way: printed, or ejected
                # blank by --no-print. Mark it NOW so a crash cannot reprint it,
                # and so a parallel run does not pull it. The server endpoint is
                # idempotent, and it takes the UID with the mark — the only card
                # write that still applies once the order is being printed.
                client.mark_printed(prefix, cid, uid)
                if uid is None:
                    no_uid.append(cid)
                what = "staged (not printed)" if args.no_print else "printed"
                print(
                    f"  -> card {cid} {what} and marked"
                    + (f" (uid {uid})" if uid else ""),
                    flush=True,
                )
            except (PrinterError, ApiError, RuntimeError) as e:
                print(f"error: card {cid} failed: {e}", file=sys.stderr)
                print(
                    f"stopping. {i - 1} card(s) printed this run; rerun to resume.",
                    file=sys.stderr,
                )
                return 1

    if args.dry_run:
        done = "inspected"
    elif args.no_print:
        done = "staged (not printed)"
    else:
        done = "printed"
    print(f"done: {len(todo)} card(s) {done}")

    # A UID that was never read is a card nothing can map back to a chip: an
    # external client promotes its cards by UID, so a run that produced any has
    # to name them rather than let them disappear into a NULL column.
    if no_uid:
        ids = ", ".join(str(i) for i in no_uid)
        print(
            f"[uid] NOTE: {len(no_uid)} card(s) "
            f"{'staged' if args.no_print else 'printed'} without a UID read: "
            f"{ids} — nothing maps those cards to a chip",
            file=sys.stderr,
        )

    if args.no_print:
        # Say what the blank cards in the hopper mean, and how to turn this into
        # a real print: the run marked them (that is where the UID landed), so
        # the prefix is about to be closed as `printed` with nothing on the
        # cards. `queued` is the operator's reprint lever and clears printed_at.
        print(
            "[no-print] NOTE: nothing was printed — the cards just ejected are "
            "blank and can go back in the feeder. The server was told each card "
            "was ejected, UIDs included, so set the order back to 'queued' and "
            "rerun without --no-print to print it for real.",
            file=sys.stderr,
        )

    # The whole run succeeded: flush the work list to `printed`. The server
    # forces any transition for the admin token, so this works whatever state
    # the prefix was in before (paid, queued, printing, even a leftover draft)
    # — anything fully printed becomes `printed`.
    if not args.dry_run and not not_printable:
        _mark_printed(client, prefix)
    return 0


def run_batch(client: Client, args: argparse.Namespace) -> int:
    """Print every prefix that needs a ribbon, one after another.

    The ribbon is `args.ribbon` when given, otherwise read from the printer
    (the operator has already loaded it). Lists all prefixes, keeps those whose
    effective ribbon matches and whose status is a print job, then prints each.
    Between jobs it pauses and asks the operator to empty the output tray,
    naming the prefix that was just printed.
    """
    ribbon = args.ribbon
    if ribbon is None:
        # Autodetect from the loaded ribbon. A dry run still opens the printer
        # (it must exercise the same path), so this works in both modes.
        try:
            with Printer(args.ip, _reader(args)) as printer:
                printer.require_ready()
                loaded = printer.ribbon_status()
        except PrinterError as e:
            print(f"error: cannot read ribbon from printer: {e}", file=sys.stderr)
            return 1
        if loaded.ribbon == "unknown":
            print(
                "error: printer reports an unrecognised ribbon; pass --ribbon "
                "explicitly",
                file=sys.stderr,
            )
            return 1
        ribbon = loaded.ribbon
        print(f"[ribbon] autodetected '{ribbon}' from the printer")

    try:
        prefixes = client.list_prefixes()
    except ApiError as e:
        print(f"error: listing prefixes: {e}", file=sys.stderr)
        return 1

    jobs = [
        p
        for p in prefixes
        if _ribbon(p) == ribbon
        and p.get("status") in PRINTABLE_STATUSES
    ]
    if not jobs:
        print(
            f"no prefixes to print for ribbon '{ribbon}' "
            f"(of {len(prefixes)} total)"
        )
        return 0

    print(
        f"batch: {len(jobs)} prefix(es) for ribbon '{ribbon}': "
        + ", ".join(p["uuid"] for p in jobs)
    )

    for i, p in enumerate(jobs, 1):
        prefix = p["uuid"]
        print(f"\n=== job {i}/{len(jobs)}: prefix {prefix} ===")
        rc = print_prefix(client, args, prefix)
        if rc != 0:
            print(
                f"batch stopped at prefix {prefix} (job {i}/{len(jobs)}); "
                "fix and rerun to continue",
                file=sys.stderr,
            )
            return rc
        # Pause before the next job so the operator can empty the tray. The
        # message names the prefix just printed, so they know which cards are
        # in the hopper.
        if i < len(jobs):
            nxt = jobs[i]["uuid"]
            input(
                f"\nEmpty the output tray (prefix {prefix} printed). "
                f"Press Enter to print prefix {nxt}... "
            )

    print(f"\nbatch done: {len(jobs)} prefix(es) printed")
    return 0


def _maybe_mark_printing(client: Client, prefix: str, status: Any) -> None:
    """Advance a `queued` prefix to `printing` before the run, so the server's
    `resume` logic (printing + still-unprinted) reflects an in-progress order.
    No-op for any other status — the transition is queued→printing only."""
    if status != "queued":
        return
    _mark_status(client, prefix, "printing")


def _mark_printed(client: Client, prefix: str) -> None:
    """Flip the prefix to `printed` once every card is printed. Admin-forced, so
    it always succeeds from any state; a failure is a real error worth surfacing.
    Re-applying an already-`printed` status is a no-op server-side."""
    _mark_status(client, prefix, "printed")


def _mark_status(client: Client, prefix: str, status: str) -> None:
    try:
        client.set_prefix_status(prefix, status)
        print(f"[status] prefix marked {status}")
    except ApiError as e:
        print(
            f"[status] ERROR: failed to mark prefix {status}: {e}",
            file=sys.stderr,
        )
        raise


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="card-printer",
        description="Print a paid order (prefix) on the attached Evolis printer.",
    )
    p.add_argument("--server", default="http://127.0.0.1:8080", help="API base URL")
    p.add_argument(
        "--token",
        default=None,
        help="operator bearer token (or set ESPLP_TOKEN)",
    )
    p.add_argument(
        "--prefix",
        default=None,
        help="prefix uuid to print (omit with --batch)",
    )
    p.add_argument(
        "--batch",
        action="store_true",
        help="print every prefix that needs --ribbon, one after another",
    )
    p.add_argument(
        "--ribbon",
        default=None,
        help="ribbon to filter on in --batch mode (ymcko|ymckok); "
        "defaults to the ribbon loaded in the printer",
    )
    p.add_argument(
        "--ip",
        required=True,
        help="printer connection string, e.g. socket://192.168.2.101?type=evolis",
    )
    p.add_argument(
        "--reader",
        default=os.environ.get("ESPLP_READER") or DEFAULT_READER_URL,
        help="base URL of the nfc-reader each card's UID is read from "
        "(or set ESPLP_READER)",
    )
    p.add_argument(
        "--uid-wait-ms",
        type=int,
        default=DEFAULT_WAIT_MS,
        help="how long the reader may hold the UID request open waiting for the "
        "card to appear (the firmware caps this at 30000)",
    )
    p.add_argument(
        "--ignore-ribbon",
        action="store_true",
        help="print even when the loaded ribbon does not match the job's ribbon",
    )
    p.add_argument(
        "--dry-run",
        action="store_true",
        help="stop short of the printer: fetch the queue and every raster, but "
        "feed no card, send no job and mark nothing",
    )
    p.add_argument(
        "--no-print",
        action="store_true",
        help="do the whole run except the printing: card fed, UID read, card "
        "marked and ejected blank — nothing reaches the print engine, so no "
        "ribbon is spent",
    )
    return p


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if not args.token:
        args.token = os.environ.get("ESPLP_TOKEN")
    if not args.token and not args.dry_run:
        # print-queue, list-prefixes and mark-printed are admin-only.
        print("error: --token (or ESPLP_TOKEN) is required to print", file=sys.stderr)
        return 2
    if args.batch:
        if args.prefix:
            print("error: --prefix and --batch are mutually exclusive", file=sys.stderr)
            return 2
    elif not args.prefix:
        print("error: --prefix is required (or use --batch)", file=sys.stderr)
        return 2
    if args.dry_run and args.no_print:
        print(
            "error: --dry-run and --no-print are mutually exclusive "
            "(--dry-run already prints nothing; --no-print exists to exercise "
            "the printer)",
            file=sys.stderr,
        )
        return 2

    client = Client(args.server, args.token)
    try:
        if args.batch:
            return run_batch(client, args)
        return print_prefix(client, args, args.prefix)
    except ApiError as e:
        print(f"error: API: {e}", file=sys.stderr)
        return 1
    except PrinterError as e:
        print(f"error: printer: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
