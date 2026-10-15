# TODO

## Image upload validation

`GET /api/prefixes/{prefix}/cards/{id}/image` serves back whatever `Content-Type`
the uploader sent, so it is a stored-XSS vector on the app's own origin. Verified:
an upload with `Content-Type: text/html` is accepted (201), and the endpoint then
returns `content-type: text/html` with no `X-Content-Type-Options: nosniff`.

  1. Restrict the *served* content type to `image/png|jpeg|webp|gif`. The bytes
     can still be stored exactly as uploaded — it is the served type that must
     not be attacker-chosen. (`extension_for` in `server/src/routes.rs` already
     knows the accepted set.)
  2. Send `X-Content-Type-Options: nosniff` on the image response.

Worth doing on its own, independent of the admin UI. Code: `store_card_form`
and `get_card_image`.

## Markdown in the back text

`back_text` is markdown, and it is rendered into a card's back. Markdown is just
another back *source* (`back_source = 'markdown'`): the ribbon decides the colour
depth, and the renderer produces the raster the spooler prints.

  - The SPA renders it live while typing (`web/src/lib/backText.ts`, `marked` plus
    DOMPurify against an allowlist of exactly the subset), so the customer can see
    what they wrote. That pane is the **editing view only**: the print queue
    carries no HTML any more, and the spooler prints the server's raster.
  - The server-side renderer is the only thing that decides what prints, and it is
    still a **stub** — `render::render_back_markdown_stub` reports `failed` with
    its reason, so a markdown-backed card is visibly not printable rather than
    coming out with a blank side. A real renderer (an external markdown→image
    binary such as Markie) lands behind the same signature, which already takes
    the ribbon's `BackDepth`.
  - The subset is small: headings, bold, italic and lists. Links are just text on
    a card, never an `<a>`, so a `javascript:` href is inert; raw HTML is dropped
    whole, tag and text. Images are **refused outright** — the whole construct is
    dropped, alt text included — and the editor says so rather than leaving a gap
    where the artwork was. CommonMark only on both sides: no GFM tables,
    strikethrough or autolinks.

`name` and `serial_number` stay plain text.

## The customer's write window — seal the prefix when it leaves draft

Decided, not built: a customer may edit **only while the prefix is `draft`**. The
moment they click Pay the project is sealed *for them*; from `payment_pending`
onward they get the read-only view, and only the operator can still change cards.

Today one predicate serves both roles, and it is too generous for the customer.
`PrefixStatus::is_writable()` is `draft | payment_pending | paid | queued`, and it
gates every customer write: card PUT/POST/PATCH/DELETE, whether the uploader is
shown, and customer DELETE of the prefix. Its doc comment says "cards may still be
added/changed while the prefix is in these states" — that is the *operator's* rule
leaking into the customer's.

The read-only rendering already exists and works; it is just rarely reached.
`CardRow` takes a `readonly` prop, `Project.svelte` derives it from `writable`, and
when false the page shows no dropzone, disabled inputs and "This project is X and
can no longer be changed" — the smoke test asserts exactly that at `printing`. So
this is close to an authorization change rather than a UI build: it makes a view
that already works appear at the right moment instead of only after printing
starts.

To do:

  1. Split the predicate — `is_customer_writable()` = `draft` only — rather than
     narrowing `is_writable()`. The operator must keep editing in any state,
     including while printing; that is what makes a stuck print job recoverable.
  2. Apply it to every customer-reachable write, not just cards. Prefix DELETE is
     customer-allowed "if writable" too, and so is whatever the uploader does.
  3. The SPA can decide from the status it already fetches, so no new endpoint is
     needed — but make the two definitions agree deliberately, not by coincidence.

