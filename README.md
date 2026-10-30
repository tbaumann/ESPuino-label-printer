# ESPuino Label Printer

Print-on-demand service that prints RFID/NFC cards for ESPuino, phoniebox,
TONUino, SonUINO and similar RFID-controlled music players.

A Cargo workspace with two crates:

| Crate                 | Path      | What it is                                            |
|-----------------------|-----------|-------------------------------------------------------|
| `card-printer-server` | `server/` | REST API (Axum + utoipa OpenAPI + SQLite/sqlx + blobs) |
| `card-printer-cli`    | `cli/`    | `card-printer-cli` command-line client                           |
| — (Svelte SPA)          | `web/`    | browser UI, served by the server process              |

Stack: **Axum** (HTTP) + **utoipa** (automatic OpenAPI 3.0 + Swagger UI) +
**SQLite via sqlx** (metadata) + **filesystem blobs** (card images). Packaged
with a **Nix flake** (rust-flake / crane).

## Run (development)

```sh
nix develop -c cargo run -p card-printer-server   # server
```

Then open http://127.0.0.1:8080/swagger-ui — the live, generated API docs. They
describe the **public surface**: every operation a caller can reach without the
operator's credential. http://127.0.0.1:8080/swagger-ui-full describes everything
the router serves, admin and shop included; the two specs are
`/api-docs/openapi.json` and `/api-docs/openapi-full.json`.

Which half an operation lands in is declared in its own
`#[utoipa::path(...)]`: naming a credential (`security((bearerAuth…))`) or
carrying the `admin` tag keeps it out of the public docs. The filter is in
`server/src/main.rs` (`restrict_to_public_surface`).

## Run (Nix)

```sh
nix run .#card-printer-server            # server, serving the web UI
nix run .#card-printer-cli -- --help     # CLI
nix build .#                             # default = the spooler (card-printer)
nix build .#card-printer-cli
nix build .#oci-card-printer-server      # the server as a nix-oci image (see "OCI image" below)
nix run .#markie -- --help               # markdown→image renderer, behind a card's markdown back
```

The SPA is built from `web/` **in the Nix store** by `buildNpmPackage` and baked
into the `card-printer-server` package: the crate binary is wrapped so `STATIC_DIR`
points straight at that store path. Nothing is read from `server/static`, and the
running server depends on nothing in the working tree:

```sh
nix run /path/to/checkout#card-printer-server   # web UI works
```

`STATIC_DIR` is applied with `--set-default`, so exporting your own value still
overrides the built-in store path.

`nix build` needs the flake tree git-tracked (Nix only sees tracked files once
the directory is a git repo), and `Cargo.lock` must be committed — not just
present — or crane refuses to vendor:

```sh
git init && git add -A && git commit -m init
nix build .#
```

That also matters for speed: in a *non-git* checkout Nix copies the entire
directory into the store for evaluation, `web/node_modules` included, which
makes every `nix build` / `nix run` slow. Once the tree is tracked (and
`web/node_modules` is ignored) only the source is copied.

## OCI image

`nix build .#oci-card-printer-server` builds the server image with
[nix-oci](https://github.com/Dauliac/nix-oci) (nix2container), wired in `oci.nix`:

```sh
nix build .#oci-card-printer-server            # image; result/bin/ has push, load-docker, load-podman, sandbox
nix run .#oci-load-podman-card-printer-server  # load into podman/docker
nix run .#oci-push-card-printer-server         # skopeo push to ghcr.io/tbaumann/card-printer-server
```

`oci.nix` sets the runtime shape: entrypoint `card-printer-server`, `DATA_DIR=/data`
as a declared volume, `BIND_ADDR=0.0.0.0:8080`, and `SSL_CERT_FILE` from `cacert`
(the server verifies TLS against Stripe). The container runs as the non-root
`card-printer` user (UID/GID 4000) and nothing in the image is writable by it, so
`/data` must be a writable mount: `--tmpfs /data:rw,mode=1777`, or a volume owned by
4000:4000.

## CLI (`card-printer-cli`)

```sh
card-printer-cli --server http://127.0.0.1:8080 <command>     # or export ESPLP_SERVER=...
card-printer-cli --token <admin-token> prefix list           # or export ESPLP_TOKEN=...
```

Commands (every API field is a flag):

| Command | Purpose |
|---------|---------|
| `prefix create [--uuid U] [--ribbon R]` | create a prefix (server assigns uuid if omitted) |
| `prefix list [--status S]` / `prefix get <P>` | list prefixes (optionally filtered by status) / show one + card count |
| `prefix set-status <P> <STATUS> [--order-ref R]` | set a prefix's status (any transition) |
| `card list <P>` | index of cards in a prefix |
| `card get <P> <ID>` / `card delete <P> <ID>` | card metadata / delete |
| `card back-image <P> <ID> <FILE>` | upload/replace a card's back image, making that image its back |
| `upload <P> <FILE> [--id N] [--name N] [--serial-number S] [--back B] [--back-text T]` | upload one card |
| `batch <P> <DIR> [--recursive] [--start-id N] [--name-template T] [--serial-prefix P] [--serial-width W] [--back B] [--back-text T] [--dry-run]` | upload a folder |

Notes:

- `upload` defaults `--name` to the file's stem. With `--id` it PUTs (create or
  replace at that index); without it the server assigns the next id (POST).
