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

Decided: `back_text` is markdown, rendered into the black-only card back. The editor
and the renderer are a solved problem — pick one and do not hand-roll either.

  - Sanitise on render, no raw HTML passthrough, and avoid `{@html}` unless the
    string has been through the sanitiser. `back_text` is the dangerous field:
    free-form, aimed at song lists, and the one a customer is most likely to paste
    into.
  - **Two renderers have to agree**, or the guarantee is worse than no preview: the
    SPA preview the customer approves, and the server-side renderer the print queue
    will need (see "Also outstanding"). Same subset, or the card comes out different
    from what was ordered.
  - The medium is 1-bit black: no colour, no greys. Decide what an image in the
    markdown does — dither, or refuse it outright — and keep the supported subset
    small. Headings, bold, italic and lists earn their keep; links are just text on
    a card, since there is nothing to click.

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
  Deliverable: a "needs attention" list in the admin UI, driven by a query rather
  than by scraping the log, with each entry saying what to do about it (refund,
  honour, rebuild by hand).
- **Email notifications, via Resend.** Decided, and the account exists. There is no
  `RESEND_API_KEY` anywhere in the codebase yet; it goes in as an environment
  variable like the Stripe secrets — never in the repo, never logged. Resend also
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
  sessions in a loop. Harmless to us, mildly annoying to Stripe. The same
  rate-limiting work as the admin path would cover it.
- **No VAT handling.** Prices are configured VAT-inclusive and nothing computes
  tax. Fine while the Kleinunternehmer thresholds apply; Stripe Tax (0.45 EUR per
  transaction on the API integration) is the drop-in if that changes.

## Print options per prefix

Each prefix picks one print mode, and the mode changes everything downstream: the
price, which inputs the customer gets, how both sides are rendered, and which
ribbon the spooler needs. Three modes:

  - **single-sided** — full-colour front, blank back. The budget option.
  - **double-sided colour** — full-colour front and back. Expensive: two YMCKO
    panel sets per card, so it drains stock twice as fast.
  - **colour front, black back** — full-colour front, black-only back. The
    economic two-sided option, and what is implemented today via `back_text`.

Availability is a property of the **ribbon**, not of the mode. Two are in stock:

  - **YMCKO** — full colour, plus a black panel and overlay.
  - **YMCKOKO** — YMCKO plus a second black panel for the back.

and the modes map onto them:

  - single-sided → YMCKO
  - double-sided colour → YMCKO, consuming two panel sets per card
  - colour front, black back → YMCKOKO

So single-sided and double-sided colour go out of stock *together*, while the
black-back mode is independent. Stock is two flags, not three: a mode is gated on
its ribbon, availability is not intrinsic to the mode.

Open question, low priority, deliberately not wired up: on YMCKO, single-sided and
double-sided colour are technically mixable inside one print job, so the mode *could*
be per card instead of per prefix. Only that pair could ever mix — YMCKOKO cannot
share a job with them, which is the constraint that matters — and per-card mode
turns one project price into a per-card sum for little gain. Left at per prefix
unless there turns out to be a reason otherwise.

A half-panel option was considered and is **dropped — do not re-propose it**. Two
reasons, recorded so the idea does not creep back:

  - The half-panel ribbons that can actually be bought are the *inverse* of what
    this shop needs: half-panel YMC with a full-panel K, optionally plus a
    full-panel K backside. Those exist for ID-card systems where only a photo is
    colour and everything else is black. They cannot produce a full-colour front,
    which is the one thing every mode here requires.
  - The panel splits **vertically** (along the width), not horizontally as an
    earlier note here assumed. Moot now, but that is what the error was.

Both of those point the same way: confirm the ribbon SKU is buyable before designing
a mode around it. That is exactly how the half-panel option got in here, and it is
the cheapest mistake to avoid.

What has to change:

  - **Data model.** A `print_mode` on the prefix, defaulting to the black-back mode
    so existing rows keep their meaning. A *colour* back needs a back image, which
    does not exist today — a card carries exactly one image plus `back_text`. So
    either a second image column or an images table keyed by side. The black back
    stays text-only (`back_text`, markdown), so that mode needs no image path at
    all. `back_text` means nothing in the other two modes and should be rejected or
    ignored rather than silently stored.
  - **Pricing in the database, not the environment.** One `STRIPE_PER_CARD_CENTS`
    prices every card today. This needs a per-mode price (with currency) that the
    shop config serves and the checkout sums over the cards' modes — so the mode
    has to be frozen for the customer exactly when the cards are (see "The
    customer's write window"), and the spooler must read the mode that was *paid
    for*, not the current one.
  - **Availability, per ribbon, visible rather than hidden.** Two stock flags
    (YMCKO, YMCKOKO); a mode is offered while its ribbon is in stock, so
    single-sided and double-sided colour disappear together. Render the unavailable
    option disabled, not absent — the customer should see that double-sided exists
    and is out. Decide what happens to a prefix already priced for a mode whose
    ribbon ran out: the checkout should refuse it, not sell a card that cannot be
    printed.
  - **Rendering.** One Svelte component per mode, and the inputs differ per mode:
    no back field at all for single-sided, an upload instead of text for a colour
    back. The black-back preview has to show honestly what black-only does to the
    artwork, since that is the entire difference between it and the colour mode.

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

- Rate-limit failed admin logins and the bearer path.
- Server-side render of the black-only card back (client-side preview only so
  far) — needed by the print queue, and per print mode.
- Admin sessions live in memory, so a restart logs the operator out. Fine for
  now; move to a table if that becomes annoying.