Open question, and the part that needs a decision before anything is built: **a
payment that does not complete leaves the prefix sealed but unpaid.** Under the
current table `payment_pending → draft` is legal, but nothing ever triggers it — so
a customer whose card is declined, or who simply closes the Stripe window, can no
longer edit their own project and needs an operator to unseal it. Pick one:
revert to `draft` on Stripe's `checkout.session.expired`; revert when the customer
comes back through `cancel_url`; or let the customer unseal themselves while
unpaid. Without this, the first declined card becomes a support ticket.

## Payments (Stripe) — outstanding

- **Shipping is flat, and region-based shipping is a dead end on this
  integration.** Re-researched once already, so worth writing down: a Shipping
  Rate object has no destination field, and the Checkout docs say outright that
  the hosted page "doesn't support dynamically customizing shipping options".
  Per-country pricing would mean moving to the embedded Checkout form (public
  preview) plus a `calculate-shipping-options` endpoint of our own that Stripe
  calls with the address. Not worth it unless shipping cost actually diverges.
- **The paid amount and the printed cards can diverge.** The session's price is
  fixed from the card count when the checkout opens, but cards stay editable
  while the prefix is `payment_pending` and `paid` — so a customer who adds a card
  in another tab mid-flight pays for the old count and has the new one printed.
  Sealing the prefix once it leaves `draft` (see "The customer's write window")
  closes this for the customer path, since the checkout flips the status before it
  opens the session and a concurrent upload would then be refused. What remains
  after that is operator edits during printing, which are deliberate. Worth
  recording the paid card count regardless, so the mismatch is detectable even
  when it was intentional.
- **Refunds are not wired up.** Cancelling a prefix (admin
  `PATCH .../status` → `cancelled`) does not refund or void the Stripe payment.
  A payment that settles for a cancelled or withdrawn prefix is logged at `WARN`
  and deliberately left alone, because choosing between refund and honour is the
  operator's call — but it still means a human in the Stripe dashboard. An
  explicit refund action, and a `refunded` status to go with it, would close
  this properly.
- **Dead ends in the payment flow are tracked only in the log.** These are exactly
  the cases where money and orders disagree, and nobody reads a log. Known dead
  ends:
    * a payment that settles for a `cancelled` prefix (logged at `WARN`)
    * a payment that settles for a `draft` prefix
    * a payment for a prefix that has since been deleted — the money is real and
      there is nowhere to put it
    * a prefix stuck in `payment_pending` because the customer abandoned the
      checkout: never paid, and invisible to any work queue
    * a payment that settles for a prefix already past printing
  **Done** — a "needs attention" list in the admin UI (`GET
  /api/admin/needs-attention`), driven by a query rather than by scraping the
  log. The webhook writes a `payment_anomalies` row at the moment a settled
  payment cannot attach to a prefix (cancelled, draft, deleted, past printing),
  and the endpoint unions those with a live query for `payment_pending` prefixes
  stuck past the 24h checkout window. Each entry carries an `action` saying what
  to do (refund, honour, rebuild by hand).
- **Email notifications, via Resend.** Decided, and the account exists. There is no
  `RESEND_API_KEY` anywhere in the codebase yet; it goes in as an environment
  variable like the Stripe secrets — never in the repo, never logged.

  **Done** — `server/src/notify.rs` (Resend over plain HTTP, like the Stripe
  client) plus migration `0009_email_notifications.sql`. The customer's
  confirmation and the operator note both go out on the `payment_pending` →
  `paid` transition. Configuration is `RESEND_API_KEY`, `EMAIL_FROM` (a verified
  Resend domain), `OPERATOR_EMAIL` (the operator copy; unset = customer only),
  and `RESEND_API_BASE` for the mock. Both traps are handled: each message is
  claimed in `email_notifications` under `<kind>:<stripe session id>` before it
  is sent, so a retried or concurrent delivery of the same payment cannot send a
  second email; and sending happens after the status write with every failure
  swallowed and logged, so a Resend outage, a bounce or a mistyped key neither
  fails the webhook nor loses the paid order. A failed send keeps its claim with
  the reason recorded, so resending stays an operator decision rather than an
  automatic retry that could double-send. Resend also
  requires a verified sending domain, so the from-address is a setup step under the
  owner's control, not a code detail.

  In order of usefulness: the customer's order confirmation when a prefix becomes
  `paid` — today they get Stripe's receipt, nothing from us, and no link back to
  their project except the browser redirect — and an operator note on the same
  event, so an order can be queued without watching the UI. Later, if anything:
  printed/shipped.

  Two traps to design around:
    * the webhook is the trigger and Stripe retries it, so send exactly once per
      transition, keyed on the Stripe session id, or the customer gets the
      confirmation three times;
    * never let an email failure fail the webhook. A Resend outage, a bounce or a
      mistyped key must not make Stripe retry for days, and must not leave a paid
      order unrecorded — the `paid` transition is the part that matters. Send
      after the status write and outside its error path.