- `batch` collects image files (`png jpg jpeg webp gif`), sorted for a
  deterministic order, and skips everything else. `--name-template` supports
  `{stem}`, `{filename}`, `{index}`. `--serial-prefix` produces
  `SN-001`, `SN-002`, ... (zero-padded to `--serial-width`). `--dry-run` prints
  the plan without uploading.
- `--quiet` prints just the uuid (prefix commands) or id (card commands) — handy
  for scripting: `P=$(card-printer-cli prefix create --quiet)`.
- `prefix list` and `prefix set-status` are admin-only, so they need the
  operator token: `--token <tok>` or `ESPLP_TOKEN`. Every other command is
  customer-level and needs no token. Passing the token anyway is harmless — the
  server ignores it on those routes, so one exported `ESPLP_TOKEN` is fine for
  everything.

## Web UI (`web/`)

A Svelte 5 + Vite + TypeScript SPA, served by the Rust process at `/` — same
origin as the API, no second service, no SSR.

For local work it builds into `server/static/`. Under Nix it is built into the
store instead and the binary is pointed at that path, so `nix build` output is
self-contained (see "Run (Nix)" above).

```sh
npm --prefix web install
npm --prefix web run build      # writes server/static; then just run the server
npm --prefix web run dev        # dev server, /api proxied to 127.0.0.1:8080
npm --prefix web run check      # svelte-check + tsc
npm --prefix web test           # jsdom smoke test; see below for the server it needs
```

The smoke test mounts the built bundle in jsdom and drives it against a live
server, so it needs a build in `server/static` plus a server on `:8099` started
with an `ADMIN_TOKEN` — the same token must be in the test's environment:

```sh
DATA_DIR=/tmp/esplp-smoke ADMIN_TOKEN=secret BIND_ADDR=127.0.0.1:8099 \
  PRINT_RASTER_WIDTH=508 PRINT_RASTER_HEIGHT=324 ./target/debug/card-printer-server &
ADMIN_TOKEN=secret npm --prefix web test
```

Run that from `nix develop`: the markdown scenarios render a back through `markie`,
which the dev shell puts on `PATH` (`MARKIE_BIN` points somewhere else if set).

A *debug* server re-renders the print rasters synchronously on every card write,
and an unoptimised Lanczos resize to the 2032x1296 default takes seconds — hence
the small raster size above. It is server config either way, and a release build
does not need it.

Its last scenario (the Pay button) needs payment configured on that server and is
skipped with a note when it is not; `scripts/stripe_e2e_test.py` is the test that
covers payment properly.

Opening `/` creates a new project and redirects to `/p/<uuid>` — the project
*is* the prefix. Use "Open" to return to an existing project code; recent
projects are remembered in `localStorage`. A project page holds the ribbon
chooser (one radio per ribbon, with the price of one billing unit — a printed
side or a whole card, as the server names it; a ribbon that
is out of stock is shown greyed out and disabled, never removed), the drop zone
(drag images, or a whole folder) and one row per card.

### One card panel, and the per-card back choice

The card chooses what its back *is*; the ribbon chooses nothing about that — it
decides only the colour depth the back is rendered in, and therefore what the
spooler is handed. `CardRow.svelte` renders each card — the front, the back the
chosen source produces, and the editor beside them — and puts the two controls in
two places on purpose: which back the card *has* is chosen in the editor (one
button per source, all four on either ribbon, with what that source needs under
it), while the **file chooser for an `image` back sits under the back pane**,
beside its caption — the pane is the picture it chooses, and stays unobstructed.
`cards/CardFields.svelte` holds the shared header (name, delete and the save
state).

| `back` | Printed on the back | The pane shows | The editor offers |
|--------|---------------------|----------------|-------------------|
| `none` | nothing — a single-sided card | no back pane at all | nothing to supply |
| `front` | the front image again | the server's render of it | nothing to supply |
| `image` | a second uploaded picture | the server's render, or the drop prompt | the file chooser, under the pane |
| `markdown` | text, rendered to an image | the server's render | the markdown editor, in a column of its own |

A markdown back is edited **as the formatting it will have**, in a Tiptap editor
(`cards/MarkdownEditor.svelte`) whose value in and out is the markdown itself:
the toolbar applies the constructs, and what the customer sees is what the
backend is handed. The editor's schema carries everything a back can hold — the
starter set plus pictures, tables and task lists — so editing a card cannot drop
a construct the backend would have printed. The pane beside it is still the
server's render and nothing else. The editor is also where the layout lives: it
sits in its own column beside the back-source choice rather than under it.

The two text fields — a card's name and its back text — are written **when the
customer stops writing or leaves the field** (a 1200 ms idle timer, plus `blur`),
never per keystroke: every write of a card re-renders its rasters. A response is
never copied back into the field it came from (the server stores what it was
sent), which is what keeps the text that was typed while a write was in flight.

Every back has exactly one picture in the UI, and it is the server's:
`display/back`, rendered at the ribbon's colour depth. The row never draws a back
itself, so the pane shows what comes out of the printer — the same picture
reduced to black and white on a black-back ribbon, and nothing to show at all
where the server could not render the back. Switching the project's ribbon
therefore repaints the back previews, and nothing else about the card changes.

While an `image` pane holds no picture yet, the **whole pane is a file drop
area**: a picture dropped anywhere on it becomes that card's back, and clicking
the empty area opens the picker. A pane already showing artwork still takes a drop
— a deliberate replace — but no longer opens a file dialog on a click, since the
chooser under it is right there.

Everything but `none` makes the card double-sided. All four sources are on offer
on either ribbon: the ribbon is not a permission, only a rendering choice. A
picture on YMCKOK's second black panel comes out black and white rather than
being refused — a poor choice, and the customer's to make, which the editor says
where the choice is made.

Black and white is not one operation but two, and which one a back gets follows
from what the back *is* (`render::Picture`):

- A **photograph** — the front, a second uploaded picture, a copy of the front —
  is **dithered** (Floyd–Steinberg error diffusion). A black panel has no grey,
  so every mid-tone has to be spent somewhere: diffused into the neighbouring
  pixels it prints as a locally correct density of dots, and the picture
  survives as texture. Cut at mid-grey instead, everything darker than 50%
  prints as a flat black shape and everything lighter as flat white: a ramp
  keeps *none* of its 256 steps, and the test card (a flat mid-grey) prints as a
  solid black rectangle.
- A **document** — a markdown render — is **cut** at mid-grey. Its ink is line
  art: the edge *is* the image, and stippling a glyph's anti-aliased edge trades
  a crisp letter for one at the mercy of the printer's dot gain. (Measured at
  600 dpi, a dither would move only ~2% of a text back's pixels and make no
  isolated dots — so this is a choice about the edge, not a cliff: a cut also
  fattens anti-aliased glyphs by ~28% in ink, printing them a little bolder than
  they were laid out.)

Both are the same code path and the same artifacts — the preview pane is
dithered or cut exactly as the print is, so a black-panel back previews as the
speckle it will print as.

The drop zone uploads **fronts** — one file, one card — so a new card is
single-sided, and one with an `image` back starts without its picture. The page
badge says how many cards are still missing their back material, and the Pay
button stays disabled while any are: the checkout refuses such an order with
`409` as well, because a card with one side missing cannot be printed.
Completing it needs no new state — the badge and the button follow the write.

A markdown back is no exception: the server renders the text with the `markie`
backend the flake packages (`MARKIE_BIN`, see "Markdown in the back text" in
TODO.md), then places that picture on the printer's bitmap and reduces it to the
ribbon's depth — the same path an uploaded picture takes, differing only in
*where the picture lands*. A rendered document is anchored at the panel's
top-left, at the width the card was laid out for, and whatever runs past the
panel is clipped: a long song list keeps the text size the layout gave it and is
cut at the card's edge rather than shrunk to fit. An uploaded picture is instead
scaled to fit inside the panel and centred, so none of it goes missing.
`render::Placement` is that choice, named at the call site rather than defaulted.
Either way the pane shows what would print, never a picture of its own, and a
markdown back whose render fails reports the card not printable with the reason
the backend gave. The print queue carries no rendering either way.

The panes load the **server-derived display images** — `display/front` for the
front, `display/back` for every back whatever its source — so the aspect, the
normalization and the colour depth are all the server's. Nothing in the SPA draws
a card, so no preview can be the source of truth for print.

A derived artifact's *path* never changes while its *bytes* can (a re-upload, a
back-source change, a ribbon switch that moves the back between colour and
1-bit), so it is served with an `ETag` and `Cache-Control: no-cache`: a stale
copy is revalidated — `304` when it is still current — rather than reused blind.
The SPA additionally versions the URL it puts in an `<img>` (off the card's
`updated_at`, plus the ribbon for a back), because a browser given the same URL
twice will not ask again at all: that version is what repaints a back pane after
a ribbon switch. The print rasters carry the same validators; the spooler
fetches them fresh and ignores both.

### Admin UI (`/admin`)

The same SPA on an operator route. `/admin` asks for the token, trades it for
the session cookie, then lists every prefix with its status, card count and
order reference. Each row takes a new status, and there is a delete button;
`/admin/p/<uuid>` shows that prefix's cards as a read-only **print preview** —
front image and back panel, with no editor, no uploader and nothing that writes
to a card.

Above the prefix list sits the shop itself: one row per ribbon with its price in
the shop's currency and its stock checkbox. The price is written for the
ribbon's own unit — a printed side, or a whole card — and the row says which, so
the operator never has to know the rule to read the number. Both are database
state (`ribbon_prices`, `ribbon_stock`) with their own endpoints
(`PUT /api/shop/ribbons/{ribbon}/price`, `PUT /api/shop/ribbons/{ribbon}`), so
the shop can be repriced or a ribbon pulled without a redeploy. A price change
applies to checkouts opened after it, never to one already open; a ribbon taken
out of stock keeps its price and stays visible-but-disabled in the customer
chooser, and only stops *new* checkouts.

The admin views are just two more routes under `web/src/routes/`; there is no
separate bundle. Since the session cookie is `HttpOnly`, the UI cannot read it,
so every mutation goes through the shared `req()` helper in `web/src/lib/api.ts`,
which attaches the `x-esplp-csrf` header automatically.

## Payments (Stripe)

Checkout goes through **Stripe Checkout**, which collects the shipping address
and the money. There is no shop system and no second service: the server does
exactly two things — open a session, and listen for the webhook that says the
money arrived.

1. the customer clicks Pay on the project page. There is no submit step: paying
   is what makes an order real, and nothing is printed until it is paid;
2. `POST /api/prefixes/{prefix}/checkout` moves the prefix to `payment_pending`,
   creates a Checkout Session, and returns its `url`; the browser is sent there;
3. Stripe takes the address and the payment, then redirects back to
   `/p/<uuid>?paid=1` **and** calls `POST /api/webhooks/stripe`;