- **Checkout is unthrottled.** Anyone holding a prefix uuid can create checkout
  sessions in a loop. Harmless to us, mildly annoying to Stripe. **Done:**
  `create_checkout` is now throttled to a few sessions per prefix per minute
  (`server/src/rate_limit.rs`, a keyed in-memory fixed-window limiter, 429 on
  excess). The `create_prefix` loop-around (spin up a fresh prefix per checkout)
  is out of scope — the task's threat model was a loop on one prefix.
- **No VAT handling.** Prices are configured VAT-inclusive and nothing computes
  tax. Fine while the Kleinunternehmer thresholds apply; Stripe Tax (0.45 EUR per
  transaction on the API integration) is the drop-in if that changes.

## Print options: one ribbon per job, one back per card

**Done.** Two layers, because they are two different decisions, made by different
people at different moments:

  - the **ribbon** is per *job* — `prefixes.ribbon`, `ymcko` or `ymckoko`. It is
    the one thing that cannot be mixed in a batch, because it is a physical
    ribbon load;
  - the **back** is per *card* — `cards.back_source`, one of `none` (a
    single-sided card), `front` (the front image printed again), `image` (a
    second uploaded picture) or `markdown` (text).

The ribbon bounds the card's choice, because it fixes the colour depth: YMCKO's
back is a colour panel set of its own, so it prints any of the four, while
YMCKOKO's back is the second black panel and can only print `none` or `markdown`.
A picture back on YMCKOKO is refused by the API with `400`, not silently
reprinted as something else. Colour depth is therefore never a third thing to
store — `Ribbon::back_depth()` derives it, and markdown is just another image
source whose depth the ribbon picks.

Availability is a property of the **ribbon**: two stock flags in `ribbon_stock`
(YMCKO, YMCKOKO, both seeded in stock), flipped with the operator token via
`PUT /api/shop/ribbons/{ribbon}`. The shop config serves both flags and each
ribbon's derived `available`. The customer UI keeps an unavailable ribbon in the
list and renders it disabled, so it is visibly *out* rather than gone, and the
checkout refuses it with `409` — the same rule at the layer that takes the money,
because a flag that only greys out a radio button still sells a card that cannot
be printed. A prefix already priced for a ribbon that has since run out is
deliberately **not** reconciled: no reprice, no refund, no notice, it just cannot
open a new session until the ribbon is back — by-hand work if it ever happens.

Pricing is per printed **side** (`ribbon_prices`, one price per ribbon, seeded at
500 cents each): YMCKO bills one side for a single-sided card and two for a
double-sided one; YMCKOKO always bills two, because its second black panel is
spent whether or not the back is printed. The ribbon is snapshotted onto the
prefix (`paid_ribbon`) when the checkout opens, so the spooler prints what was
paid for even if the operator changes it later. `PATCH /api/prefixes/{prefix}`
refuses with `409` a ribbon change that would strand a card — a YMCKOKO prefix
cannot print a colour back.