4. the webhook is the only thing that marks an order paid. It verifies the
   `Stripe-Signature` header, reads the prefix uuid out of `client_reference_id`,
   moves `payment_pending → paid`, stores the Stripe session id as `order_ref`
   and the shipping address Checkout collected on the prefix. That transition is
   also what sends the order emails — see "Email notifications (Resend)" below.

The address is stored with the order, so shipping it never means reading the
Stripe dashboard. It is served by `GET /api/prefixes/{prefix}/shipping`
**(admin)** — an address is the customer's, and the prefix uuid is the only
credential the customer's own view holds, so it is deliberately not a field of
the public prefix JSON. Both `shipping_details` and
`collected_information.shipping_details` are read: which one Stripe sends is the
API version the **webhook endpoint** is configured for, not something this code
sets, and an endpoint version bump must not silently stop storing addresses. An
address-less event leaves a stored one alone.

**The redirect is not proof of payment.** It is a separate event from the webhook
and can arrive first — or be typed in by hand. So the project page never trusts
`?paid=1`: it shows "waiting for the payment to be confirmed" and polls the
prefix until the status changes. Only the signed webhook writes `paid`.

Payment methods are not chosen in code. Stripe removed the
`payment_method_types` parameter from Checkout Session creation: the methods you
accept are configured in the Stripe Dashboard (Settings → Payment methods), and
Stripe decides which of them to show each customer, and in what order. Enable
PayPal, Klarna and SEPA direct debit there — they are worth having for a German
shop, and none of them needs a code change. Leaving that section empty means
card-only. (The old `STRIPE_PAYMENT_METHODS` variable is now inert, and logs a
warning if you still have it set.)

A session can still narrow that list, or be pinned to one:

```sh
STRIPE_EXCLUDED_PAYMENT_METHODS=klarna          # never offer these, whatever the Dashboard says
STRIPE_PAYMENT_METHOD_CONFIGURATION=pmc_1234    # pin sessions to a Dashboard pmc_… configuration
```

Shipping is one flat charge (`STRIPE_SHIPPING_CENTS`) for every destination, and
that is a Stripe limitation rather than a shortcut. A Shipping Rate has no
destination field — `active`, `created`, `delivery_estimate`, `display_name`,
`fixed_amount`, `livemode`, `metadata`, `tax_behavior`, `tax_code`, `type`, and
nothing else — so a rate cannot vary by country. Region-based shipping exists
only in the embedded Checkout form and Elements integrations, where Stripe calls
an endpoint of yours with the address and you return the price; hosted Checkout
has no such hook. Note that `STRIPE_COUNTRIES` is not a pricing knob: it restricts
which addresses are accepted at all, and the same shipping line is charged
whatever is entered.

Delayed methods are handled properly: Klarna and SEPA report
`checkout.session.completed` while still unpaid, and only settle with a later
`checkout.session.async_payment_succeeded`. The server waits for
`payment_status == "paid"` either way, so credit is never given early.

A payment that lands on a prefix which is neither `payment_pending` nor `paid` — a
draft, or a cancelled order — is deliberately **not** forced through. It is logged
at `WARN` and left for the operator to refund or honour, because guessing either
way is worse than asking. That refusal is also why the checkout moves the status
*before* opening the session: a customer can pay the moment they land on Stripe's
page, and a slow status write would otherwise strand a real payment on that WARN
line.

### Setting it up

1. create the Stripe account and stay in **test mode** to begin with;
2. copy the secret key (Developers → API keys) into `STRIPE_SECRET_KEY`;
3. add a webhook endpoint (Developers → Webhooks) at
   `https://<your-host>/api/webhooks/stripe`, subscribed to
   `checkout.session.completed` and `checkout.session.async_payment_succeeded`,
   and copy its signing secret into `STRIPE_WEBHOOK_SECRET`;
4. set `PUBLIC_BASE_URL` to the public origin, e.g. `https://cards.example.com`
   — that is where Stripe sends the customer back;
5. set the prices: `ribbon_prices` holds one price per ribbon (seeded at 500
   cents each), alongside `STRIPE_SHIPPING_CENTS` and `STRIPE_CURRENCY`. The
   unit differs per ribbon — a printed side on YMCKO (a card costs one or two of
   them), a whole card on YMCKOK — and `Ribbon::billing_unit` is what says so.
   Taking a ribbon out of stock is the same kind of database state
   (`ribbon_stock`, both ribbons seeded in stock), flipped through
   `PUT /api/shop/ribbons/{ribbon}` with the operator token;
6. only then switch to live keys.

During local development the Stripe CLI replaces steps 2–3:

```sh
stripe listen --forward-to 127.0.0.1:8080/api/webhooks/stripe   # prints a whsec_…
```

`STRIPE_SECRET_KEY` unset switches payments off: `/api/shop/config` reports
`enabled: false`, the customer UI shows no way to pay, and both payment endpoints
answer `503`. Same convention as `ADMIN_TOKEN` — unset is never open. Unset
`STRIPE_WEBHOOK_SECRET` is the one combination to avoid: checkout still works,
but every webhook is rejected and no order ever becomes `paid`.

### Testing payments without a Stripe account

`scripts/mock_stripe.py` stands in for `api.stripe.com` and records the form body
it receives. `scripts/stripe_e2e_test.py` drives the real API against it — it
creates a prefix, uploads cards, asks for a checkout session, asserts on exactly
what was sent, then delivers webhook events to the live endpoint, signed the way
Stripe signs them:

```sh
python3 scripts/mock_stripe.py 12111 &
python3 scripts/mock_resend.py 12112 &
DATA_DIR=/tmp/esplp-stripe STRIPE_SECRET_KEY=sk_test_x \
  ADMIN_TOKEN=secret \
  STRIPE_WEBHOOK_SECRET=whsec_test_secret STRIPE_API_BASE=http://127.0.0.1:12111 \
  PUBLIC_BASE_URL=http://127.0.0.1:8080 \
  STRIPE_SHIPPING_CENTS=250 \
  RESEND_API_KEY=re_test_x RESEND_API_BASE=http://127.0.0.1:12112 \
  EMAIL_FROM='ESPuino <orders@example.test>' OPERATOR_EMAIL=shop@example.test \
  ./target/debug/card-printer-server &
python3 scripts/stripe_e2e_test.py http://127.0.0.1:8080
```

## Email notifications (Resend)

A paid order sends two emails: the **customer's order confirmation**, and a
**note to the operator** so an order can be queued without watching the UI.
Those are the only two so far — `printed`/`shipped` are the obvious next ones,
and `server/src/notify.rs` is where they would join.

Both are triggered by the Stripe webhook, which shapes the design in two ways:

  - **exactly once per transition.** Stripe retries a delivery until it is
    answered 2xx, so a naive send mails the customer three times for one
    payment. Each message is *claimed* in `email_notifications` under
    `<kind>:<stripe session id>` before it is sent, and a duplicate or
    concurrent delivery loses the claim and sends nothing — so a replayed event
    is free;
  - **never the webhook's failure.** Sending happens after the `paid` status
    write and every failure is swallowed and logged: a Resend outage, a bounce
    or a mistyped key cannot make Stripe retry for days, and cannot leave a paid
    order unrecorded. A failed message keeps its claim, with the reason
    recorded — resending is an operator decision, not an automatic retry.

`RESEND_API_KEY` unset, or no `EMAIL_FROM`, switches sending off, with a warning
saying which: an unmailable shop is the failure mode hardest to notice. The
confirmation goes to the address on the Stripe session (`customer_details.email`,
which is what Checkout collects) and links back to `{PUBLIC_BASE_URL}/p/<uuid>`
— the thing Stripe's own receipt does not provide. The operator note carries the
card count, the ribbon, the amount and the customer's contact details.

### Setting it up

1. verify a sending domain in Resend — it refuses any other from-address;
2. put the API key in `RESEND_API_KEY`: environment only, never in the repo;
3. set `EMAIL_FROM` to an address on that domain, e.g.
   `ESPuino <orders@example.com>`;
4. set `OPERATOR_EMAIL` for the operator copy; leave it unset and only the
   customer is mailed;
5. make sure `PUBLIC_BASE_URL` is right, or the link in the email goes nowhere.

`RESEND_API_BASE` points the client at a mock: `scripts/mock_resend.py` stands in
for `api.resend.com` and records every message it is asked to send.
`scripts/stripe_e2e_test.py` covers the whole path (its section 5b), including
that a *replayed* webhook delivery sends no second email.

## Configuration (server environment)

| Variable       | Default                        | Meaning                              |
|----------------|--------------------------------|--------------------------------------|
| `BIND_ADDR`    | `127.0.0.1:8080`               | listen address                       |
| `DATA_DIR`     | `data`                         | root for the DB and image blobs      |
| `DATABASE_URL` | `sqlite://$DATA_DIR/cards.db`  | sqlx SQLite URL                      |
| `RUST_LOG`     | `info,tower_http=info`         | tracing filter                       |
| `STATIC_DIR`   | `server/static`                | built SPA to serve at `/`            |
| `PRINT_RASTER_WIDTH` / `PRINT_RASTER_HEIGHT` | `2032` / `1296` | the printer's internal bitmap, in pixels — read once at startup, and what every print raster and back preview is rendered at (600-dpi Primacy default; overscan past the card outline is deliberate) |
| `MARKIE_BIN`   | `markie` (from `PATH`)         | the markdown→image backend a `markdown` back is rendered with; the package points it at its own store path |
| `ADMIN_TOKEN`  | *(unset)*                      | operator bearer token; unset switches the admin API off |
| `SESSION_COOKIE_SECURE` | *(unset)*             | force the `Secure` flag on the admin session cookie; unset = infer from `X-Forwarded-Proto` |
| `STRIPE_SECRET_KEY` | *(unset)*                 | Stripe API key; unset switches checkout and the webhook off (`503`) |
| `STRIPE_WEBHOOK_SECRET` | *(unset)*             | signing secret for the webhook; without it every webhook is rejected |
| `STRIPE_API_BASE` | `https://api.stripe.com`     | API base; override to point at a mock |
| `PUBLIC_BASE_URL` | *(unset)*                   | public origin for the post-payment redirect; unset falls back to the request `Host` |
| `STRIPE_CURRENCY` | `eur`                       | ISO 4217 code |
| `STRIPE_SHIPPING_CENTS` | `0`                   | flat shipping charge; `0` adds no shipping line |
| `STRIPE_COUNTRIES` | `DE,AT,CH`                 | allowed shipping-address countries |
| `STRIPE_EXCLUDED_PAYMENT_METHODS` | *(none)*       | methods to never offer in a session |
| `STRIPE_PAYMENT_METHOD_CONFIGURATION` | *(unset)*  | pin sessions to a Dashboard `pmc_…` configuration |
| `RESEND_API_KEY` | *(unset)*                   | Resend API key; unset (or no `EMAIL_FROM`) switches order emails off |
| `EMAIL_FROM`   | *(unset)*                      | sending address, on a domain verified in Resend, e.g. `ESPuino <orders@example.com>` |
| `OPERATOR_EMAIL` | *(unset)*                    | where the operator's copy of a paid order goes; unset = the customer only |
| `RESEND_API_BASE` | `https://api.resend.com`    | API base; override to point at a mock |