Rendering is one panel per card (`web/src/lib/CardRow.svelte`), offering the
sources the ribbon allows and the input each one needs: nothing for `none` and
`front`, a file input for `image`, the markdown textarea for `markdown`. The
shared header (name, delete, debounced save) lives in `cards/CardFields.svelte`.
The 1-bit pane for a markdown back is honest about the medium — paper white, pure
black, no greys, no theme colours — and the editor *says* an image will not be
printed rather than leaving a gap where the artwork was. Smoke scenario I covers
all of it.

A card with an `image` back and no picture yet is *incomplete*, which is why
`back_image` is not required at creation: the dropzone uploads fronts one file at
a time, so requiring both sides in one request would have made double-sided cards
impossible to upload into. `PUT .../cards/{id}/back-image` sets the back alone
(and makes that image the card's back), the row and the project page show how many
cards are still missing their material, and the checkout answers `409` — the same
layer that refuses an out-of-stock ribbon, and for the same reason.

A half-panel option was considered and is **dropped — do not re-propose it**. Two
reasons, recorded so the idea does not creep back:

  - The half-panel ribbons that can actually be bought are the *inverse* of what
    this shop needs: half-panel YMC with a full-panel K, optionally plus a
    full-panel K backside. Those exist for ID-card systems where only a photo is
    colour and everything else is black. They cannot produce a full-colour front,
    which is the one thing every card here requires.
  - The panel splits **vertically** (along the width), not horizontally as an
    earlier note here assumed. Moot now, but that is what the error was.

Both of those point the same way: confirm the ribbon SKU is buyable before designing
a card around it. That is exactly how the half-panel option got in here, and it is
the cheapest mistake to avoid.

Rasters: **size to the driver's own internal bitmap**, not to a figure derived from
the card dimensions and dpi. Overscan is deliberate and wanted — printing past the
card outline is what avoids unprinted white edges. Reading the size from the driver
also stays right if the printer or its resolution setting changes.

For reference only, the bare card outline at 600 dpi is 2022 × 1275 px (ID-1,
85.60 × 53.98 mm); colour planes are 8-bit (≈2.5 MiB each) against a 1-bit K panel
(315 KiB). Useful for sizing buffers, not for choosing a canvas.

This is also the input contract for uploaded images: scale-to-fit, and whether that
means letterboxing or centre-cropping follows from the mode's aspect ratio.

## Per-card print status (for the spooler)

Internal use only. Each card needs a `printed` flag — or better, a `printed_at`
timestamp — so the print spooler, to be written later, can walk the cards of a paid
prefix and know where to resume after a fault. Keep it in the database rather than
in the spooler's own state, or a restart reprints a whole batch.

To settle when it is built:

  - Mark a card printed only once the printer confirms it was ejected. Marking at
    queue time silently skips a card if the process dies in between; marking twice
    prints it twice.
  - Order the walk explicitly by card `id`; do not trust the database's natural
    order.
  - It is operator-only state. Keep it out of the customer view entirely, and
    probably out of the admin print preview too — it belongs to the future spooler
    UI.
  - Interaction with the prefix status machine: `printing` is per prefix today, so
    a half-finished order sits in `printing` with some cards marked. That is the
    right place for it, but it means "resume" is really "a prefix in `printing`
    that still has unprinted cards", and whatever shows the print queue has to say
    so.

## Also outstanding

- Rate-limit failed admin logins and the bearer path. **Done** — a per-client
  `LoginLimiter` (5 misses / 15 min → 15 min lockout, in-memory) gates both the
  login endpoint and the bearer path in `auth::resolve`; a locked-out key
  answers 429. Keyed on `X-Forwarded-For`/`X-Real-Ip`.
- Server-side render of the markdown back. **Superseded** — `server/src/backtext.rs`
  (markdown to sanitised HTML for the print queue) is gone: the spooler prints a
  raster, not HTML, so the server renders through `server/src/render.rs` and the
  queue carries no `back_html`. The renderer itself is still the stub described
  under "Markdown in the back text".
- Admin sessions live in memory, so a restart logs the operator out. Fine for
  now; move to a table if that becomes annoying.