Layout on disk:

```
data/
  cards.db                   # prefix + card metadata (the single index)
  blobs/<prefix>/<id>.<ext>       # card front images
  blobs/<prefix>/<id>-back.<ext>  # back images (an `image` back only)
  tmp/                        # scratch for the markdown backend: one file, deleted after each render
```

## Data model

- **Prefix** — a UUID that groups one customer's cards (one order/batch). The
  customer picks a UUID or the server assigns one. `card_count` is derived.
  Carries a `status` (see lifecycle below), `status_changed_at`, an optional
  `order_ref` (external shop id), and a `ribbon` that fixes the ribbon load and
  therefore what a card's back side may be.
- **Shipping address** — collected by Stripe Checkout and stored on the prefix by
  the payment webhook, so shipping an order does not mean reading the Stripe
  dashboard. Served only by `GET /api/prefixes/{prefix}/shipping` **(admin)**;
  never part of the public prefix JSON, whose uuid is the customer's only
  credential. Empty until the payment settles, and on a checkout that collected
  no address.
- **Ribbons** — `ymcko` (colour front and a full-colour back; the **default**)
  and `ymckok` (colour front, black-and-white back). The one choice per
  *job*, because a batch cannot mix ribbon loads. Set at prefix creation or via
  `PATCH /api/prefixes/{prefix}` while the prefix is still writable. The switch
  is always lossless: no card is tied to the ribbon except in how it is
  rendered, so the route re-renders the prefix's derived artifacts from the
  stored originals and nothing else changes. It re-renders *every* back side in
  the new depth — colour to black and back — and the print queue reports the new
  ribbon on the next read, because there is one ribbon and this is it. Paying
  does not pin it: the charge is taken and recorded when the checkout opens, and
  which load goes in the printer afterwards is a production decision, not a
  repricing.
- **Pricing** — one price per ribbon in the database (`ribbon_prices`, seeded at
  500 cents each), not the environment, and the *unit* that price is for differs
  per ribbon (`unit` in the shop config, `Ribbon::billing_unit`): YMCKO prices a
  printed *side* (one for a single-sided card, two for a double-sided one),
  YMCKOK prices a whole *card* whatever its back is, because its panel set is
  spent whether or not the second black panel is used. `Ribbon::billed_units`
  applies the rule at the checkout. The charge is settled there, when the
  checkout session opens; what a job is later produced on is the prefix's
  `ribbon` and nothing else, so an operator changing it is making a production
  decision, not a repricing.
- **Ribbon availability** — two stock flags in the database (`ribbon_stock`,
  `ymcko` and `ymckok`, both seeded in stock), flipped through
  `PUT /api/shop/ribbons/{ribbon}`. The shop config serves both flags and, per
  ribbon, the derived `available`; the customer UI renders an unavailable ribbon
  **disabled rather than absent**, and the checkout refuses it with `409` — a
  stock flag that only greys out a radio button would still sell a card that
  cannot be printed. Nothing is reconciled for a prefix already priced for a
  ribbon that has since run out.
- **Card** — `id` (1-based index *within its prefix*), `name`, optional
  `serial_number`, a front image (ISO/IEC 7810 / CR80, scaled to fit), and a
  `back`: `none` (a single-sided card), `front` (the front image again), `image`
  (an uploaded picture) or `markdown` (rendered from `back_text`). Anything but
  `none` is a double-sided card, and the choices are bounded by the prefix's
  ribbon — a back it cannot print is rejected by the API, never silently stored.
  The back image is uploaded per card **after** the front (the dropzone is a
  bulk flow), so a card can exist while its back is still missing — it is just
  not *complete*, and an order holding one is refused at the checkout (`409`).
- **`serial_number` is the card's identity**: the physical RFID UID the print
  process read off the card's chip — four UID bytes as `%03d` each, so exactly
  12 decimal digits, often with a leading zero, and never hex — or an
  operator-assigned serial on a card no reader has seen. It is written by the
  print process with the print mark (`POST .../cards/{id}/printed`), never by the
  customer UI, and it is unique per prefix: a UID is one physical chip, so a
  second card claiming it answers `409` naming the card that has it. NULL means
  "not known yet", and any number of cards may be in that state.

## Prefix lifecycle

```
draft ──Pay──▶ payment_pending ──paid──▶ paid ──▶ queued ──▶ printing ──▶ printed
  └─────────────────── cancelled (reachable from any pre-printing state) ───────────────────┘
```

- The customer's Pay button opens a checkout (`POST .../checkout`), which moves a
  draft to `payment_pending`; Stripe's webhook marks it paid; the print queue
  advances `queued → printing → printed`. There is no submit step, and
  `POST .../submit` no longer exists — it answers `410`.
- A `payment_pending` prefix can be paid for again: a closed window or a declined
  card is not a dead end, and nothing has to be reset by hand.
- A customer may edit cards only while the prefix is `draft`; the moment they
  click Pay the project is sealed for them, and from `payment_pending` onward
  they get the read-only view. The operator may still edit while the prefix is
  `draft`, `payment_pending`, `paid` or `queued`; once it reaches `printing`
  even the operator's card writes (PUT/POST/DELETE) return `409`. Reads always
  work. The one write that still applies there is the print spooler's mark —
  `POST .../cards/{id}/printed` — which is also where a card's RFID UID arrives
  (`{"serial_number": "123456789012"}`, optional, like the whole body):
  the write that records the ejection is the only one left at the moment the
  chip has been read.
- `GET /api/prefixes?status=paid` is the print queue's work list.
- Transitions are validated; an invalid one returns `409`. Re-applying the
  current status is a no-op, so shop/queue callbacks can safely retry.
- **Entering `queued` clears every card's `printed_at`, in the same transaction
  as the status write** — `queued` means "a print run waiting to happen", so it
  is the operator's reprint lever: a half-printed job, or one already printed
  that has to be printed again, starts from card 1 instead of resuming. No other
  status touches print state: `printing` is where the spooler resumes from, and
  re-applying `queued` when the prefix is already queued is not a transition, so
  it clears nothing (a retried callback must not spend a ribbon).
- There are no accounts: a customer's credential is the prefix uuid they know,
  and the operator additionally presents a bearer token. See "Who may do what".

## Who may do what

There are no accounts. Two kinds of caller:

- **Customer** — anyone holding a prefix uuid. That uuid *is* the credential,
  and it cannot be discovered: listing is admin-only, so there is no
  enumeration route. A customer can read their own prefix and its cards, edit
  them and delete the prefix while it is still a `draft`, and pay for it. Once
  it leaves `draft` the project is sealed for them.
- **Operator** — either sends `Authorization: Bearer $ADMIN_TOKEN` (the CLI), or
  logs in through the admin UI, which trades the token for a session cookie.
  Can list every prefix, set any status at any time, delete a prefix in any
  state, and write cards even while one is printing.

| Operation                                  | Customer      | Operator |
|--------------------------------------------|---------------|----------|
| list prefixes                              | ✗             | ✓        |
| create a prefix                            | ✓             | ✓        |
| read a prefix / its cards / an image       | ✓             | ✓        |
| add, edit or delete cards                  | ✓ if writable | ✓ always |
| pay (`draft → payment_pending`)            | ✓             | ✓        |
| set status (`PATCH .../status`)            | ✗             | ✓ (any transition) |
| delete a prefix                            | ✓ if writable | ✓ always |

For a **customer**, "writable" means `draft` only: the moment they click Pay
the project is sealed for them, and from `payment_pending` onward they get the
read-only view. For the **operator**, "writable" means `draft`, `payment_pending`,
`paid` or `queued`; from `printing` onward even the operator's card writes are
frozen, though they can still set any status — which is what makes a stuck
print job recoverable, since a customer has no legal edge out of `printing`.

When `ADMIN_TOKEN` is unset the admin API is switched off: admin-only calls
answer `503` with an explanatory body while everything customer-facing keeps
working. An unset token is never an open door.

The token is compared in constant time. Failed admin-credential attempts are
rate-limited per client address: 5 misses inside 15 minutes (a wrong bearer
token on any request, or a wrong token at login) lock that address out for 15
minutes, during which further attempts answer `429`. The counters are
in-memory, so a restart clears them. The key is the left-most `X-Forwarded-For`
entry (or `X-Real-Ip`, or a shared constant for direct clients), so put the
server behind a proxy that sets it or every direct client shares one bucket.

### Admin session

`POST /api/admin/session` with `{"token": "…"}` sets:

```
esplp_admin=<random>; Path=/api; HttpOnly; SameSite=Strict; Max-Age=43200
```

`HttpOnly` keeps script from reading it, so a stored-XSS bug cannot steal the
credential — which is why the admin UI gets a cookie rather than holding the
token in JS. The price is that cookies are *ambient*: a browser attaches them to
cross-site requests too. Two locks answer that:

  - `SameSite=Strict` — the cookie is not sent from another site at all;
  - every non-GET request that carries the cookie must also send an
    `x-esplp-csrf` header, or it gets `403`. A cross-site page cannot add one.

Bearer-token callers are exempt from the header rule: a browser never sets that
header by itself, so there is nothing to forge.

`GET /api/admin/session` answers `200`/`401` so the UI can tell whether it is
already logged in. `DELETE /api/admin/session` logs out, and is idempotent.
Sessions live in memory and expire after 12h, so restarting the server logs the
operator out. `Secure` is added to the cookie when `X-Forwarded-Proto: https` is
present; force it with `SESSION_COOKIE_SECURE=true` (or `=false` to suppress).

## API

`(admin)` marks operations that need the admin credential — the bearer token or
an admin session.

Everything below is routed. Swagger UI at `/swagger-ui` describes the public
surface only — the operations callable without the operator's credential — so the
rows marked `(admin)` and the `/api/admin/session` ones are documented at
`/swagger-ui-full` instead.

| Method | Path                                   | Purpose                       |
|--------|----------------------------------------|-------------------------------|
| POST   | `/api/prefixes`                        | create prefix (optional uuid) |
| PUT    | `/api/prefixes/{prefix}`               | create prefix at path uuid, idempotent |
| GET    | `/api/prefixes`                        | list prefixes, `?status=` filter **(admin)** |
| GET    | `/api/prefixes/{prefix}`               | get one prefix + card count   |
| PATCH  | `/api/prefixes/{prefix}`               | set the ribbon (while writable; `409` if a card's back cannot be printed on it) |
| DELETE | `/api/prefixes/{prefix}`               | delete prefix + all its cards and blobs (writable, or admin) |
| POST   | `/api/prefixes/{prefix}/submit`        | **removed** — answers `410`, use checkout |
| PATCH  | `/api/prefixes/{prefix}/status`        | set status, any transition **(admin)** |
| GET    | `/api/prefixes/{prefix}/cards`         | index of cards in the prefix  |
| POST   | `/api/prefixes/{prefix}/cards`         | add card, server-assigned id (optional `back`) |
| DELETE | `/api/prefixes/{prefix}/cards`         | delete every card in the prefix |
| GET    | `/api/prefixes/{prefix}/cards/{id}`    | card metadata                 |
| PUT    | `/api/prefixes/{prefix}/cards/{id}`    | create/replace card at id (image required) |
| PATCH  | `/api/prefixes/{prefix}/cards/{id}`    | update name / serial / back / back text only |
| DELETE | `/api/prefixes/{prefix}/cards/{id}`    | delete card + its blob        |
| POST   | `/api/prefixes/{prefix}/cards/{id}/printed` | mark a card ejected, optionally with its RFID UID (`serial_number`) **(admin)** |
| GET    | `/api/prefixes/{prefix}/print-queue`   | what to print, in card order, with `printed_at` **(admin)** |
| GET    | `/api/prefixes/{prefix}/cards/{id}/image/front` | card front image bytes     |
| GET    | `/api/prefixes/{prefix}/cards/{id}/image/back` | back image bytes (an `image` back only) |
| PUT    | `/api/prefixes/{prefix}/cards/{id}/image/back` | upload or replace the back image, making that image the card's back |
| GET    | `/api/prefixes/{prefix}/cards/{id}/display/front` | normalized front preview (PNG) |
| GET    | `/api/prefixes/{prefix}/cards/{id}/display/back` | back preview at the ribbon's depth (PNG) |
| GET    | `/api/prefixes/{prefix}/cards/{id}/print-raster/front` | front print raster (BMP, the printer's own bitmap) |
| GET    | `/api/prefixes/{prefix}/cards/{id}/print-raster/back` | back print raster (24-bit, or 1-bit on a black panel) |
| GET    | `/api/prefixes/{prefix}/shipping`      | where the order ships, as Stripe collected it **(admin)** |
| GET    | `/api/shop/config`                     | prices, currency, ribbon stock, shipping and countries |
| PUT    | `/api/shop/ribbons/{ribbon}`           | set a ribbon's stock flag, `ymcko` or `ymckok` **(admin)** |
| PUT    | `/api/shop/ribbons/{ribbon}/price`     | set a ribbon's price, in minor units, for its own unit **(admin)** |
| POST   | `/api/prefixes/{prefix}/checkout`      | open a Stripe Checkout session (draft or payment_pending only) |
| POST   | `/api/webhooks/stripe`                 | Stripe's payment callback (signature-authenticated) |
| POST   | `/api/admin/session`                   | log in: token → session cookie |
| GET    | `/api/admin/session`                   | am I logged in? **(admin)**   |
| DELETE | `/api/admin/session`                   | log out                       |

Card create/update (`POST`/`PUT`) takes `multipart/form-data` with fields
`name`, `serial_number`, `back`, `back_text` and a file part `image`, plus an
optional `back_image` file part. `back` is one of `none`, `front`, `image` or
`markdown`, and defaults to `none` (a single-sided card); sending a `back_image`
part implies `back=image`. `back_text` is the markdown a `markdown` back is
rendered from, and it is stored whatever the back is: a card whose back is not
markdown keeps it for the day it becomes one, so sending a text never selects a
back side. No ribbon restricts a back *source*, so `400` is for a form that
contradicts itself, never for a choice the ribbon dislikes. `serial_number` here
is a serial you assign yourself; the card's RFID UID is written by the print
process through `POST .../cards/{id}/printed`.

`back_image` is optional even for an `image` back, because cards are created by
the front-image dropzone, one file at a time. A card without one is simply not
complete: its own row offers the upload (`PUT .../cards/{id}/image/back`, which
leaves the front alone and makes the uploaded image the card's back), and an
order containing an incomplete card is refused at the checkout with `409` rather
than sold. `requires a back_image on every card` at creation time would have
made double-sided cards impossible to upload into at all.

`PUT` is full-replace and **requires** the image; use `PATCH` to edit
name/serial/back without re-uploading it. Re-uploading an image in a different
format replaces the old blob (no orphan files). Deleting a card (or clearing a
prefix) removes the corresponding blobs. Switching a card's `back` deliberately
leaves the other material in place — turning a card from `image` to `markdown`
and back again does not lose the picture or the text.

## Smoke test

```sh
nix shell nixpkgs#python3 -c python3 scripts/make_png.py /tmp/card.png
export ESPLP_SERVER=http://127.0.0.1:8080
P=$(./target/debug/card-printer-cli prefix create --quiet)
./target/debug/card-printer-cli upload "$P" /tmp/card.png --name Conni --serial-number SN-001
./target/debug/card-printer-cli card list "$P"
```